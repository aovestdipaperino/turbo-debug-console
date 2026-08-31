// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! `turbo-debug-console` — a Turbo Vision monitor for model-token streams.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use trace_stream::render::RenderOptions;
use turbo_debug_console::cmd;
use turbo_debug_console::proto::{PROTOCOL_VERSION, StreamKind};
use turbo_debug_console::registry::{Server, ServerEvent, SessionId};
use turbo_debug_console::session::{Sessions, SharedStreamView, format_title};
use turbo_debug_console::streamview::StreamView;
use turbo_vision::app::Application;
use turbo_vision::core::command::{CM_NEXT, CM_QUIT};
use turbo_vision::core::event::{EventType, KB_ALT_X, KB_F6, KB_F10};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::menu_data::{Menu, MenuItem};
use turbo_vision::core::state::{SF_CLOSED, shadow_size};
use turbo_vision::views::file_dialog::FileDialog;
use turbo_vision::views::menu_bar::{MenuBar, SubMenu};
use turbo_vision::views::msgbox;
use turbo_vision::views::status_line::{StatusItem, StatusLine};
use turbo_vision::views::view::ViewId;
use turbo_vision::views::window::{Window, WindowBuilder};

/// A bounded number of pending stream-server events drained per main-loop
/// iteration. `Server::events()` is an unbounded channel, so a client
/// streaming faster than the pipeline/draw can keep up (`cat bigfile | nc
/// ...`) would otherwise spin this inner loop forever and never return to
/// `app.get_event()` — starving keystrokes, redraws and Alt-X. A message
/// count is a simpler, more predictable budget here than a time slice (no
/// clock reads on a hot loop, and behavior does not depend on how fast the
/// draw happens to be on the day it is run).
const MAX_EVENTS_PER_TICK: usize = 64;

/// The well-known control port clients `HELLO` into.
const CONTROL_PORT: u16 = 7878;

/// The render options every `Pipeline` is created with. Thinking text and
/// markdown formatting are always on -- a debug console that can be put
/// into a state where it does not format its input is a worse tool, so
/// these are no longer user-toggleable (see the removed View > Show
/// thinking / View > Markdown menu items).
const RENDER_OPTIONS: RenderOptions = RenderOptions {
    use_color: true,
    format_thinking: true,
    format_markdown: true,
};

/// Handles `--version` / `--help` and exits, before any terminal setup.
///
/// Both must be answered without initialising the UI: the terminal goes into
/// raw mode as soon as `Application::new` runs, and a packager smoke-testing
/// the binary (`brew test`) has no TTY to give it.
fn handle_cli_flags() {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("turbo-debug-console {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "turbo-debug-console {}\n\
                     {}\n\
                     \n\
                     USAGE:\n    turbo-debug-console\n\
                     \n\
                     Takes no options: it listens on the fixed control port {CONTROL_PORT}.\n\
                     \n\
                     Name a session and get a port to stream at:\n\
                     \n    printf 'HELLO {PROTOCOL_VERSION} tokens build\\n' | nc 127.0.0.1 {CONTROL_PORT}\n\
                     \n\
                     <kind> is 'tokens' (a model token stream) or 'trace' (JSON-lines\n\
                     tracing-subscriber records):\n\
                     \n    printf 'HELLO {PROTOCOL_VERSION} trace myapp\\n' | nc 127.0.0.1 {CONTROL_PORT}\n\
                     \n\
                     Or skip the handshake -- anything that is not a HELLO is\n\
                     rendered as a raw token stream in its own window:\n\
                     \n    cat capture.txt | nc 127.0.0.1 {CONTROL_PORT}\n\
                     \n\
                     KEYS\n    \
                     F10 menu   F6 next window   Alt-X quit\n    \
                     PgUp/PgDn/Home/End scroll the focused window\n\
                     \n\
                     {}",
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_DESCRIPTION"),
                    env!("CARGO_PKG_REPOSITORY"),
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

/// Sets the host terminal's window/tab title (OSC 2). Written straight to
/// stdout before the UI comes up: turbo-vision has no title API of its own,
/// and the escape is inert on terminals that do not understand it.
fn set_terminal_title(title: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]2;{title}\x07");
    let _ = out.flush();
}

fn main() -> turbo_vision::core::error::Result<()> {
    handle_cli_flags();

    set_terminal_title("Turbo Debug Console");

    let mut app = Application::new()?;
    let (width, height) = app.terminal.size();
    app.set_menu_bar(build_menu_bar(width));
    app.set_status_line(build_status_line(width, height, 0));

    let mut server = match Server::bind(CONTROL_PORT) {
        Ok(s) => s,
        Err(e) => {
            // The terminal is already in raw mode; drop out of it before
            // printing, or the message lands in a half-torn-down screen.
            drop(app);
            eprintln!("turbo-debug-console: cannot bind 127.0.0.1:{CONTROL_PORT}: {e}");
            std::process::exit(1);
        }
    };

    let mut console = Console::default();

    // The window-dependent commands all start disabled: there is no window
    // and nothing selected yet. `sync_command_state` keeps them in step from
    // here on.
    for command in [
        cmd::CM_COPY,
        cmd::CM_SAVE_AS,
        cmd::CM_SELECT_ALL,
        cmd::CM_CLEAR_WINDOW,
        cmd::CM_CLEANUP,
    ] {
        app.disable_command(command);
    }

    app.draw();
    let _ = app.terminal.flush();

    let mut last_reap = Instant::now();
    let mut last_status_tick = Instant::now();

    while app.running {
        let mut dirty = false;

        if let Some(mut event) = app.get_event() {
            // Re-assert the command state *before* the event is handled.
            // `get_event` calls `Application::idle()` internally on every
            // poll timeout, and `idle()` re-enables `CM_TILE` / `CM_CASCADE`
            // whenever the desktop holds *any* tileable window — clobbering
            // the stricter "more than one window" rule below. Syncing only
            // before `draw` is too late: a menu dropdown is painted inside
            // `handle_event`, so it would show the clobbered state.
            console.sync_command_state(&mut app);
            app.handle_event(&mut event);
            dirty = true;
            if event.what == EventType::Command {
                console.handle_command(&mut app, event.command);
            }
        }

        for _ in 0..MAX_EVENTS_PER_TICK {
            let Ok(ev) = server.events().try_recv() else {
                break;
            };
            dirty = true;
            console.handle_server_event(&mut app, ev);
        }

        if last_reap.elapsed() > Duration::from_secs(60) {
            last_reap = Instant::now();
            server.reap(Duration::from_mins(30));
        }

        if last_status_tick.elapsed() >= Duration::from_secs(1) {
            last_status_tick = Instant::now();
            let (w, h) = app.terminal.size();
            app.set_status_line(build_status_line(w, h, server.live_count()));
            dirty = true;
        }

        if dirty {
            console.sync_command_state(&mut app);
            app.draw();
            let _ = app.terminal.flush();
        }
        // `Application::run()` calls this each iteration; the hand-rolled
        // loop must too, or a moved window's move-tracking state is never
        // cleared and `get_redraw_union()` returns stale data indefinitely.
        app.desktop.handle_moved_windows(&mut app.terminal);

        if app.desktop.remove_closed_windows() {
            console.forget_closed_windows(&app, &mut server);
            app.draw();
            let _ = app.terminal.flush();
        }
    }

    Ok(())
}

/// Everything the main loop mutates across frames: live sessions and the
/// `ViewId <-> SessionId` maps needed to find the focused window's session.
///
/// `window_ids` maps a desktop `ViewId` to the session it belongs to, and is
/// how the focused window's session id is found each frame. **Deviation
/// from the brief:** rather than tracking a `HashMap<u8, SessionId>` keyed
/// by a Borland-style window number (this turbo-vision version does not
/// assign window numbers automatically), focus is read directly from
/// `Desktop::top_view_id()`, which the library already keeps correct across
/// `CM_NEXT`, clicks, and window close — a strictly more reliable source of
/// truth for "which window is focused" than a number this crate would have
/// to assign and track itself.
#[derive(Default)]
struct Console {
    sessions: Sessions,
    window_ids: HashMap<ViewId, SessionId>,
    session_windows: HashMap<SessionId, ViewId>,
}

/// What a decided [`ServerEvent`] means for the desktop: create a window for
/// a session, retitle one, or close one. Carries no `Application` reference,
/// so deciding which of these applies is testable without a TTY; only
/// *applying* one touches the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConsoleIntent {
    CreateWindow {
        id: SessionId,
        name: String,
        port: u16,
        kind: StreamKind,
    },
    Retitle {
        view_id: ViewId,
        title: String,
    },
    CloseWindow {
        view_id: ViewId,
    },
}

impl Console {
    fn handle_command(
        &mut self,
        app: &mut Application,
        command: turbo_vision::core::command::CommandId,
    ) {
        let focused_id = app
            .desktop
            .top_view_id()
            .and_then(|id| self.window_ids.get(&id).copied());

        match command {
            cmd::CM_CLEAR_WINDOW => {
                if let Some(id) = focused_id {
                    self.sessions.clear(id);
                }
            }
            cmd::CM_SAVE_AS => {
                if let Some(id) = focused_id {
                    self.save_as(app, id);
                }
            }
            cmd::CM_SELECT_ALL => {
                if let Some(id) = focused_id {
                    self.sessions.select_all(id);
                }
            }
            cmd::CM_COPY => {
                if let Some(id) = focused_id {
                    self.copy_selection(app, id);
                }
            }
            cmd::CM_OPEN_CAPTURE => self.open_capture(app),
            cmd::CM_CLEANUP => self.cleanup_windows(app),
            cmd::CM_TILE_WINDOWS => app.tile(),
            cmd::CM_CASCADE_WINDOWS => app.cascade(),
            _ => {}
        }
    }

    /// Window > Cleanup: closes every window whose session no longer has a
    /// client attached, leaving the live ones alone. The windows are only
    /// flagged `SF_CLOSED` here; the main loop's existing
    /// `remove_closed_windows` sweep removes them and
    /// [`Self::forget_closed_windows`] drops the bookkeeping, so this takes
    /// exactly the same path as a user closing each window by hand.
    fn cleanup_windows(&mut self, app: &mut Application) {
        let stale: Vec<ViewId> = self
            .window_ids
            .iter()
            .filter(|(_, id)| !self.sessions.is_connected(**id))
            .map(|(view_id, _)| *view_id)
            .collect();
        for view_id in stale {
            if let Some(view) = app.desktop.child_by_id_mut(view_id) {
                view.set_state_flag(SF_CLOSED, true);
            }
        }
    }

    /// Copies the focused window's current selection to the clipboard, with a
    /// brief confirmation. `CM_COPY` is disabled unless something is selected
    /// (see [`Self::can_copy`]), so the empty case is just a defensive no-op.
    fn copy_selection(&self, app: &mut Application, id: SessionId) {
        let Some(text) = self.sessions.selected_text(id).filter(|t| !t.is_empty()) else {
            return;
        };
        let lines = text.lines().count();
        turbo_vision::core::clipboard::set_clipboard(&text);
        msgbox::message_box_ok(
            app,
            &format!(
                "Copied {lines} line{} to the clipboard.",
                if lines == 1 { "" } else { "s" }
            ),
        );
    }

    /// Whether the focused window has copyable (non-empty) selected text.
    fn can_copy(&self, focused_id: Option<SessionId>) -> bool {
        focused_id
            .and_then(|id| self.sessions.selected_text(id))
            .is_some_and(|t| !t.is_empty())
    }

    /// Whether any tracked window's session is disconnected — the exact
    /// condition [`Self::cleanup_windows`] needs something to do.
    fn has_stale_windows(&self) -> bool {
        self.window_ids
            .values()
            .any(|id| !self.sessions.is_connected(*id))
    }

    /// Greys out every menu item that needs something to act on: `CM_COPY`
    /// unless the focused window has a selection, the focused-window
    /// commands unless there *is* a focused stream window, and `CM_CLEANUP`
    /// unless at least one window is disconnected. Cheap enough to call each
    /// frame.
    fn sync_command_state(&self, app: &mut Application) {
        let focused_id = app
            .desktop
            .top_view_id()
            .and_then(|id| self.window_ids.get(&id).copied());
        if self.can_copy(focused_id) {
            app.enable_command(cmd::CM_COPY);
        } else {
            app.disable_command(cmd::CM_COPY);
        }
        // Next / Tile / Cascade only mean something with more than one
        // window. `CM_NEXT` is safe to drive directly, but Tile and Cascade
        // use this crate's own command ids: Turbo Vision's `idle()` owns the
        // enabled state of its `CM_TILE` / `CM_CASCADE` and re-enables them
        // for any window count >= 1 (see `cmd::CM_CASCADE_WINDOWS`).
        let multiple_windows = app.desktop.count_tileable_windows() > 1;
        for command in [CM_NEXT, cmd::CM_TILE_WINDOWS, cmd::CM_CASCADE_WINDOWS] {
            if multiple_windows {
                app.enable_command(command);
            } else {
                app.disable_command(command);
            }
        }
        for command in [cmd::CM_SAVE_AS, cmd::CM_SELECT_ALL, cmd::CM_CLEAR_WINDOW] {
            if focused_id.is_some() {
                app.enable_command(command);
            } else {
                app.disable_command(command);
            }
        }
        if self.has_stale_windows() {
            app.enable_command(cmd::CM_CLEANUP);
        } else {
            app.disable_command(cmd::CM_CLEANUP);
        }
    }

    /// Applies a live stream-server event to a desktop, via the pure
    /// decision made by [`Self::decide_server_event`].
    fn handle_server_event(&mut self, app: &mut Application, ev: ServerEvent) {
        let Some(intent) = self.decide_server_event(ev) else {
            return;
        };
        self.apply_intent(app, intent);
    }

    /// Decides what a `ServerEvent` means for this console's bookkeeping and
    /// what (if anything) the caller must do to the desktop. Pure decision
    /// logic: it touches `self.sessions` / `self.window_ids` /
    /// `self.session_windows` only, never an `Application`, so it is
    /// unit-testable without a TTY.
    fn decide_server_event(&mut self, ev: ServerEvent) -> Option<ConsoleIntent> {
        match ev {
            ServerEvent::Opened {
                id,
                name,
                port,
                kind,
            } => Some(ConsoleIntent::CreateWindow {
                id,
                name,
                port,
                kind,
            }),
            ServerEvent::Attached { id, reattached } => {
                self.sessions.mark_attached(id, reattached);
                self.retitle_intent(id)
            }
            ServerEvent::Bytes { id, data } => {
                self.sessions.feed(id, &data);
                None
            }
            ServerEvent::Disconnected { id } => {
                self.sessions.mark_disconnected(id);
                self.retitle_intent(id)
            }
            ServerEvent::Closed { id } => {
                // The server sends this once per session, on its own
                // 30-minute idle TTL (`Server::reap`) — invisible in a short
                // manual test, but certain in a long-running session. Drop
                // both bookkeeping maps here so a stale entry can never
                // misattribute a later command, and hand back an intent so
                // the caller actually removes the window: otherwise it sits
                // on screen forever, frozen on its last frame.
                self.sessions.remove(id);
                let view_id = self.session_windows.remove(&id)?;
                self.window_ids.remove(&view_id);
                Some(ConsoleIntent::CloseWindow { view_id })
            }
        }
    }

    /// The retitle intent for a session's window, if it still has one and
    /// still has a title to show (both `None` for e.g. a capture window,
    /// which isn't tracked in `session_windows`).
    fn retitle_intent(&self, id: SessionId) -> Option<ConsoleIntent> {
        let view_id = *self.session_windows.get(&id)?;
        let title = self.sessions.window_title(id)?;
        Some(ConsoleIntent::Retitle { view_id, title })
    }

    /// Performs the turbo-vision side effects a decided [`ConsoleIntent`]
    /// calls for.
    fn apply_intent(&mut self, app: &mut Application, intent: ConsoleIntent) {
        match intent {
            ConsoleIntent::CreateWindow {
                id,
                name,
                port,
                kind,
            } => {
                let window_bounds = tile_window_bounds(app);
                let view = Rc::new(RefCell::new(StreamView::new(session_view_bounds(
                    window_bounds,
                ))));
                self.sessions.insert(
                    id,
                    name.clone(),
                    port,
                    kind,
                    Rc::clone(&view),
                    RENDER_OPTIONS,
                );
                // Title the window from the session itself rather than from
                // `format_title` alone. A session is created on `Opened` —
                // the HELLO handshake — and only becomes connected later,
                // when the client dials the data port, which it may never
                // do. Building the title here from `name`/`port` only made
                // such a window look attached while `connected` was still
                // false: the window carried no `[disconnected]` marker, yet
                // Window > Cleanup (rightly) counted it as one to sweep, so
                // Cleanup appeared enabled with nothing visibly
                // disconnected on screen. Asking the session for its own
                // title keeps the two in step from the first frame.
                let title = self
                    .sessions
                    .window_title(id)
                    .unwrap_or_else(|| format_title(&name, port));
                let mut window = WindowBuilder::new()
                    .bounds(window_bounds)
                    .title(title)
                    .build();
                apply_session_window_palette(&mut window);
                window.add(Box::new(SharedStreamView(view)));
                let view_id = app.desktop.add(Box::new(window));
                self.window_ids.insert(view_id, id);
                self.session_windows.insert(id, view_id);
            }
            ConsoleIntent::Retitle { view_id, title } => {
                if let Some(view) = app.desktop.child_by_id_mut(view_id)
                    && let Some(window) = view.as_any_mut().downcast_mut::<Window>()
                {
                    window.set_title(&title);
                }
            }
            ConsoleIntent::CloseWindow { view_id } => {
                // Mark the window SF_CLOSED so the desktop's normal
                // `remove_closed_windows` sweep (already run each loop
                // iteration) picks it up, exactly as a user-driven close
                // does — no separate removal path to keep in sync.
                if let Some(view) = app.desktop.child_by_id_mut(view_id) {
                    view.set_state_flag(SF_CLOSED, true);
                }
            }
        }
    }

    /// A window may have been closed by the user (frame close box,
    /// Window > Close) rather than by the server. Drop any mapping whose
    /// `ViewId` is no longer on the desktop so a stale entry cannot
    /// misattribute a later command to the wrong session, drop the closed
    /// window's `SessionState` so it does not leak (a 10,000-line
    /// `StreamView` plus its `Pipeline`, kept alive with no window to show
    /// it, for the rest of the process's life), and tear down the
    /// server-side session so its port and accept thread are actually
    /// released.
    ///
    /// **Semantics decision (defect 2):** closing a window tears the
    /// server-side session down too, rather than leaving it running
    /// headless for a later reconnect. A closed window is the user's
    /// explicit signal that they are done watching this stream — plank
    /// already has a distinct, weaker "went away" state (`Disconnected`,
    /// idle-but-listening, reconnectable) for the case the user *hasn't*
    /// asked to stop; reusing that for an explicit close would mean a
    /// stream nobody can see keeps consuming a port and a thread
    /// indefinitely, never reaped (a live session is never a reap
    /// candidate) and never reachable again (no window to reopen it into).
    /// If a later use case wants "detach but keep running", that is a
    /// different, explicit command from Close — not the current default.
    fn forget_closed_windows(&mut self, app: &Application, server: &mut Server) {
        let closed: Vec<SessionId> = self
            .window_ids
            .iter()
            .filter(|(view_id, _)| !app.desktop.contains_id(**view_id))
            .map(|(_, id)| *id)
            .collect();
        self.window_ids
            .retain(|view_id, _| app.desktop.contains_id(*view_id));
        self.session_windows
            .retain(|_, view_id| app.desktop.contains_id(*view_id));
        for id in closed {
            self.sessions.remove(id);
            server.close_session(id);
        }
    }

    fn save_as(&self, app: &mut Application, id: SessionId) {
        let Some(text) = self.sessions.plain_text(id) else {
            return;
        };
        let mut dialog = build_file_dialog(app, "Save As")
            .with_button_label("~S~ave")
            .build();
        if let Some(path) = dialog.execute(app)
            && let Err(e) = std::fs::write(&path, text)
        {
            msgbox::message_box_error(app, &format!("Cannot write {}: {e}", path.display()));
        }
    }

    fn open_capture(&mut self, app: &mut Application) {
        let mut dialog = build_file_dialog(app, "Open Capture").build();
        let Some(path) = dialog.execute(app) else {
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                msgbox::message_box_error(app, &format!("Cannot read {}: {e}", path.display()));
                return;
            }
        };

        let name = basename(&path);
        let window_bounds = tile_window_bounds(app);
        let view = Rc::new(RefCell::new(StreamView::new(session_view_bounds(
            window_bounds,
        ))));
        let id = NEXT_CAPTURE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        self.sessions.insert(
            id,
            name.clone(),
            0,
            StreamKind::Tokens,
            Rc::clone(&view),
            RENDER_OPTIONS,
        );
        if let Some(state) = self.sessions.get_mut(id) {
            state.connected = true;
            state.feed(&bytes);
            state.finish();
        }

        let mut window = WindowBuilder::new()
            .bounds(window_bounds)
            .title(name)
            .build();
        apply_session_window_palette(&mut window);
        window.add(Box::new(SharedStreamView(view)));
        let view_id = app.desktop.add(Box::new(window));
        self.window_ids.insert(view_id, id);
    }
}

/// Bounds for a new stream window: the full tile rect, shrunk on the
/// bottom-right by the window shadow. Every window shows its shadow by
/// default (`SF_SHADOW`), and a window placed at the *exact* desktop bounds
/// gets pushed up-and-left by `constrain_to_limits` to make room for that
/// shadow — hiding its entire top row (title bar included) above row 0 and
/// off screen. Reserving that room up front keeps the window, title bar
/// included, fully on screen.
fn tile_window_bounds(app: &Application) -> Rect {
    let mut bounds = app.get_tile_rect();
    let (shadow_x, shadow_y) = shadow_size();
    bounds.b.x -= shadow_x;
    bounds.b.y -= shadow_y;
    bounds
}

/// Bounds for a session's `StreamView`, given the *window's* outer bounds.
///
/// `Window::add` places children in the window's interior `Group`, whose
/// children take bounds relative to the interior (starting at `(0, 0)`) —
/// `Group::add` converts relative to absolute using the interior's own
/// origin. The interior itself is the window bounds grown by `(-1, -1)`
/// (one row/column inset for the frame on every side), so with `Rect::b`
/// exclusive the interior is exactly two narrower and two shorter than the
/// outer window. Passing the outer window bounds here (as before) drew the
/// view over the frame instead of inside it. See
/// `turbo-vision-4-rust/src/views/log_window.rs` for the same idiom.
fn session_view_bounds(window_bounds: Rect) -> Rect {
    Rect::new(0, 0, window_bounds.width() - 2, window_bounds.height() - 2)
}

/// Points a session window's palette at the app palette's "Black Window"
/// region (`CP_APP_COLOR` entries 97-104) instead of the default Blue
/// window colors, per the user's request for a black background.
///
/// Only the 8 window-palette entries (frame passive/active/icon, scrollbar
/// page/arrows, normal/selected text, reserved) are overridden — matching
/// `CP_BLUE_WINDOW`'s own first 8 entries and the `LogWindowBuilder`
/// precedent in `log_window.rs`. `Palette::get` returns the error color 0
/// (not a panic or garbage read) for any index beyond the slice's length,
/// so the 11 trailing syntax-highlighting entries (9-19) that `CP_BLUE_WINDOW`
/// carries are simply left unmapped here; `StreamView` paints its own
/// `Cell`s with explicit `Attr`s from the ANSI parser, so those entries are
/// never consulted for the stream text, and only the frame/scrollbar read
/// the 8 entries we do provide. Frame passive/active (entries 1-2, mapped
/// to app colors 97-98) keep the same layout as the Blue palette's own
/// frame entries, so the title stays legible focused and unfocused.
fn apply_session_window_palette(window: &mut Window) {
    window.set_custom_palette(vec![97, 98, 99, 100, 101, 102, 103, 104]);
}

fn build_menu_bar(width: i16) -> MenuBar {
    let mut menu_bar = MenuBar::new(Rect::new(0, 0, width, 1));

    menu_bar.add_submenu(SubMenu::new(
        "~F~ile",
        Menu::from_items(vec![
            MenuItem::new("~O~pen capture...", cmd::CM_OPEN_CAPTURE, 0, 0),
            MenuItem::new("~S~ave As...", cmd::CM_SAVE_AS, 0, 0),
            MenuItem::separator(),
            MenuItem::new("E~x~it", CM_QUIT, 0, 0),
        ]),
    ));
    menu_bar.add_submenu(SubMenu::new(
        "~E~dit",
        Menu::from_items(vec![
            MenuItem::new("~C~opy", cmd::CM_COPY, 0, 0),
            MenuItem::new("Select ~A~ll", cmd::CM_SELECT_ALL, 0, 0),
            MenuItem::separator(),
            MenuItem::new("C~l~ear window", cmd::CM_CLEAR_WINDOW, 0, 0),
        ]),
    ));
    menu_bar.add_submenu(SubMenu::new(
        "~W~indow",
        Menu::from_items(vec![
            MenuItem::new("~N~ext", CM_NEXT, 0, 0),
            MenuItem::new("~T~ile", cmd::CM_TILE_WINDOWS, 0, 0),
            MenuItem::new("C~a~scade", cmd::CM_CASCADE_WINDOWS, 0, 0),
            MenuItem::separator(),
            MenuItem::new("Clean~u~p", cmd::CM_CLEANUP, 0, 0),
        ]),
    ));

    menu_bar
}

fn build_status_line(width: i16, height: i16, live: usize) -> StatusLine {
    StatusLine::new(
        Rect::new(0, height - 1, width, height),
        vec![
            StatusItem::new("~F6~ Next", KB_F6, CM_NEXT),
            StatusItem::new("~F10~ Menu", KB_F10, 0),
            StatusItem::new("~Alt-X~ Exit", KB_ALT_X, CM_QUIT),
            StatusItem::new(&format!("{live} conn"), 0, 0),
        ],
    )
}

/// A centered file dialog sized to the current terminal, showing all files.
fn build_file_dialog(app: &Application, title: &str) -> FileDialog {
    let (width, height) = app.terminal.size();
    let dialog_width = 62.min(width);
    let dialog_height = 20.min(height);
    let dialog_x = (width - dialog_width) / 2;
    let dialog_y = (height - dialog_height) / 2;
    FileDialog::new(
        Rect::new(
            dialog_x,
            dialog_y,
            dialog_x + dialog_width,
            dialog_y + dialog_height,
        ),
        title,
        "*",
        None,
    )
}

fn basename(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// A synthetic, negative-space session id: capture windows never collide
/// with server-assigned ids because those start at 1 and only increase, so
/// a fixed high band is a simple, adequate allocator here.
static NEXT_CAPTURE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX / 2);

#[cfg(test)]
mod console_decision_tests {
    use super::*;
    use turbo_debug_console::registry::ServerEvent;

    /// Window > Cleanup lights up from `Sessions::is_connected`, the same
    /// state a window title reports as `[disconnected]`. A session is
    /// disconnected from the moment it is created (the HELLO) until its
    /// client dials the data port, so a freshly `Opened` session counts as
    /// stale, `Attached` clears it, and `Disconnected` brings it back.
    /// (`has_stale_windows` itself needs real `ViewId`s, which only a live
    /// desktop can mint, so this pins the decision it reads.)
    #[test]
    fn cleanup_availability_follows_the_session_connect_state() {
        let mut console = Console::default();
        // `decide_server_event` leaves session creation to `apply_intent`
        // (it needs a desktop); register the session the same way.
        console.sessions.insert(
            1,
            "demo".into(),
            61278,
            StreamKind::Tokens,
            Rc::new(RefCell::new(StreamView::new(Rect::new(0, 0, 20, 5)))),
            RENDER_OPTIONS,
        );
        assert!(
            !console.sessions.is_connected(1),
            "a session whose client has not attached yet is a cleanup candidate"
        );

        console.decide_server_event(ServerEvent::Attached {
            id: 1,
            reattached: false,
        });
        assert!(
            console.sessions.is_connected(1),
            "an attached session is not a cleanup candidate"
        );

        console.decide_server_event(ServerEvent::Disconnected { id: 1 });
        assert!(
            !console.sessions.is_connected(1),
            "a session whose client went away is a cleanup candidate again"
        );
    }

    #[test]
    fn opened_decides_to_create_a_window() {
        let mut console = Console::default();
        let intent = console.decide_server_event(ServerEvent::Opened {
            id: 1,
            name: "demo".into(),
            port: 61278,
            kind: StreamKind::Tokens,
        });
        assert_eq!(
            intent,
            Some(ConsoleIntent::CreateWindow {
                id: 1,
                name: "demo".into(),
                port: 61278,
                kind: StreamKind::Tokens,
            })
        );
    }

    #[test]
    fn opened_carries_the_trace_kind_through() {
        let mut console = Console::default();
        let intent = console.decide_server_event(ServerEvent::Opened {
            id: 1,
            name: "myapp".into(),
            port: 61279,
            kind: StreamKind::Trace,
        });
        assert_eq!(
            intent,
            Some(ConsoleIntent::CreateWindow {
                id: 1,
                name: "myapp".into(),
                port: 61279,
                kind: StreamKind::Trace,
            })
        );
    }

    #[test]
    fn bytes_decides_nothing_but_still_feeds_the_session() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            4242,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        let intent = console.decide_server_event(ServerEvent::Bytes {
            id: 1,
            data: b"hello\n".to_vec(),
        });
        assert_eq!(intent, None);
        assert!(console.sessions.plain_text(1).unwrap().contains("hello"));
    }

    #[test]
    fn attached_reattach_decides_to_retitle_the_mapped_window() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            4242,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        let view_id = ViewId::from_u16(7);
        console.session_windows.insert(1, view_id);
        console.window_ids.insert(view_id, 1);

        let intent = console.decide_server_event(ServerEvent::Attached {
            id: 1,
            reattached: true,
        });

        assert_eq!(
            intent,
            Some(ConsoleIntent::Retitle {
                view_id,
                title: "demo :4242".into(),
            })
        );
    }

    /// Regression test for defect 1: the ordinary first-attach path (no
    /// prior handshake reconnect) must also mark the session connected and
    /// retitle its window — not stay stuck reading `[disconnected]` for the
    /// entire first connection.
    #[test]
    fn attached_first_attach_also_decides_to_retitle_the_mapped_window() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            4242,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        let view_id = ViewId::from_u16(8);
        console.session_windows.insert(1, view_id);
        console.window_ids.insert(view_id, 1);

        let intent = console.decide_server_event(ServerEvent::Attached {
            id: 1,
            reattached: false,
        });

        assert_eq!(
            intent,
            Some(ConsoleIntent::Retitle {
                view_id,
                title: "demo :4242".into(),
            })
        );
    }

    /// Regression test for the "Closed leaks an orphan window" finding:
    /// `Server::reap`'s 30-minute idle TTL sends `Closed` once per session,
    /// and the decision must produce a close-the-window intent — not just
    /// drop the bookkeeping maps and leave the window on screen forever.
    #[test]
    fn closed_decides_to_close_the_mapped_window_and_forgets_it() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            4242,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        let view_id = ViewId::from_u16(9);
        console.session_windows.insert(1, view_id);
        console.window_ids.insert(view_id, 1);

        let intent = console.decide_server_event(ServerEvent::Closed { id: 1 });

        assert_eq!(intent, Some(ConsoleIntent::CloseWindow { view_id }));
        assert!(!console.session_windows.contains_key(&1));
        assert!(!console.window_ids.contains_key(&view_id));
        assert!(console.sessions.plain_text(1).is_none());
    }

    #[test]
    fn closed_with_no_mapped_window_decides_nothing() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            4242,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        let intent = console.decide_server_event(ServerEvent::Closed { id: 1 });
        assert_eq!(intent, None);
    }

    fn test_view() -> turbo_debug_console::session::SharedView {
        std::rc::Rc::new(std::cell::RefCell::new(StreamView::new(Rect::new(
            0, 0, 80, 24,
        ))))
    }

    #[test]
    fn copy_is_available_only_with_a_nonempty_selection() {
        let mut console = Console::default();
        console.sessions.insert(
            1,
            "demo".into(),
            0,
            StreamKind::Tokens,
            test_view(),
            RENDER_OPTIONS,
        );
        assert!(!console.can_copy(None), "no focused window");
        assert!(!console.can_copy(Some(1)), "nothing selected yet");

        console.sessions.feed(1, b"hello\n");
        console.sessions.select_all(1);
        assert!(
            console.can_copy(Some(1)),
            "select-all over content is copyable"
        );
    }
}

/// Headless regression test for the "window title bars render empty"
/// finding. `Application::new()` needs a real TTY, so this exercises
/// `tile_window_bounds` + `WindowBuilder` + `Desktop::add` + `Frame::draw`
/// directly against a `Terminal` running a `NullBackend`, which is enough
/// to reproduce and pin the bug: a window placed at the *exact* desktop
/// bounds gets pushed up by `constrain_to_limits` to make room for its
/// shadow, hiding the title bar off the top of the screen.
#[cfg(test)]
mod title_render_tests {
    use std::io;
    use std::time::Duration;

    use turbo_vision::core::event::Event;
    use turbo_vision::core::geometry::Rect;
    use turbo_vision::terminal::{Backend, Terminal};
    use turbo_vision::views::desktop::Desktop;
    use turbo_vision::views::view::View;
    use turbo_vision::views::window::WindowBuilder;

    struct NullBackend;
    impl Backend for NullBackend {
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn init(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn cleanup(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn size(&self) -> io::Result<(u16, u16)> {
            Ok((80, 25))
        }
        fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
            Ok(None)
        }
        fn write_raw(&mut self, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self, _x: u16, _y: u16) -> io::Result<()> {
            Ok(())
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A window whose bounds equal the tile rect returned by
    /// `tile_window_bounds` (shadow-shrunk) renders its title on row 0 —
    /// not pushed off screen.
    #[test]
    fn shadow_shrunk_bounds_keep_the_title_bar_on_screen() {
        let mut terminal = Terminal::with_backend(Box::new(NullBackend)).unwrap();
        let mut desktop = Desktop::new(Rect::new(0, 0, 80, 25));

        let mut bounds = desktop.bounds();
        let (shadow_x, shadow_y) = turbo_vision::core::state::shadow_size();
        bounds.b.x -= shadow_x;
        bounds.b.y -= shadow_y;

        let window = WindowBuilder::new()
            .bounds(bounds)
            .title("demo :61278")
            .build();
        desktop.add(Box::new(window));
        desktop.draw(&mut terminal);

        let row0: String = terminal.buffer()[0].iter().map(|c| c.ch).collect();
        assert!(row0.contains("demo :61278"), "title not on row 0: {row0:?}");
    }

    /// Without the shadow-shrink, the same window (bounds == full desktop)
    /// gets constrained up by one row to make room for its shadow, and the
    /// title bar (row 0 of the frame) scrolls off screen entirely. This
    /// pins the actual root cause: not `Frame`/`set_title`, but window
    /// sizing versus the shadow.
    #[test]
    fn unshrunk_bounds_push_the_title_bar_off_screen() {
        let mut terminal = Terminal::with_backend(Box::new(NullBackend)).unwrap();
        let mut desktop = Desktop::new(Rect::new(0, 0, 80, 25));

        let window = WindowBuilder::new()
            .bounds(desktop.bounds())
            .title("demo :61278")
            .build();
        desktop.add(Box::new(window));
        desktop.draw(&mut terminal);

        let row0: String = terminal.buffer()[0].iter().map(|c| c.ch).collect();
        assert!(
            !row0.contains("demo :61278"),
            "expected the unshrunk-bounds title to be scrolled off row 0, but found it: {row0:?}"
        );
    }

    /// Regression test for the "`StreamView` drawn over the frame" bug:
    /// `StreamView::new(app.get_tile_rect())` handed the view the window's
    /// *outer* bounds, so `Group::add` (relative -> absolute) placed it at
    /// the window's own origin, on top of the frame. A view added to a
    /// window's interior must be given bounds relative to the interior —
    /// `(0, 0, w - 2, h - 2)` for a window of width `w`, height `h` — so
    /// that after `Window::add` its absolute bounds land one row/column
    /// inside the frame on every side.
    #[test]
    fn stream_view_bounds_land_inside_the_frame_not_over_it() {
        use turbo_debug_console::streamview::StreamView;

        let window_bounds = Rect::new(0, 0, 40, 20);
        let view_bounds = super::session_view_bounds(window_bounds);
        // Interior-relative: must start at the origin, not the window's.
        assert_eq!(view_bounds, Rect::new(0, 0, 38, 18));

        let mut window = WindowBuilder::new()
            .bounds(window_bounds)
            .title("demo")
            .build();
        window.add(Box::new(StreamView::new(view_bounds)));

        // After Group::add's relative -> absolute conversion, the child's
        // absolute bounds must be the window bounds inset by one cell of
        // frame on every side (Rect::b is exclusive), not the window's
        // outer bounds.
        let absolute = window.child_at(0).bounds();
        assert_eq!(absolute, Rect::new(1, 1, 39, 19));
    }

    /// Regression test for the actual pre-fix bug: passing the window's
    /// *outer* bounds (unshrunk) as the child's relative bounds. `Group::add`
    /// still offsets the child's top-left by the interior's own origin
    /// (`(1, 1)` here), so the left/top edge lands correctly by
    /// coincidence — the bug is that the child's bottom-right is then two
    /// cells too large in each dimension, so it overlaps the frame's right
    /// and bottom border instead of stopping at the interior's edge.
    #[test]
    fn outer_bounds_overflow_the_interior_by_the_frame_width() {
        use turbo_debug_console::streamview::StreamView;

        let window_bounds = Rect::new(0, 0, 40, 20);
        let mut window = WindowBuilder::new()
            .bounds(window_bounds)
            .title("demo")
            .build();
        window.add(Box::new(StreamView::new(window_bounds)));

        let absolute = window.child_at(0).bounds();
        // The interior is (1, 1, 39, 19); the buggy child bounds instead
        // reach to (41, 21) — two cells past the interior on each side.
        assert_eq!(absolute, Rect::new(1, 1, 41, 21));
        assert!(
            absolute.b.x > 39 && absolute.b.y > 19,
            "expected the unfixed bounds to overflow the interior (1,1,39,19), got {absolute:?}"
        );
    }
}

/// Reproduces the two-window overlap bug end to end -- the real `Window` +
/// `Desktop` compositing (not just a bare `StreamView` painting into a bare
/// `Terminal`, which does not reproduce it; see `streamview.rs`'s own
/// `a_covering_window_s_flush_fully_blanks_a_row_that_held_a_wide_character`,
/// which passes against unfixed code). A lower window's content contains
/// plank's real tool-call banner glyph on a row beyond the upper window's
/// own two lines; the upper window opens on top at the same bounds (as
/// `tile_window_bounds` hands every new window, no auto-tiling). One
/// `Desktop::draw` + `Terminal::flush` cycle (matching `main`'s per-tick
/// `app.draw()` + one flush) must leave that row fully blank -- nothing
/// from the window underneath may still show through.
#[cfg(test)]
mod window_overlap_tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use turbo_debug_console::session::SharedStreamView;
    use turbo_debug_console::streamview::StreamView;
    use turbo_vision::core::draw::Cell;
    use turbo_vision::core::event::Event;
    use turbo_vision::core::geometry::Rect;
    use turbo_vision::core::palette::{Attr, TvColor};
    use turbo_vision::terminal::{Backend, Terminal};
    use turbo_vision::views::desktop::Desktop;
    use turbo_vision::views::view::View;
    use turbo_vision::views::window::WindowBuilder;

    /// The exact banner glyph plank emits: U+1F6E0 HAMMER AND WRENCH
    /// followed by U+FE0F VARIATION SELECTOR-16.
    const WRENCH: &str = "\u{1F6E0}\u{FE0F}";

    fn line(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|c| Cell::new(c, Attr::new(TvColor::LightGray, TvColor::Black)))
            .collect()
    }

    /// Records every byte `Terminal::flush` sends downstream, so a test can
    /// inspect what actually reaches a real terminal (the diffed,
    /// escape-coded re-encode of the cell buffer) rather than only the
    /// in-memory buffer `write_line` always updates unconditionally.
    #[derive(Clone, Default)]
    struct RecordingBackend {
        width: u16,
        height: u16,
        output: Arc<Mutex<Vec<u8>>>,
    }

    impl Backend for RecordingBackend {
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn init(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn cleanup(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn size(&self) -> io::Result<(u16, u16)> {
            Ok((self.width, self.height))
        }
        fn poll_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
            Ok(None)
        }
        fn write_raw(&mut self, data: &[u8]) -> io::Result<()> {
            self.output.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self, _x: u16, _y: u16) -> io::Result<()> {
            Ok(())
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Replays `flush`'s escape-coded byte stream onto a plain grid the way
    /// a real terminal would: `ESC[row;colH` repositions the cursor
    /// (1-indexed), an SGR color sequence is consumed and ignored, and
    /// every other character is placed at the cursor and advances it by
    /// its own display width, exactly as a real terminal renders it.
    fn replay_onto_grid(bytes: &[u8], grid: &mut [Vec<char>]) {
        use unicode_width::UnicodeWidthChar;
        let text = std::str::from_utf8(bytes).expect("flush emits valid UTF-8");
        let mut chars = text.chars().peekable();
        let mut row = 0usize;
        let mut col = 0usize;
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte = ' ';
                for pc in chars.by_ref() {
                    if pc.is_ascii_digit() || pc == ';' {
                        params.push(pc);
                    } else {
                        final_byte = pc;
                        break;
                    }
                }
                if final_byte == 'H' {
                    let mut parts = params.split(';');
                    let r: usize = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
                    let cix: usize = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
                    row = r.saturating_sub(1);
                    col = cix.saturating_sub(1);
                }
                continue;
            }
            let width = c.width().unwrap_or(0);
            if row < grid.len() && col < grid[row].len() {
                grid[row][col] = c;
            }
            col += width;
        }
    }

    #[test]
    fn a_new_window_s_flush_fully_covers_a_row_that_held_a_wide_character_underneath() {
        let output: Arc<Mutex<Vec<u8>>> = Arc::default();
        let backend = RecordingBackend {
            width: 40,
            height: 12,
            output: output.clone(),
        };
        let mut terminal = Terminal::with_backend(Box::new(backend)).unwrap();
        let mut desktop = Desktop::new(Rect::new(0, 0, 40, 12));
        let mut grid = vec![vec![' '; 40]; 12];

        // Window A: same bounds every new window gets (no auto-tiling) --
        // interior content: two blank lines, then the real banner line, so
        // the banner sits at interior row 2, beyond window B's two lines.
        let window_bounds = Rect::new(0, 0, 30, 8);
        let mut window_a = WindowBuilder::new()
            .bounds(window_bounds)
            .title("a")
            .build();
        let view_a = Rc::new(RefCell::new(StreamView::new(Rect::new(0, 0, 28, 6))));
        view_a.borrow_mut().push_line(&line(""));
        view_a.borrow_mut().push_line(&line(""));
        view_a
            .borrow_mut()
            .push_line(&line(&format!("{WRENCH} Reading src/dsml.rs 1:500...")));
        window_a.add(Box::new(SharedStreamView(view_a)));
        desktop.add(Box::new(window_a));

        desktop.draw(&mut terminal);
        terminal.flush().unwrap();
        replay_onto_grid(&output.lock().unwrap(), &mut grid);
        output.lock().unwrap().clear();

        // Window B: identical bounds, opens on top, only two short lines of
        // its own.
        let mut window_b = WindowBuilder::new()
            .bounds(window_bounds)
            .title("b")
            .build();
        let view_b = Rc::new(RefCell::new(StreamView::new(Rect::new(0, 0, 28, 6))));
        view_b.borrow_mut().push_line(&line("hi"));
        view_b.borrow_mut().push_line(&line("there"));
        window_b.add(Box::new(SharedStreamView(view_b)));
        desktop.add(Box::new(window_b));

        desktop.draw(&mut terminal);
        terminal.flush().unwrap();
        replay_onto_grid(&output.lock().unwrap(), &mut grid);

        // Interior row 2 (absolute row 1 + 2 = 3, since the window's
        // interior starts one row inside its own top-left) must be fully
        // blank in window B: nothing from window A's banner may bleed
        // through.
        let absolute_row = usize::try_from(window_bounds.a.y).unwrap() + 1 + 2;
        let interior_x0 = usize::try_from(window_bounds.a.x).unwrap() + 1;
        let interior_x1 = usize::try_from(window_bounds.b.x).unwrap() - 1;
        for col in interior_x0..interior_x1 {
            assert_eq!(
                grid[absolute_row][col], ' ',
                "row {absolute_row} column {col} still shows a leftover \
                 character from window A: {:?}",
                grid[absolute_row]
            );
        }
    }
}
