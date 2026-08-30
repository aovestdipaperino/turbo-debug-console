// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! One window per stream session.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use trace_stream::render::RenderOptions;
use turbo_vision::core::event::Event;
use turbo_vision::core::geometry::Rect;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::View;

use crate::pipeline::Pipeline;
use crate::proto::StreamKind;
use crate::registry::SessionId;
use crate::streamview::StreamView;
use crate::tracefmt::TraceRenderer;

/// A `StreamView` addressable from both the desktop and the event pump.
pub type SharedView = Rc<RefCell<StreamView>>;

/// Forwards `View` calls into a shared `StreamView`.
#[derive(Debug)]
pub struct SharedStreamView(pub SharedView);

impl View for SharedStreamView {
    fn bounds(&self) -> Rect {
        self.0.borrow().bounds()
    }
    fn set_bounds(&mut self, bounds: Rect) {
        self.0.borrow_mut().set_bounds(bounds);
    }
    fn draw(&mut self, terminal: &mut Terminal) {
        self.0.borrow_mut().draw(terminal);
    }
    fn handle_event(&mut self, event: &mut Event) {
        self.0.borrow_mut().handle_event(event);
    }
    fn can_focus(&self) -> bool {
        true
    }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}

/// A session's renderer: which one it holds depends on its [`StreamKind`].
/// A trace session has no [`Pipeline`] -- that pipeline is the markdown/DSML
/// renderer for model token streams, the wrong tool for a structured log
/// line, so none is ever constructed for one.
#[derive(Debug)]
enum Renderer {
    Tokens(Box<Pipeline>),
    Trace(TraceRenderer),
}

impl Renderer {
    fn feed(&mut self, bytes: &[u8], view: &mut StreamView) {
        match self {
            Self::Tokens(p) => p.feed(bytes, view),
            Self::Trace(t) => t.feed(bytes, view),
        }
    }

    fn finish(&mut self, view: &mut StreamView) {
        match self {
            Self::Tokens(p) => p.finish(view),
            Self::Trace(t) => t.finish(view),
        }
    }
}

/// Per-session state owned by the main loop.
#[derive(Debug)]
pub struct SessionState {
    pub name: String,
    pub port: u16,
    pub view: SharedView,
    pub kind: StreamKind,
    renderer: Renderer,
    pub connected: bool,
}

impl SessionState {
    /// Title text for this session's window. A trace session's kind is
    /// called out with a leading `[trace]` tag -- the `name :port` shape
    /// alone gives no hint that a window is rendering structured log
    /// records rather than a token stream, and that distinction matters
    /// enough at a glance to be worth the few extra characters.
    #[must_use]
    pub fn window_title(&self) -> String {
        let base = format_title(&self.name, self.port);
        let base = match self.kind {
            StreamKind::Tokens => base,
            StreamKind::Trace => format!("[trace] {base}"),
        };
        if self.connected {
            base
        } else {
            format!("{base} [disconnected]")
        }
    }

    /// Pushes stream bytes through this session's renderer and into its view.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut view = self.view.borrow_mut();
        self.renderer.feed(bytes, &mut view);
    }

    /// Ends the stream: flushes the renderer and any trailing partial line.
    pub fn finish(&mut self) {
        let mut view = self.view.borrow_mut();
        self.renderer.finish(&mut view);
    }
}

/// The `name :port` half of a window title, shared between the initial
/// title set when a window is created and `SessionState::window_title`'s
/// later connect/disconnect updates. Port 0 is not a real port — anonymous
/// sessions and opened captures use it as a sentinel — so it is omitted
/// rather than displayed as `name :0`.
#[must_use]
pub fn format_title(name: &str, port: u16) -> String {
    if port == 0 {
        name.to_string()
    } else {
        format!("{name} :{port}")
    }
}

/// All live sessions, keyed by id.
#[derive(Debug, Default)]
pub struct Sessions {
    inner: HashMap<SessionId, SessionState>,
}

impl Sessions {
    pub fn insert(
        &mut self,
        id: SessionId,
        name: String,
        port: u16,
        kind: StreamKind,
        view: SharedView,
        opts: RenderOptions,
    ) {
        let renderer = match kind {
            StreamKind::Tokens => Renderer::Tokens(Box::new(Pipeline::new(opts))),
            StreamKind::Trace => Renderer::Trace(TraceRenderer::new()),
        };
        self.inner.insert(
            id,
            SessionState {
                name,
                port,
                view,
                kind,
                renderer,
                connected: false,
            },
        );
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut SessionState> {
        self.inner.get_mut(&id)
    }

    pub fn remove(&mut self, id: SessionId) -> Option<SessionState> {
        self.inner.remove(&id)
    }

    /// Feeds bytes into a session's renderer and view.
    pub fn feed(&mut self, id: SessionId, data: &[u8]) {
        if let Some(s) = self.inner.get_mut(&id) {
            s.feed(data);
        }
    }

    /// Draws a horizontal rule announcing a reattached client.
    pub fn mark_reconnected(&mut self, id: SessionId) {
        if let Some(s) = self.inner.get_mut(&id) {
            s.connected = true;
            s.feed(b"\n-- reconnected --\n");
        }
    }

    /// Reflects a `ServerEvent::Attached`: always marks the session
    /// connected, but draws the "-- reconnected --" rule only for a
    /// genuine reattach (`reattached`), never for a brand-new session's
    /// first-ever attach — see defect 1 in
    /// `.superpowers/sdd/lifecycle-fixes-report.md`.
    pub fn mark_attached(&mut self, id: SessionId, reattached: bool) {
        if reattached {
            self.mark_reconnected(id);
        } else if let Some(s) = self.inner.get_mut(&id) {
            s.connected = true;
        }
    }

    pub fn mark_disconnected(&mut self, id: SessionId) {
        if let Some(s) = self.inner.get_mut(&id) {
            s.connected = false;
            s.finish();
        }
    }

    /// Empties one session's scrollback.
    pub fn clear(&mut self, id: SessionId) {
        if let Some(s) = self.inner.get_mut(&id) {
            s.view.borrow_mut().clear();
        }
    }

    /// One session's scrollback as plain text, for File > Save As.
    #[must_use]
    pub fn plain_text(&self, id: SessionId) -> Option<String> {
        self.inner.get(&id).map(|s| s.view.borrow().plain_text())
    }

    /// The title a session's window should currently show, for reflecting
    /// connect/disconnect state after the fact (the window itself is not
    /// reachable from here — the caller owns the desktop).
    #[must_use]
    pub fn window_title(&self, id: SessionId) -> Option<String> {
        self.inner.get(&id).map(SessionState::window_title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_stream::render::RenderOptions;

    fn opts() -> RenderOptions {
        RenderOptions {
            use_color: true,
            format_thinking: true,
            format_markdown: true,
        }
    }

    fn view() -> SharedView {
        Rc::new(RefCell::new(StreamView::new(Rect::new(0, 0, 80, 24))))
    }

    #[test]
    fn feed_reaches_the_session_view() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        sessions.feed(1, b"hello\n");
        assert!(sessions.plain_text(1).unwrap().contains("hello"));
    }

    #[test]
    fn window_title_reflects_connection_state() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        assert_eq!(
            sessions.window_title(1).unwrap(),
            "demo :4242 [disconnected]"
        );
        sessions.mark_reconnected(1);
        assert_eq!(sessions.window_title(1).unwrap(), "demo :4242");
        sessions.mark_disconnected(1);
        assert_eq!(
            sessions.window_title(1).unwrap(),
            "demo :4242 [disconnected]"
        );
    }

    #[test]
    fn window_title_omits_a_zero_port() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "anon-1".into(), 0, StreamKind::Tokens, view(), opts());
        assert_eq!(sessions.window_title(1).unwrap(), "anon-1 [disconnected]");
        sessions.mark_reconnected(1);
        assert_eq!(sessions.window_title(1).unwrap(), "anon-1");
    }

    /// Regression test for defect 1: a session's first-ever attach must
    /// mark it connected (title stops reading `[disconnected]`) without
    /// drawing the "-- reconnected --" rule — that rule announces a
    /// genuine rejoin, and would be wrong above the very first line of a
    /// brand-new session.
    #[test]
    fn mark_attached_first_attach_connects_without_a_rule() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        sessions.mark_attached(1, false);
        assert_eq!(sessions.window_title(1).unwrap(), "demo :4242");
        assert!(!sessions.plain_text(1).unwrap().contains("reconnected"));
    }

    /// A genuine reattach (`reattached: true`) both connects and draws the
    /// rule, same as `mark_reconnected`.
    #[test]
    fn mark_attached_reattach_connects_and_draws_a_rule() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        sessions.mark_attached(1, true);
        assert_eq!(sessions.window_title(1).unwrap(), "demo :4242");
        assert!(sessions.plain_text(1).unwrap().contains("reconnected"));
    }

    #[test]
    fn mark_reconnected_draws_a_horizontal_rule() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        sessions.mark_reconnected(1);
        assert!(sessions.plain_text(1).unwrap().contains("reconnected"));
    }

    #[test]
    fn clear_empties_the_scrollback() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        sessions.feed(1, b"hello\n");
        sessions.clear(1);
        assert_eq!(sessions.plain_text(1).unwrap(), "");
    }

    #[test]
    fn remove_drops_the_session() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "demo".into(), 4242, StreamKind::Tokens, view(), opts());
        assert!(sessions.remove(1).is_some());
        assert!(sessions.plain_text(1).is_none());
    }

    #[test]
    fn unknown_id_returns_none_everywhere() {
        let sessions = Sessions::default();
        assert!(sessions.plain_text(99).is_none());
        assert!(sessions.window_title(99).is_none());
    }

    #[test]
    fn a_trace_session_renders_through_tracefmt_not_the_pipeline() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "myapp".into(), 4242, StreamKind::Trace, view(), opts());
        sessions.feed(1, b"{\"level\":\"INFO\",\"fields\":{\"message\":\"hi\"}}\n");
        assert_eq!(sessions.plain_text(1).unwrap(), "INFO  hi");
    }

    #[test]
    fn a_trace_session_window_title_is_tagged() {
        let mut sessions = Sessions::default();
        sessions.insert(1, "myapp".into(), 4242, StreamKind::Trace, view(), opts());
        assert_eq!(
            sessions.window_title(1).unwrap(),
            "[trace] myapp :4242 [disconnected]"
        );
        sessions.mark_reconnected(1);
        assert_eq!(sessions.window_title(1).unwrap(), "[trace] myapp :4242");
    }
}
