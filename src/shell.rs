use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::num::*;
use std::mem;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use futures::{FutureExt, executor::block_on};

use tokio::sync::{
    Mutex as AsyncMutex,
    Notify,
};

use clap::{self, value_t};

use cairo;
use gdk;
use gdk::{EventButton, EventMotion, EventScroll, EventType, ModifierType};
use glib;
use gtk;
use gtk::{Button, MenuButton, Notebook};
use gtk::prelude::*;
use pango;
use pango::FontDescription;
use pangocairo;

use nvim_rs::Value;

use crate::color::{Color, COLOR_BLACK, COLOR_WHITE};
use crate::grid::GridMap;
use crate::highlight::{HighlightMap, BackgroundState};
use crate::misc::{decode_uri, escape_filename, split_at_comma};
use crate::nvim::{
    self, CompleteItem, ErrorReport, NeovimClient, NvimHandler, RepaintMode, NvimSession, Tabpage,
    NormalError, CallErrorExt
};
use crate::settings::{FontSource, Settings};
use crate::ui_model::ModelRect;
use crate::{spawn_timeout, spawn_timeout_user_err};

use crate::cmd_line::{CmdLine, CmdLineContext};
use crate::cursor::{BlinkCursor, Cursor, CursorRedrawCb};
use crate::error;
use crate::input;
use crate::input::keyval_to_input_string;
use crate::mode;
use crate::popup_menu::{self, PopupMenu};
use crate::render;
use crate::render::CellMetrics;
use crate::subscriptions::{SubscriptionHandle, SubscriptionKey, Subscriptions};
use crate::tabline::Tabline;
use crate::ui::UiMutex;

const DEFAULT_FONT_NAME: &str = "DejaVu Sans Mono 12";
pub const MINIMUM_SUPPORTED_NVIM_VERSION: &str = "0.3.2";

macro_rules! idle_cb_call {
    ($state:ident.$cb:ident($( $x:expr ),*)) => (
        glib::idle_add_once(move || {
            if let Some(ref cb) = $state.borrow().$cb {
                (&mut *cb.borrow_mut())($($x),*);
            }
        });
    )
}

pub struct RenderState {
    pub font_ctx: render::Context,
    pub hl: HighlightMap,
    pub mode: mode::Mode,
}

impl RenderState {
    pub fn new(pango_context: pango::Context) -> Self {
        RenderState {
            font_ctx: render::Context::new(pango_context),
            hl: HighlightMap::new(),
            mode: mode::Mode::new(),
        }
    }
}

pub struct TransparencySettings {
    background_alpha: f64,
    filled_alpha: f64,
    enabled: bool,
}

impl TransparencySettings {
    pub fn new() -> Self {
        TransparencySettings {
            background_alpha: 1.0,
            filled_alpha: 1.0,
            enabled: false,
        }
    }

    fn filled_alpha(&self) -> Option<f64> {
        if self.enabled {
            Some(self.filled_alpha)
        } else {
            None
        }
    }

    fn background_alpha(&self) -> Option<f64> {
        if self.enabled {
            Some(self.background_alpha)
        } else {
            None
        }
    }
}

/// Contains state related to resize requests we are going to/have sent to nvim
pub struct ResizeRequests {
    /// The most recently submitted resize request, if any. This might not have been received by
    /// neovim yet.
    pub current: Option<(NonZeroI64, NonZeroI64)>,
    /// The next resize request to submit to neovim, if any.
    requested: Option<(NonZeroI64, NonZeroI64)>,
    /// Whether there's a resize future active or not
    active: bool,
}

pub struct ResizeState {
    /// The current state of neovim's resize requests
    pub requests: AsyncMutex<ResizeRequests>,
    /// Signal when we've finished a resize request
    autocmd_status: Notify,
}

impl ResizeState {
    pub fn notify_finished(&self) {
        self.autocmd_status.notify_one();
    }
}

/// A collection of all header bar buttons used in nvim-gtk
pub struct HeaderBarButtons {
    open_btn: Button,
    new_tab_btn: Button,
    paste_btn: Button,
    save_btn: Button,
    primary_menu_btn: MenuButton,
}

impl HeaderBarButtons {
    pub fn new(
        open_btn: Button,
        new_tab_btn: Button,
        paste_btn: Button,
        save_btn: Button,
        primary_menu_btn: MenuButton,
    ) -> Self {
        Self {
            open_btn,
            new_tab_btn,
            paste_btn,
            primary_menu_btn,
            save_btn,
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.new_tab_btn.set_sensitive(enabled);
        self.paste_btn.set_sensitive(enabled);
        self.save_btn.set_sensitive(enabled);
        self.primary_menu_btn.set_sensitive(enabled);

        // Use an idle callback for open_btn, as we might be calling this from one of its own
        // callbacks which would result in borrowing it mutably twice
        let open_btn = self.open_btn.clone();
        glib::idle_add_local_once(move || open_btn.set_sensitive(enabled));
    }
}

/// A struct containing all of the widgets in neovim-gtk that interact with Neovim in some way using
/// RPC calls. They are grouped together so that they may be easily enabled/disabled when nvim is
/// blocked/unblocked.
pub struct ActionWidgets {
    header_bar: Option<Box<HeaderBarButtons>>,
    tabs: Notebook,
    file_browser: gtk::Box,
}

impl ActionWidgets {
    /// Enable or disable all widgets
    pub fn set_enabled(&self, enabled: bool) {
        if let Some(ref header_bar) = self.header_bar {
            header_bar.set_enabled(enabled);
        }
        self.tabs.set_sensitive(enabled);
        self.file_browser.set_sensitive(enabled);
    }
}

pub struct State {
    pub grids: GridMap,

    mouse_enabled: bool,
    nvim: Rc<NeovimClient>,
    cursor: Option<BlinkCursor<State>>,
    popup_menu: PopupMenu,
    cmd_line: CmdLine,
    settings: Rc<RefCell<Settings>>,
    render_state: Rc<RefCell<RenderState>>,

    resize_status: Arc<ResizeState>,
    focus_state: Arc<AsyncMutex<FocusState>>,

    pub clipboard_clipboard: gtk::Clipboard,
    pub clipboard_primary: gtk::Clipboard,

    stack: gtk::Stack,
    pub drawing_area: gtk::DrawingArea,
    tabs: Tabline,
    im_context: gtk::IMMulticontext,
    error_area: error::ErrorArea,

    pub options: RefCell<ShellOptions>,
    transparency_settings: TransparencySettings,

    detach_cb: Option<Box<RefCell<dyn FnMut(i32) + Send + 'static>>>,
    nvim_started_cb: Option<Box<RefCell<dyn FnMut() + Send + 'static>>>,
    command_cb: Option<Box<dyn FnMut(&mut State, nvim::NvimCommand) + Send + 'static>>,

    subscriptions: RefCell<Subscriptions>,

    action_widgets: Arc<UiMutex<Option<ActionWidgets>>>,

    exit_status: Arc<Mutex<Option<i32>>>,
}

impl State {
    pub fn new(settings: Rc<RefCell<Settings>>, options: ShellOptions)
        -> State {
        let drawing_area = gtk::DrawingArea::new();

        let pango_context = drawing_area.create_pango_context();
        pango_context.set_font_description(&FontDescription::from_string(DEFAULT_FONT_NAME));

        let mut render_state = RenderState::new(pango_context);
        render_state.hl.set_use_cterm(options.cterm_colors);
        let render_state = Rc::new(RefCell::new(render_state));

        let popup_menu = PopupMenu::new(&drawing_area);
        let cmd_line = CmdLine::new(&drawing_area, render_state.clone());

        State {
            grids: GridMap::new(),
            nvim: Rc::new(NeovimClient::new()),
            mouse_enabled: true,
            cursor: None,
            popup_menu,
            cmd_line,
            settings,
            render_state,

            resize_status: Arc::new(ResizeState {
                requests: AsyncMutex::new(ResizeRequests {
                    current: None,
                    requested: None,
                    active: false,
                }),
                autocmd_status: Notify::new(),
            }),
            focus_state: Arc::new(AsyncMutex::new(FocusState {
                last: true,
                next: true,
                is_pending: false
            })),

            clipboard_clipboard: gtk::Clipboard::get(&gdk::Atom::intern("CLIPBOARD")),
            clipboard_primary: gtk::Clipboard::get(&gdk::Atom::intern("PRIMARY")),

            // UI
            stack: gtk::Stack::new(),
            drawing_area,
            tabs: Tabline::new(),
            im_context: gtk::IMMulticontext::new(),
            error_area: error::ErrorArea::new(),

            options: RefCell::new(options),
            transparency_settings: TransparencySettings::new(),

            detach_cb: None,
            nvim_started_cb: None,
            command_cb: None,

            subscriptions: RefCell::new(Subscriptions::new()),

            action_widgets: Arc::new(UiMutex::new(None)),

            exit_status: Arc::new(Mutex::new(None)),
        }
    }

    pub fn nvim(&self) -> Option<NvimSession> {
        self.nvim.nvim()
    }

    pub fn nvim_clone(&self) -> Rc<NeovimClient> {
        self.nvim.clone()
    }

    pub fn set_action_widgets(
        &self,
        header_bar: Option<Box<HeaderBarButtons>>,
        file_browser: gtk::Box
    ) {
        self.action_widgets.replace(Some(ActionWidgets {
            header_bar,
            tabs: self.tabs.clone(),
            file_browser,
        }));
    }

    pub fn action_widgets(&self) -> Arc<UiMutex<Option<ActionWidgets>>> {
        self.action_widgets.clone()
    }

    pub fn start_nvim_initialization(&self) -> bool {
        if self.nvim.is_uninitialized() {
            self.nvim.set_in_progress();
            true
        } else {
            false
        }
    }

    pub fn set_detach_cb<F>(&mut self, cb: Option<F>)
    where
        F: FnMut(i32) + Send + 'static,
    {
        if let Some(c) = cb {
            self.detach_cb = Some(Box::new(RefCell::new(c)));
        } else {
            self.detach_cb = None;
        }
    }

    pub fn set_nvim_started_cb<F>(&mut self, cb: Option<F>)
    where
        F: FnMut() + Send + 'static,
    {
        if let Some(c) = cb {
            self.nvim_started_cb = Some(Box::new(RefCell::new(c)));
        } else {
            self.nvim_started_cb = None;
        }
    }

    pub fn set_nvim_command_cb<F>(&mut self, cb: Option<F>)
    where
        F: FnMut(&mut State, nvim::NvimCommand) + Send + 'static,
    {
        if let Some(c) = cb {
            self.command_cb = Some(Box::new(c));
        } else {
            self.command_cb = None;
        }
    }

    pub fn set_font_desc(&mut self, desc: &str) {
        let font_description = FontDescription::from_string(desc);

        if font_description.size() <= 0 {
            error!("Font size must be > 0");
            return;
        }

        let pango_context = self.drawing_area.create_pango_context();
        pango_context.set_font_description(&font_description);

        self.render_state
            .borrow_mut()
            .font_ctx
            .update(pango_context);
        self.grids.clear_glyphs();
        self.try_nvim_resize();
        self.on_redraw(&RepaintMode::All);
    }

    pub fn set_font_features(&mut self, font_features: String) {
        let font_features = render::FontFeatures::from(font_features);

        self.render_state
            .borrow_mut()
            .font_ctx
            .update_font_features(font_features);
        self.grids.clear_glyphs();
        self.on_redraw(&RepaintMode::All);
    }

    pub fn set_line_space(&mut self, line_space: String) {
        let line_space: i32 = match line_space.parse() {
            Ok(line_space) => line_space,
            Err(e) => {
                error!("Can't convert argument to integer: {}", e.to_string());
                return;
            }
        };

        self.render_state
            .borrow_mut()
            .font_ctx
            .update_line_space(line_space);
        self.grids.clear_glyphs();
        self.try_nvim_resize();
        self.on_redraw(&RepaintMode::All);
    }

    /// return true if transparency enabled
    pub fn set_transparency(&mut self, background_alpha: f64, filled_alpha: f64) -> bool {
        if background_alpha < 1.0 || filled_alpha < 1.0 {
            self.transparency_settings.background_alpha = background_alpha;
            self.transparency_settings.filled_alpha = filled_alpha;
            self.transparency_settings.enabled = true;
        } else {
            self.transparency_settings.background_alpha = 1.0;
            self.transparency_settings.filled_alpha = 1.0;
            self.transparency_settings.enabled = false;
        }

        self.on_redraw(&RepaintMode::All);

        self.transparency_settings.enabled
    }

    pub fn set_cursor_blink(&mut self, val: i32) {
        if let Some(cursor) = &mut self.cursor {
            cursor.set_cursor_blink(val);
        }
    }

    pub fn set_exit_status(&self, val: i32) {
        let mut status = self.exit_status.lock().unwrap();
        *status = Some(val);
    }

    pub fn open_file(&self, path: &str) {
        if let Some(nvim) = self.nvim() {
            let action_widgets = self.action_widgets();
            let path = format!("e {}", path).to_owned();

            action_widgets.borrow().as_ref().unwrap().set_enabled(false);

            nvim.clone().spawn(async move {
                let res = nvim.command(&path).await;

                glib::idle_add_once(move || {
                    action_widgets.borrow().as_ref().unwrap().set_enabled(true);
                });

                if let Err(e) = res {
                    if let Ok(e) = NormalError::try_from(&*e) {
                        // Filter out errors we get if the user is presented with a prompt
                        if !e.has_code(325) {
                            e.print(&nvim).await;
                        }
                        return;
                    }
                    e.print();
                }
            });
        }
    }

    pub fn cd(&self, path: &str) {
        if let Some(nvim) = self.nvim() {
            let path = format!("cd {}", path);
            spawn_timeout!(nvim.command(&path));
        }
    }

    pub fn clipboard_clipboard_set(&self, text: &str) {
        self.clipboard_clipboard.set_text(text);
    }

    pub fn clipboard_primary_set(&self, text: &str) {
        self.clipboard_primary.set_text(text);
    }

    fn close_popup_menu(&self) {
        if self.popup_menu.is_open() {
            if let Some(nvim) = self.nvim() {
                nvim.block_timeout(nvim.input("<Esc>")).report_err();
            }
        }
    }

    fn queue_draw_area<M: AsRef<ModelRect>>(&mut self, rect_list: &[M]) {
        // extends by items before, then after changes

        let rects: Vec<_> = rect_list
            .iter()
            .map(|rect| rect.as_ref().clone())
            .map(|mut rect| {
                rect.extend_by_items(self.grids.current_model());
                rect
            })
            .collect();

        self.update_dirty_glyphs();

        let render_state = self.render_state.borrow();
        let cell_metrics = render_state.font_ctx.cell_metrics();

        for mut rect in rects {
            rect.extend_by_items(self.grids.current_model());

            let (x, y, width, height) =
                rect.to_area_extend_ink(self.grids.current_model(), cell_metrics);
            self.drawing_area.queue_draw_area(x, y, width, height);
        }
    }

    fn update_dirty_glyphs(&mut self) {
        let render_state = self.render_state.borrow();
        if let Some(model) = self.grids.current_model_mut() {
            render::shape_dirty(&render_state.font_ctx, model, &render_state.hl);
        }
    }

    fn im_commit(&self, ch: &str) {
        if let Some(mut nvim) = self.nvim() {
            input::im_input(&mut nvim, ch);
        }
    }

    pub fn should_resize(&self, _columns: usize, _rows: usize) -> bool {
        true
    }

    pub fn calc_window_size(&self, columns: usize, rows: usize) -> (usize, usize) {
        let &CellMetrics {
            line_height,
            char_width,
            ..
        } = self.render_state.borrow().font_ctx.cell_metrics();

        (
            (columns as f64 * char_width).ceil() as usize,
            (rows as f64 * line_height).ceil() as usize,
        )
    }

    fn calc_nvim_size(&self) -> (NonZeroI64, NonZeroI64) {
        let &CellMetrics {
            line_height,
            char_width,
            ..
        } = self.render_state.borrow().font_ctx.cell_metrics();
        let alloc = self.drawing_area.allocation();
        // SAFETY: We clamp w to 1 and h to 3
        unsafe {(
            NonZeroI64::new_unchecked(((alloc.width as f64 / char_width).trunc() as i64).max(1)),
            /* Neovim won't resize to below 3 rows, and trying to do this will potentially cause
             * nvim to avoid sending back an autocmd when the resize is processed. So, limit us to
             * at least 3 rows at all times.
             */
            NonZeroI64::new_unchecked(((alloc.height as f64 / line_height).trunc() as i64).max(3)),
        )}
    }

    fn show_error_area(&self) {
        let stack = self.stack.clone();
        glib::idle_add_local_once(move || stack.set_visible_child_name("Error"));
    }

    fn set_im_location(&self) {
        if let Some((row, col)) = self.grids.current().map(|g| g.get_cursor()) {
            let (x, y, width, height) = ModelRect::point(col, row)
                .to_area(self.render_state.borrow().font_ctx.cell_metrics());

            self.im_context.set_cursor_location(&gdk::Rectangle {
                x,
                y,
                width,
                height,
            });

            self.im_context.reset();
        }
    }

    pub fn resize_status(&self) -> Arc<ResizeState> {
        self.resize_status.clone()
    }

    fn try_nvim_resize(&mut self) {
        let nvim = match self.nvim() {
            Some(nvim) => nvim,
            None => return,
        };

        {
            let mut status = nvim.block_on(self.resize_status.requests.lock());

            // Abort if the UI isn't attached yet
            if status.current.is_none() {
                return;
            }

            let our_req = self.calc_nvim_size();
            if status.requested == Some(our_req) {
                return;
            } else if status.current == Some(our_req) {
                if status.requested.is_some() {
                    debug!("Resize request matches last committed size, cancelling reqs");
                }
                status.requested = None;
                return;
            }

            debug!("Requesting resize to {:?}", our_req);
            status.requested.replace(our_req);

            // Don't spawn a resize future if one's already active
            if status.active {
                debug!("Request already pending, not starting new one");
                return;
            }
            status.active = true;
        }

        let status_ref = self.resize_status.clone();
        nvim.clone().spawn(async move {
            loop {
                let (cols, rows) = {
                    let mut status = status_ref.requests.lock().await;
                    let req = status.requested.take();

                    if let Some((cols, rows)) = req {
                        status.current = req;
                        (cols, rows)
                    } else {
                        status.active = false;
                        debug!("No new resize requests, finishing");
                        return;
                    }
                };

                debug!("Committing new size {}x{}...", cols, rows);

                /* We don't use subscriptions for this since we want to ensure that there's
                 * no potential for RPC requests between autocmd registration and our resize
                 * request
                 */
                nvim.call_atomic(vec![
                    Value::Array(vec![
                        "nvim_command".into(),
                        Value::Array(vec![
                            "au VimResized * ++once cal rpcnotify(1, 'resized')".into()
                        ]),
                    ]),
                    Value::Array(vec![
                        "nvim_ui_try_resize".into(),
                        Value::Array(vec![cols.get().into(), rows.get().into()]),
                    ]),
                ]).await.report_err();

                // Wait for the resize request to finish, and then update the request state
                status_ref.autocmd_status.notified().await;
            };
        });
    }

    fn edit_paste(&self, clipboard: &'static str) {
        let nvim = self.nvim();
        if let Some(nvim) = nvim {
            let render_state = self.render_state.borrow();
            if render_state.mode.is(&mode::NvimMode::Insert)
                || render_state.mode.is(&mode::NvimMode::Normal)
            {
                spawn_timeout_user_err!(nvim.command(&format!("normal! \"{}P", clipboard)));
            } else {
                spawn_timeout_user_err!(nvim.input(&format!("<C-r>{}", clipboard)));
            };
        }
    }

    fn edit_copy(&self, clipboard: &'static str) {
        if let Some(nvim) = self.nvim() {
            spawn_timeout_user_err!(nvim.command(&format!("normal! \"{}y", clipboard)));
        }
    }

    fn max_popup_width(&self) -> i32 {
        self.drawing_area.allocated_width() - 20
    }

    pub fn subscribe<F>(&self, key: SubscriptionKey, args: &[&str], cb: F) -> SubscriptionHandle
    where
        F: Fn(Vec<String>) + 'static,
    {
        self.subscriptions.borrow_mut().subscribe(key, args, cb)
    }

    pub fn set_autocmds(&self) {
        self.subscriptions
            .borrow()
            .set_autocmds(&self.nvim().unwrap());
    }

    pub fn notify(&self, params: Vec<Value>) -> Result<(), String> {
        self.subscriptions.borrow().notify(params)
    }

    pub fn run_now(&self, handle: &SubscriptionHandle) {
        self.subscriptions
            .borrow()
            .run_now(handle, &mut self.nvim().unwrap());
    }

    pub fn set_font(&mut self, font_desc: String) {
        self.set_font_rpc(&font_desc);
    }

    pub fn set_font_rpc(&mut self, font_desc: &str) {
        {
            let mut settings = self.settings.borrow_mut();
            settings.set_font_source(FontSource::Rpc);
        }

        self.set_font_desc(font_desc);
    }

    pub fn on_command(&mut self, command: nvim::NvimCommand) {
        let mut cb = self.command_cb.take();

        if let Some(ref mut cb) = cb {
            cb(self, command);
        }

        self.command_cb = cb;
    }

    pub fn focus_update(&self, state: bool) {
        let nvim = {
            let mut focus_state = block_on(self.focus_state.lock());
            if focus_state.next == state {
                return;
            }
            focus_state.next = state;

            if focus_state.is_pending {
                // A future is still running, no need for another
                return;
            } else if let Some(nvim) = self.nvim() {
                focus_state.is_pending = true;
                nvim
            } else {
                return;
            }
        };

        let focus_state = self.focus_state.clone();
        nvim.clone().spawn(async move {
            loop {
                let next = {
                    let mut focus_state = focus_state.lock().await;
                    if focus_state.next == focus_state.last {
                        focus_state.is_pending = false;
                        return;
                    }

                    focus_state.last = focus_state.next;
                    focus_state.next
                };
                let autocmd = if next == true { "FocusGained" } else { "FocusLost" };

                debug!("Triggering {} autocmd", autocmd);
                nvim.command(&format!(
                    "if exists('#{a}')|doau {a}|endif", a = autocmd
                )).await.report_err();
            }
        });
    }

    pub fn set_tabline(&self, visible: bool) {
        self.tabs.set_visible(visible)
    }

    pub fn set_background(&self, background: BackgroundState) {
        self.render_state.borrow_mut().hl.set_background_state(background)
    }
}

#[derive(PartialEq)]
enum MouseCursor {
    None,
    Text,
    Default,
}

pub struct UiState {
    mouse_pressed: bool,
    scroll_delta: (f64, f64),

    // previous editor position (col, row)
    prev_pos: (u64, u64),

    mouse_cursor: MouseCursor,
}

impl UiState {
    pub fn new() -> UiState {
        UiState {
            mouse_pressed: false,
            scroll_delta: (0.0, 0.0),
            prev_pos: (0, 0),

            mouse_cursor: MouseCursor::None,
        }
    }

    fn apply_mouse_cursor(&mut self, cursor: MouseCursor, window: Option<gdk::Window>) {
        if self.mouse_cursor == cursor {
            return;
        }

        self.mouse_cursor = cursor;

        if let Some(window) = window {
            let cursor = match self.mouse_cursor {
                MouseCursor::Default => "default",
                MouseCursor::None => "none",
                MouseCursor::Text => "text",
            };

            window.set_cursor(gdk::Cursor::from_name(&window.display(), cursor).as_ref());
        }
    }
}

/// The mode to start the neovim instance in, this is the equivalent of nvim's TUI client's -b, -d,
/// -e, etc. options
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    Normal,
    Diff
}

#[derive(Clone)]
pub struct ShellOptions {
    nvim_bin_path: Option<String>,
    timeout: Option<Duration>,
    args_for_neovim: Vec<String>,
    input_data: Option<String>,
    cterm_colors: bool,
    pub mode: StartMode,
    post_config_cmds: Box<[String]>,
}

impl ShellOptions {
    pub fn new(matches: &clap::ArgMatches, input_data: Option<String>) -> Self {
        ShellOptions {
            input_data,
            cterm_colors: matches.is_present("cterm-colors"),
            mode:
                if matches.is_present("diff-mode") {
                    StartMode::Diff
                } else {
                    StartMode::Normal
                },
            nvim_bin_path: matches.value_of("nvim-bin-path").map(str::to_owned),
            timeout: value_t!(matches.value_of("timeout"), u64)
                .map(Duration::from_secs)
                .ok(),
            args_for_neovim: matches
                .values_of("nvim-args")
                .map(|args| args.map(str::to_owned).collect())
                .unwrap_or_else(|| vec![]),
            post_config_cmds: matches
                .values_of("post-config-cmds")
                .map(|args| args.map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }

    /// Remove input data from original shell option, as it need to be used only once
    pub fn input_data(&mut self) -> Self {
        let input_data = self.input_data.take();
        let mut clone = self.clone();
        clone.input_data = input_data;

        clone
    }

    /// Steal the post config commands, since they're only needed once
    pub fn post_config_cmds(&mut self) -> Box<[String]> {
        mem::take(&mut self.post_config_cmds)
    }
}

pub struct Shell {
    pub state: Arc<UiMutex<State>>,
    ui_state: Rc<RefCell<UiState>>,

    widget: gtk::Box,
}

impl Shell {
    pub fn new(settings: Rc<RefCell<Settings>>, options: ShellOptions) -> Shell {
        let shell = Shell {
            state: Arc::new(UiMutex::new(State::new(settings, options))),
            ui_state: Rc::new(RefCell::new(UiState::new())),

            widget: gtk::Box::new(gtk::Orientation::Vertical, 0),
        };

        let shell_ref = Arc::downgrade(&shell.state);
        shell.state.borrow_mut().cursor = Some(BlinkCursor::new(shell_ref));

        shell
    }

    pub fn is_nvim_initialized(&self) -> bool {
        let state = self.state.borrow();
        state.nvim.is_initialized()
    }

    pub fn init(&mut self) {
        let state = self.state.borrow();

        state.drawing_area.set_hexpand(true);
        state.drawing_area.set_vexpand(true);
        state.drawing_area.set_can_focus(true);

        state.im_context.set_use_preedit(false);

        let nvim_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

        nvim_box.pack_start(&*state.tabs, false, true, 0);
        nvim_box.pack_start(&state.drawing_area, true, true, 0);

        state.stack.add_named(&nvim_box, "Nvim");
        state.stack.add_named(&*state.error_area, "Error");

        self.widget.pack_start(&state.stack, true, true, 0);

        state.drawing_area.add_events(
            gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_MOTION_MASK
                | gdk::EventMask::SCROLL_MASK
                | gdk::EventMask::SMOOTH_SCROLL_MASK
                | gdk::EventMask::ENTER_NOTIFY_MASK
                | gdk::EventMask::LEAVE_NOTIFY_MASK
                | gdk::EventMask::POINTER_MOTION_MASK,
        );

        let menu = self.create_context_menu(&state);
        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state.drawing_area.connect_button_press_event(move |_, ev| {
            gtk_button_press(&mut *ref_state.borrow_mut(), &ref_ui_state, ev, &menu)
        });

        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state
            .drawing_area
            .connect_button_release_event(move |_, ev| {
                gtk_button_release(
                    &mut *ref_state.borrow_mut(),
                    &mut *ref_ui_state.borrow_mut(),
                    ev,
                )
            });

        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state
            .drawing_area
            .connect_motion_notify_event(move |_, ev| {
                gtk_motion_notify(
                    &mut *ref_state.borrow_mut(),
                    &mut *ref_ui_state.borrow_mut(),
                    ev,
                )
            });

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_draw(move |_, ctx| gtk_draw(&ref_state, ctx));

        let ref_ui_state = self.ui_state.clone();
        let ref_state = self.state.clone();
        state.drawing_area.connect_key_press_event(move |_, ev| {
            ref_state
                .borrow_mut()
                .cursor
                .as_mut()
                .unwrap()
                .reset_state();

            if ref_state.borrow().im_context.filter_keypress(ev) {
                Inhibit(true)
            } else {
                let state = ref_state.borrow();
                let nvim = state.nvim();
                if let Some(mut nvim) = nvim {
                    input::gtk_key_press(&mut nvim, ev)
                } else {
                    Inhibit(false)
                }
            }
        });
        let ref_state = self.state.clone();
        state.drawing_area.connect_key_release_event(move |da, ev| {
            ref_state.borrow().im_context.filter_keypress(ev);
            ref_ui_state
                .borrow_mut()
                .apply_mouse_cursor(MouseCursor::None, da.window());
            Inhibit(false)
        });

        let ref_state = self.state.clone();
        let ref_ui_state = self.ui_state.clone();
        state.drawing_area.connect_scroll_event(move |_, ev| {
            gtk_scroll_event(
                &mut *ref_state.borrow_mut(),
                &mut *ref_ui_state.borrow_mut(),
                ev,
            )
        });

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_focus_in_event(move |_, _| gtk_focus_in(&mut *ref_state.borrow_mut()));

        let ref_state = self.state.clone();
        state
            .drawing_area
            .connect_focus_out_event(move |_, _| gtk_focus_out(&mut *ref_state.borrow_mut()));

        let ref_state = self.state.clone();
        state.drawing_area.connect_realize(move |w| {
            // sometime set_client_window does not work without idle_add
            // and looks like not enabled im_context
            glib::idle_add_local_once(clone!(ref_state, w => move || {
                ref_state.borrow().im_context.set_client_window(
                    w.window().as_ref(),
                );
            }));
        });

        let ref_state = self.state.clone();
        state
            .im_context
            .connect_commit(move |_, ch| ref_state.borrow().im_commit(ch));

        let ref_state = self.state.clone();
        state.drawing_area.connect_configure_event(move |_, ev| {
            debug!("configure_event {:?}", ev.size());

            let mut state = ref_state.borrow_mut();
            state.try_nvim_resize();

            false
        });

        let ref_state = self.state.clone();
        state.drawing_area.connect_size_allocate(move |_, _| {
            init_nvim(&ref_state);
        });

        let ref_state = self.state.clone();
        let targets = vec![gtk::TargetEntry::new(
            "text/uri-list",
            gtk::TargetFlags::OTHER_APP,
            0,
        )];
        state
            .drawing_area
            .drag_dest_set(gtk::DestDefaults::ALL, &targets, gdk::DragAction::COPY);
        state
            .drawing_area
            .connect_drag_data_received(move |_, _, _, _, s, _, _| {
                let uris = s.uris();
                let command = uris.iter().filter_map(|uri| decode_uri(uri)).fold(
                    ":ar".to_owned(),
                    |command, filename| {
                        let filename = escape_filename(&filename);
                        command + " " + &filename
                    },
                );

                let state = ref_state.borrow_mut();
                let nvim = state.nvim().unwrap();
                let action_widgets = state.action_widgets();

                action_widgets.borrow().as_ref().unwrap().set_enabled(false);

                nvim.clone().spawn(async move {
                    let res = nvim.command(&command).await;

                    glib::idle_add_once(move || {
                        action_widgets.borrow().as_ref().unwrap().set_enabled(true);
                    });

                    if let Err(e) = res {
                        if let Ok(e) = NormalError::try_from(&*e) {
                            // Filter out errors we get if the user is presented with a prompt
                            if !e.has_code(325) {
                                e.print(&nvim).await;
                            }
                            return;
                        }
                        e.print();
                    }
                });
            });

        let ui_state_ref = self.ui_state.clone();
        state.drawing_area.connect_enter_notify_event(move |_, ev| {
            ui_state_ref
                .borrow_mut()
                .apply_mouse_cursor(MouseCursor::Text, ev.window());
            gtk::Inhibit(false)
        });

        let ui_state_ref = self.ui_state.clone();
        state.drawing_area.connect_leave_notify_event(move |_, ev| {
            ui_state_ref
                .borrow_mut()
                .apply_mouse_cursor(MouseCursor::Default, ev.window());
            gtk::Inhibit(false)
        });
    }

    fn create_context_menu(&self, state: &State) -> gtk::PopoverMenu {
        let state_ref = &self.state;

        let action_group = gio::SimpleActionGroup::new();

        let copy = gio::SimpleAction::new("copy", None);
        copy.connect_activate(clone!(state_ref => move |_, _| state_ref.borrow().edit_copy("+")));
        action_group.add_action(&copy);

        let paste = gio::SimpleAction::new("paste", None);
        paste.connect_activate(clone!(state_ref => move |_, _| state_ref.borrow().edit_paste("+")));
        action_group.add_action(&paste);

        let menu = gio::Menu::new();
        let section = gio::Menu::new();
        section.append(Some("Copy"), Some("copy"));
        section.append(Some("Paste"), Some("paste"));
        menu.append_section(None, &section);

        let popover = gtk::PopoverMenuBuilder::new()
            .position(gtk::PositionType::Bottom)
            .relative_to(&state.drawing_area)
            .build();
        popover.insert_action_group("menu", Some(&action_group));
        popover.bind_model(Some(&menu), Some("menu"));

        popover
    }

    #[cfg(unix)]
    pub fn set_font_desc(&self, font_name: &str) {
        self.state.borrow_mut().set_font_desc(font_name);
    }

    pub fn grab_focus(&self) {
        self.state.borrow().drawing_area.grab_focus();
    }

    pub fn open_file(&self, path: &str) {
        self.state.borrow().open_file(path);
    }

    pub fn cd(&self, path: &str) {
        self.state.borrow().cd(path);
    }

    pub fn detach_ui(&mut self) {
        let state = self.state.borrow();
        let nvim_client = state.nvim.clone();

        if let Some(nvim) = nvim_client.nvim() {
            nvim_client.clear();
            nvim.block_timeout(nvim.ui_detach()).report_err();
            nvim.block_on(nvim.shutdown());
        }
    }

    pub fn edit_paste(&self) {
        self.state.borrow().edit_paste("+");
    }

    pub fn edit_save_all(&self) {
        if let Some(nvim) = self.state.borrow().nvim() {
            spawn_timeout_user_err!(nvim.command(":wa"));
        }
    }

    pub fn new_tab(&self) {
        if let Some(nvim) = self.state.borrow().nvim() {
            spawn_timeout!(nvim.command(":tabe"));
        }
    }

    pub fn set_detach_cb<F>(&self, cb: Option<F>)
    where
        F: FnMut(i32) + Send + 'static,
    {
        let mut state = self.state.borrow_mut();
        state.set_detach_cb(cb);
    }

    pub fn set_nvim_started_cb<F>(&self, cb: Option<F>)
    where
        F: FnMut() + Send + 'static,
    {
        let mut state = self.state.borrow_mut();
        state.set_nvim_started_cb(cb);
    }

    pub fn set_nvim_command_cb<F>(&self, cb: Option<F>)
    where
        F: FnMut(&mut State, nvim::NvimCommand) + Send + 'static,
    {
        let mut state = self.state.borrow_mut();
        state.set_nvim_command_cb(cb);
    }

    pub fn set_completeopts(&self, options: &str) {
        self.state
            .borrow()
            .popup_menu
            .set_preview(options.contains("preview"));
    }

    pub fn set_exit_status(&self, status: i32) {
        let state = self.state.borrow();
        state.set_exit_status(status);
    }
}

impl Deref for Shell {
    type Target = gtk::Box;

    fn deref(&self) -> &gtk::Box {
        &self.widget
    }
}

/// Keeps track of focus/unfocus requests for neovim.
struct FocusState {
    /// The last focus state we sent to neovim, which may or may not have been received yet.
    last: bool,
    /// The next focus state to send to neovim, if any. If there's no new state to send, this is
    /// equal to `last`.
    next: bool,
    /// Whether there's a focus/unfocus request being sent to neovim.
    is_pending: bool,
}

fn gtk_focus_in(state: &mut State) -> Inhibit {
    state.focus_update(true);
    state.im_context.focus_in();
    state.cursor.as_mut().unwrap().enter_focus();
    state.queue_redraw_cursor();

    Inhibit(false)
}

fn gtk_focus_out(state: &mut State) -> Inhibit {
    state.focus_update(false);
    state.im_context.focus_out();
    state.cursor.as_mut().unwrap().leave_focus();
    state.queue_redraw_cursor();

    Inhibit(false)
}

fn gtk_scroll_event(state: &mut State, ui_state: &mut UiState, ev: &EventScroll) -> Inhibit {
    if !state.mouse_enabled && !state.nvim.is_initializing() {
        return Inhibit(false);
    }

    state.close_popup_menu();

    match ev.direction() {
        gdk::ScrollDirection::Right => {
            mouse_input(state, "wheel", "right", ev.state(), ev.position())
        }
        gdk::ScrollDirection::Left => {
            mouse_input(state, "wheel", "left", ev.state(), ev.position())
        }
        gdk::ScrollDirection::Up => {
            mouse_input(state, "wheel", "up", ev.state(), ev.position())
        }
        gdk::ScrollDirection::Down => {
            mouse_input(state, "wheel", "down", ev.state(), ev.position())
        }
        gdk::ScrollDirection::Smooth => {
            // Remember and accumulate scroll deltas, so slow scrolling still
            // works.
            ui_state.scroll_delta.0 += ev.as_ref().delta_x;
            ui_state.scroll_delta.1 += ev.as_ref().delta_y;
            // Perform scroll action for deltas with abs(delta) >= 1.
            let x = ui_state.scroll_delta.0 as isize;
            let y = ui_state.scroll_delta.1 as isize;
            for _ in 0..x {
                mouse_input(state, "wheel", "right", ev.state(), ev.position())
            }
            for _ in 0..-x {
                mouse_input(state, "wheel", "left", ev.state(), ev.position())
            }
            for _ in 0..y {
                mouse_input(state, "wheel", "down", ev.state(), ev.position())
            }
            for _ in 0..-y {
                mouse_input(state, "wheel", "up", ev.state(), ev.position())
            }
            // Subtract performed scroll deltas.
            ui_state.scroll_delta.0 -= x as f64;
            ui_state.scroll_delta.1 -= y as f64;
        }
        _ => (),
    }
    Inhibit(false)
}

fn gtk_button_press(
    shell: &mut State,
    ui_state: &Rc<RefCell<UiState>>,
    ev: &EventButton,
    menu: &gtk::PopoverMenu,
) -> Inhibit {
    if ev.event_type() != EventType::ButtonPress {
        return Inhibit(false);
    }

    if shell.mouse_enabled {
        ui_state.borrow_mut().mouse_pressed = true;

        match ev.button() {
            1 => mouse_input(shell, "left", "press", ev.state(), ev.position()),
            2 => mouse_input(shell, "middle", "press", ev.state(), ev.position()),
            3 => {
                let (x, y) = ev.position();
                menu.set_pointing_to(&gtk::Rectangle {
                    x: x.round() as i32,
                    y: y.round() as i32,
                    width: 0,
                    height: 0,
                });

                // Popping up the menu will trigger a focus event, so handle this in the idle loop
                // to avoid a double borrow_mut()
                glib::idle_add_local_once(clone!(menu => move || menu.popup()));
            },
            _ => (),
        }
    }
    Inhibit(false)
}

fn mouse_input(
    shell: &mut State,
    button: &str,
    action: &str,
    state: ModifierType,
    position: (f64, f64)
) {
    if let Some(nvim) = shell.nvim() {
        let (col, row) = mouse_coordinates_to_nvim(shell, position);

        nvim.block_timeout(
            nvim.input_mouse(button, action, &keyval_to_input_string("", state), 0,
                             row as i64, col as i64)
        ).ok_and_report().expect("Can't send mouse input event");
    }
}

/**
 * Translate gtk mouse event coordinates to nvim (col, row).
 */
fn mouse_coordinates_to_nvim(shell: &State, position: (f64, f64)) -> (u64, u64) {
    let &CellMetrics {
        line_height,
        char_width,
        ..
    } = shell.render_state.borrow().font_ctx.cell_metrics();
    let (x, y) = position;
    let col = (x / char_width).trunc() as u64;
    let row = (y / line_height).trunc() as u64;
    (col, row)
}

fn gtk_button_release(shell: &mut State, ui_state: &mut UiState, ev: &EventButton) -> Inhibit {
    ui_state.mouse_pressed = false;

    if shell.mouse_enabled && !shell.nvim.is_initializing() {
        match ev.button() {
            1 => mouse_input(shell, "left", "release", ev.state(), ev.position()),
            2 => mouse_input(shell, "middle", "release", ev.state(), ev.position()),
            3 => mouse_input(shell, "right", "release", ev.state(), ev.position()),
            _ => (),
        }
    }

    Inhibit(false)
}

fn gtk_motion_notify(shell: &mut State, ui_state: &mut UiState, ev: &EventMotion) -> Inhibit {
    if shell.mouse_enabled && ui_state.mouse_pressed {
        let ev_pos = ev.position();
        let pos = mouse_coordinates_to_nvim(shell, ev_pos);

        // if we fire LeftDrag on the same coordinates multiple times, then
        // we get: https://github.com/daa84/neovim-gtk/issues/185
        if pos != ui_state.prev_pos {
            mouse_input(shell, "left", "drag", ev.state(), ev_pos);
            ui_state.prev_pos = pos;
        }
    }

    ui_state.apply_mouse_cursor(MouseCursor::Text, shell.drawing_area.window());
    Inhibit(false)
}

fn draw_content(state: &State, ctx: &cairo::Context) {
    ctx.push_group();

    let render_state = state.render_state.borrow();
    render::fill_background(
        ctx,
        &render_state.hl,
        state.transparency_settings.background_alpha(),
    );
    render::render(
        ctx,
        state.cursor.as_ref().unwrap(),
        &render_state.font_ctx,
        state.grids.current_model().unwrap(),
        &render_state.hl,
        state.transparency_settings.filled_alpha(),
    );

    ctx.pop_group_to_source().unwrap();
    ctx.paint().unwrap();
}

fn gtk_draw(state_arc: &Arc<UiMutex<State>>, ctx: &cairo::Context) -> Inhibit {
    let state = state_arc.borrow();
    if state.nvim.is_initialized() {
        draw_content(&*state, ctx);
    } else if state.nvim.is_initializing() {
        draw_initializing(&*state, ctx);
    }

    Inhibit(false)
}

fn show_nvim_start_error(err: &nvim::NvimInitError, state_arc: Arc<UiMutex<State>>) {
    let source = err.source();
    let cmd = err.cmd().unwrap().to_owned();

    glib::idle_add_once(move || {
        let state = state_arc.borrow();
        state.nvim.set_error();
        state.error_area.show_nvim_start_error(&source, &cmd);
        state.show_error_area();
    });
}

fn show_nvim_init_error(err: &nvim::NvimInitError, state_arc: Arc<UiMutex<State>>) {
    let source = err.source();

    glib::idle_add_once(move || {
        let state = state_arc.borrow();
        state.nvim.set_error();
        state.error_area.show_nvim_init_error(&source);
        state.show_error_area();
    });
}

fn init_nvim_async(
    state_arc: Arc<UiMutex<State>>,
    nvim_handler: NvimHandler,
    options: ShellOptions,
    cols: NonZeroI64,
    rows: NonZeroI64,
) {
    // execute nvim
    let (session, io_future) = match nvim::start(
        nvim_handler,
        options.nvim_bin_path.clone(),
        options.timeout,
        options.args_for_neovim,
    ) {
        Ok(session) => session,
        Err(err) => {
            show_nvim_start_error(&err, state_arc);
            return;
        }
    };

    set_nvim_to_state(state_arc.clone(), &session);

    // add callback on session end
    let cb_state_arc = state_arc.clone();
    session.spawn(io_future.map(|r| {
        if let Err(e) = r {
            if !e.is_reader_error() {
                error!("{}", e);
            }
        }
        glib::idle_add_once(move || {
            cb_state_arc.borrow().nvim.clear();
            if let Some(ref cb) = cb_state_arc.borrow().detach_cb {
                let sr = cb_state_arc.borrow();
                let lock = sr.exit_status.lock().unwrap();
                (&mut *cb.borrow_mut())(lock.unwrap_or(0));
            }
        });
    }));

    // attach ui
    let input_data = options.input_data;
    session.clone().spawn(async move {
        match nvim::post_start_init(session, cols, rows, input_data).await {
            Ok(_) => set_nvim_initialized(state_arc),
            Err(ref e) => show_nvim_init_error(e, state_arc),
        }
    });
}

fn set_nvim_to_state(state_arc: Arc<UiMutex<State>>, nvim: &NvimSession) {
    let pair = Arc::new((Mutex::new(None), Condvar::new()));
    let pair2 = pair.clone();
    let nvim = Some(nvim.clone());

    glib::idle_add_once(move || {
        state_arc.borrow().nvim.set(nvim.clone().unwrap());

        let &(ref lock, ref cvar) = &*pair2;
        let mut started = lock.lock().unwrap();
        *started = Some(nvim.clone());
        cvar.notify_one();
    });

    // Wait idle set nvim properly
    let &(ref lock, ref cvar) = &*pair;
    let mut started = lock.lock().unwrap();
    while started.is_none() {
        started = cvar.wait(started).unwrap();
    }
}

fn set_nvim_initialized(state_arc: Arc<UiMutex<State>>) {
    glib::idle_add_once(clone!(state_arc => move || {
        let mut state = state_arc.borrow_mut();
        state.nvim.set_initialized();
        // in some case resize can happens while initilization in progress
        // so force resize here
        state.try_nvim_resize();
        state.cursor.as_mut().unwrap().start();
    }));

    idle_cb_call!(state_arc.nvim_started_cb());
}

fn draw_initializing(state: &State, ctx: &cairo::Context) {
    let render_state = state.render_state.borrow();
    let hl = &render_state.hl;
    let layout = pangocairo::functions::create_layout(ctx).unwrap();
    let alloc = state.drawing_area.allocation();

    let bg_color = hl.bg();
    let fg_color = hl.fg();
    ctx.set_source_rgb(bg_color.0, bg_color.1, bg_color.2);
    ctx.paint().unwrap();

    layout.set_text("Loading->");
    let (width, height) = layout.pixel_size();

    let x = alloc.width as f64 / 2.0 - width as f64 / 2.0;
    let y = alloc.height as f64 / 2.0 - height as f64 / 2.0;

    ctx.move_to(x, y);
    ctx.set_source_rgb(fg_color.0, fg_color.1, fg_color.2);
    pangocairo::functions::update_layout(ctx, &layout);
    pangocairo::functions::show_layout(ctx, &layout);

    ctx.move_to(x + width as f64, y);
    state
        .cursor
        .as_ref()
        .unwrap()
        .draw(ctx, &render_state.font_ctx, y, false, &hl);
}

fn init_nvim(state_ref: &Arc<UiMutex<State>>) {
    let state = state_ref.borrow_mut();
    if state.start_nvim_initialization() {
        let (cols, rows) = state.calc_nvim_size();

        debug!("Init nvim {}/{}", cols, rows);

        let state_arc = state_ref.clone();
        let nvim_handler = NvimHandler::new(state_ref.clone(), state.resize_status());
        let options = state.options.borrow_mut().input_data();
        thread::spawn(move || init_nvim_async(
            state_arc, nvim_handler, options, cols, rows
        ));
    }
}

// Neovim redraw events
impl State {
    pub fn grid_line(
        &mut self,
        grid: u64,
        row: u64,
        col_start: u64,
        cells: Vec<Vec<Value>>,
    ) -> RepaintMode {
        let hl = &self.render_state.borrow().hl;
        let repaint_area = self.grids[grid].line(row as usize, col_start as usize, cells, hl);
        RepaintMode::Area(repaint_area)
    }

    pub fn grid_clear(&mut self, grid: u64) -> RepaintMode {
        let hl = &self.render_state.borrow().hl;
        self.grids[grid].clear(&hl.default_hl());
        RepaintMode::All
    }

    pub fn grid_destroy(&mut self, grid: u64) -> RepaintMode {
        self.grids.destroy(grid);
        RepaintMode::All
    }

    pub fn grid_cursor_goto(&mut self, grid: u64, row: u64, column: u64) -> RepaintMode {
        let repaint_area = self.grids[grid].cursor_goto(row as usize, column as usize);
        self.set_im_location();
        RepaintMode::AreaList(repaint_area)
    }

    pub fn grid_resize(&mut self, grid: u64, columns: u64, rows: u64) -> RepaintMode {
        debug!("on_resize {}/{}", columns, rows);

        let nvim = self.nvim().unwrap();
        nvim.block_on(async {
            let mut resize_state = self.resize_status.requests.lock().await;

            if resize_state.current.is_none() {
                resize_state.current = Some((
                    NonZeroI64::new(columns as i64).unwrap(),
                    NonZeroI64::new(rows as i64).unwrap(),
                ));
            }
        });

        self.grids.get_or_create(grid).resize(columns, rows);
        RepaintMode::Nothing
    }

    pub fn on_redraw(&mut self, mode: &RepaintMode) {
        match *mode {
            RepaintMode::All => {
                self.update_dirty_glyphs();
                self.drawing_area.queue_draw();
            }
            RepaintMode::Area(ref rect) => self.queue_draw_area(&[rect]),
            RepaintMode::AreaList(ref list) => self.queue_draw_area(&list.list),
            RepaintMode::Nothing => (),
        }
    }

    pub fn grid_scroll(
        &mut self,
        grid: u64,
        top: u64,
        bot: u64,
        left: u64,
        right: u64,
        rows: i64,
        cols: i64,
    ) -> RepaintMode {
        let hl = &self.render_state.borrow().hl;
        RepaintMode::Area(self.grids[grid].scroll(
            top,
            bot,
            left,
            right,
            rows,
            cols,
            &hl.default_hl(),
        ))
    }

    pub fn hl_attr_define(
        &mut self,
        id: u64,
        rgb_attr: HashMap<String, Value>,
        _: &Value,
        info: Vec<HashMap<String, Value>>,
    ) -> RepaintMode {
        self.render_state.borrow_mut().hl.set(id, &rgb_attr, &info);
        RepaintMode::Nothing
    }

    pub fn default_colors_set(
        &mut self,
        fg: i64,
        bg: i64,
        sp: i64,
        cterm_fg: i64,
        cterm_bg: i64,
    ) -> RepaintMode {
        self.render_state.borrow_mut().hl.set_defaults(
            if fg >= 0 {
                Some(Color::from_indexed_color(fg as u64))
            } else {
                None
            },
            if bg >= 0 {
                Some(Color::from_indexed_color(bg as u64))
            } else {
                None
            },
            if sp >= 0 {
                Some(Color::from_indexed_color(sp as u64))
            } else {
                None
            },
            if cterm_fg > 0 {
                Color::from_cterm((cterm_fg - 1) as u8)
            } else {
                COLOR_WHITE
            },
            if cterm_bg > 0 {
                Color::from_cterm((cterm_bg - 1) as u8)
            } else {
                COLOR_BLACK
            },
        );
        RepaintMode::All
    }

    fn cur_point_area(&self) -> RepaintMode {
        if let Some(cur_point) = self.grids.current().map(|g| g.cur_point()) {
            RepaintMode::Area(cur_point)
        } else {
            RepaintMode::Nothing
        }
    }

    pub fn on_mode_change(&mut self, mode: String, idx: u64) -> RepaintMode {
        let mut render_state = self.render_state.borrow_mut();
        render_state.mode.update(&mode, idx as usize);
        self.cursor
            .as_mut()
            .unwrap()
            .set_mode_info(render_state.mode.mode_info().cloned());
        self.cmd_line
            .set_mode_info(render_state.mode.mode_info().cloned());

        self.cur_point_area()
    }

    pub fn on_mouse(&mut self, on: bool) -> RepaintMode {
        self.mouse_enabled = on;
        RepaintMode::Nothing
    }

    pub fn on_busy(&mut self, busy: bool) -> RepaintMode {
        if busy {
            self.cursor.as_mut().unwrap().busy_on();
        } else {
            self.cursor.as_mut().unwrap().busy_off();
        }

        self.cur_point_area()
    }

    pub fn popupmenu_show(
        &mut self,
        menu: &[CompleteItem],
        selected: i64,
        row: u64,
        col: u64,
    ) -> RepaintMode {
        let point = ModelRect::point(col as usize, row as usize);
        let render_state = self.render_state.borrow();
        let (x, y, width, height) = point.to_area(render_state.font_ctx.cell_metrics());

        let context = popup_menu::PopupMenuContext {
            nvim: &self.nvim,
            hl: &render_state.hl,
            font_ctx: &render_state.font_ctx,
            menu_items: &menu,
            selected,
            x,
            y,
            width,
            height,
            max_width: self.max_popup_width(),
        };

        self.popup_menu.show(context);

        RepaintMode::Nothing
    }

    pub fn popupmenu_hide(&mut self) -> RepaintMode {
        self.popup_menu.hide();
        RepaintMode::Nothing
    }

    pub fn popupmenu_select(&mut self, selected: i64) -> RepaintMode {
        self.popup_menu.select(selected);
        RepaintMode::Nothing
    }

    pub fn tabline_update(
        &mut self,
        selected: Tabpage,
        tabs: Vec<(Tabpage, Option<String>)>,
    ) -> RepaintMode {
        self.tabs.update_tabs(&self.nvim, &selected, &tabs);

        RepaintMode::Nothing
    }

    pub fn option_set(&mut self, name: String, val: Value) -> RepaintMode {
        if let "guifont" = name.as_str() {
            self.set_font_from_value(val)
        };
        RepaintMode::Nothing
    }

    fn set_font_from_value(&mut self, val: Value) {
        if let Value::String(val) = val {
            if let Some(val) = val.into_str() {
                if !val.is_empty() {
                    let exists_fonts = self.render_state.borrow().font_ctx.font_families();
                    let fonts = split_at_comma(&val);
                    for font in &fonts {
                        let desc = FontDescription::from_string(&font);
                        if desc.size() > 0
                            && exists_fonts.contains(&desc.family().unwrap_or_else(|| "".into()))
                        {
                            self.set_font_rpc(font);
                            return;
                        }
                    }

                    // font does not exists? set first one
                    if !fonts.is_empty() {
                        self.set_font_rpc(&fonts[0]);
                    }
                }
            }
        }
    }

    pub fn mode_info_set(
        &mut self,
        cursor_style_enabled: bool,
        mode_infos: Vec<HashMap<String, Value>>,
    ) -> RepaintMode {
        let mode_info_arr = mode_infos
            .iter()
            .map(|mode_info_map| mode::ModeInfo::new(mode_info_map))
            .collect();

        match mode_info_arr {
            Ok(mode_info_arr) => {
                let mut render_state = self.render_state.borrow_mut();
                render_state
                    .mode
                    .set_info(cursor_style_enabled, mode_info_arr);
            }
            Err(err) => {
                error!("Error load mode info: {}", err);
            }
        }

        RepaintMode::Nothing
    }

    pub fn cmdline_show(
        &mut self,
        content: Vec<(u64, String)>,
        pos: u64,
        firstc: String,
        prompt: String,
        indent: u64,
        level: u64,
    ) -> RepaintMode {
        {
            let cursor = self.grids.current().unwrap().cur_point();
            let render_state = self.render_state.borrow();
            let (x, y, width, height) = cursor.to_area(render_state.font_ctx.cell_metrics());
            let ctx = CmdLineContext {
                nvim: &self.nvim,
                content,
                pos,
                firstc,
                prompt,
                indent,
                level_idx: level,
                x,
                y,
                width,
                height,
                max_width: self.max_popup_width(),
            };

            self.cmd_line.show_level(&ctx);
        }

        self.on_busy(true)
    }

    pub fn cmdline_hide(&mut self, level: u64) -> RepaintMode {
        self.cmd_line.hide_level(level);
        self.on_busy(false)
    }

    pub fn cmdline_block_show(&mut self, content: Vec<Vec<(u64, String)>>) -> RepaintMode {
        let max_width = self.max_popup_width();
        self.cmd_line.show_block(&content, max_width);
        self.on_busy(true)
    }

    pub fn cmdline_block_append(&mut self, content: Vec<(u64, String)>) -> RepaintMode {
        self.cmd_line.block_append(&content);
        RepaintMode::Nothing
    }

    pub fn cmdline_block_hide(&mut self) -> RepaintMode {
        self.cmd_line.block_hide();
        self.on_busy(false)
    }

    pub fn cmdline_pos(&mut self, pos: u64, level: u64) -> RepaintMode {
        let render_state = self.render_state.borrow();
        self.cmd_line.pos(&*render_state, pos, level);
        RepaintMode::Nothing
    }

    pub fn cmdline_special_char(&mut self, c: String, shift: bool, level: u64) -> RepaintMode {
        let render_state = self.render_state.borrow();
        self.cmd_line.special_char(&*render_state, c, shift, level);
        RepaintMode::Nothing
    }

    pub fn wildmenu_show(&self, items: Vec<String>) -> RepaintMode {
        self.cmd_line
            .show_wildmenu(items, &*self.render_state.borrow(), self.max_popup_width());
        RepaintMode::Nothing
    }

    pub fn wildmenu_hide(&self) -> RepaintMode {
        self.cmd_line.hide_wildmenu();
        RepaintMode::Nothing
    }

    pub fn wildmenu_select(&self, selected: i64) -> RepaintMode {
        self.cmd_line.wildmenu_select(selected);
        RepaintMode::Nothing
    }
}

impl CursorRedrawCb for State {
    fn queue_redraw_cursor(&mut self) {
        if let Some(cur_point) = self.grids.current().map(|g| g.cur_point()) {
            self.on_redraw(&RepaintMode::Area(cur_point));
        }
    }
}
