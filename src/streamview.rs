// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! A scrollback view over styled cells, one `Vec<Cell>` per line.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use turbo_vision::core::draw::{Cell, DrawBuffer};
use turbo_vision::core::event::{
    Event, EventType, KB_DOWN, KB_END, KB_HOME, KB_PGDN, KB_PGUP, KB_UP,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{View, write_line_to_terminal};

/// A base character immediately followed by U+FE0F (the emoji presentation
/// selector, VS-16) or U+FE0E (the text presentation selector, VS-15) forms
/// one *presentation sequence* whose combined width can differ from the
/// base character's own width in isolation. This is exactly the shape of
/// plank's tool-call banner glyph (`🛠️` = U+1F6E0 + U+FE0F): the bare
/// wrench is East-Asian-Width `Neutral` (width 1), but the fully-qualified
/// emoji sequence the model actually emits is double-width. `unicode_width`
/// only resolves this at the *string* level (`UnicodeWidthStr`), not per
/// `char`, so a two-character lookahead is required to catch it — this is
/// still the crate doing the Unicode-correctness work; nothing here is a
/// hand-rolled codepoint table.
const PRESENTATION_SELECTORS: [char; 2] = ['\u{FE0F}', '\u{FE0E}'];

/// Normalizes a naive, one-`Cell`-per-`char` line into one `Cell` per
/// terminal *column* — the invariant every other method in this module
/// relies on (row width, the horizontal `left` clip, and `draw`'s column
/// count).
///
/// A double-width character (an emoji, a CJK glyph) keeps its real `char`
/// in the first cell and gets a filler cell for each additional column,
/// mirroring `turbo_vision`'s own `DrawBuffer::move_str` convention: the
/// terminal's cell-diffing flush already knows to skip a `'\0'` when
/// encoding output, so an invented filler paints as blank if ever exposed
/// (e.g. a horizontal scroll cutting a wide character in half) rather than
/// emitting half a glyph. When the second column instead comes from a real
/// trailing presentation selector, that selector's own character is kept
/// as the filler — it is a genuine, zero-advance character, not a padding
/// artifact, so `plain_text` must still hand it back on Save As.
///
/// A zero-width character (a combining mark, a selector whose sequence
/// collapses to width 0) occupies no column and is dropped — again
/// matching `move_str`, and this module's only way to keep the stored
/// column count equal to the true rendered width without a codepoint-range
/// table of our own.
///
/// Idempotent: a spacer cell (`ch == '\0'`) already produced by a previous
/// call passes through unchanged, so re-normalizing already-normalized
/// cells (e.g. lines rebuilt from `styled_lines()`) is harmless.
fn normalize_line(cells: &[Cell]) -> Vec<Cell> {
    let mut out = Vec::with_capacity(cells.len());
    let mut i = 0;
    while i < cells.len() {
        let cell = cells[i];
        if cell.ch == '\0' {
            out.push(cell);
            i += 1;
            continue;
        }

        let next = cells.get(i + 1).copied();
        let selector = next.filter(|n| PRESENTATION_SELECTORS.contains(&n.ch));

        let width = if let Some(sel) = selector {
            let mut seq = String::with_capacity(cell.ch.len_utf8() + sel.ch.len_utf8());
            seq.push(cell.ch);
            seq.push(sel.ch);
            seq.width()
        } else {
            cell.ch.width().unwrap_or(0)
        };

        if width == 0 {
            i += if selector.is_some() { 2 } else { 1 };
            continue;
        }

        out.push(cell);
        if let Some(sel) = selector {
            out.push(sel);
            for _ in 2..width {
                out.push(Cell::new('\0', cell.attr));
            }
            i += 2;
        } else {
            for _ in 1..width {
                out.push(Cell::new('\0', cell.attr));
            }
            i += 1;
        }
    }
    out
}

/// Default scrollback depth.
pub const DEFAULT_MAX_LINES: usize = 10_000;

/// A scrollback of styled lines, with autoscroll that releases when the user
/// scrolls back and re-arms at the bottom.
#[derive(Debug)]
pub struct StreamView {
    bounds: Rect,
    /// Completed lines, oldest first.
    lines: Vec<Vec<Cell>>,
    /// The line currently streaming in, not yet terminated by a newline.
    partial: Option<Vec<Cell>>,
    max_lines: usize,
    /// Index of the topmost displayed line.
    top: usize,
    /// Horizontal scroll offset, in cells.
    left: usize,
    /// True while the view follows the tail.
    follow: bool,
    fill: Attr,
}

impl StreamView {
    #[must_use]
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            lines: Vec::new(),
            partial: None,
            max_lines: DEFAULT_MAX_LINES,
            top: 0,
            left: 0,
            follow: true,
            fill: Attr::new(TvColor::LightGray, TvColor::Black),
        }
    }

    pub fn set_max_lines(&mut self, n: usize) {
        self.max_lines = n.max(1);
        self.trim();
    }

    /// Appends a completed line.
    pub fn push_line(&mut self, cells: &[Cell]) {
        self.lines.push(normalize_line(cells));
        self.trim();
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    /// Replaces the in-progress line. Called on every repaint while a line is
    /// still streaming, so it must overwrite rather than append.
    pub fn set_partial(&mut self, cells: &[Cell]) {
        let cells = normalize_line(cells);
        self.partial = if cells.is_empty() { None } else { Some(cells) };
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.partial = None;
        self.top = 0;
        self.left = 0;
        self.follow = true;
    }

    /// Total displayed lines, including the in-progress one.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len() + usize::from(self.partial.is_some())
    }

    /// Visible rows, i.e. the view height.
    fn page(&self) -> usize {
        usize::try_from(self.bounds.height()).unwrap_or(0).max(1)
    }

    fn max_top(&self) -> usize {
        self.line_count().saturating_sub(self.page())
    }

    pub fn scroll_to_bottom(&mut self) {
        self.top = self.max_top();
        self.follow = true;
    }

    pub fn scroll_to_top(&mut self) {
        self.top = 0;
        self.follow = false;
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.top = self.top.saturating_sub(n);
        self.follow = false;
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.top = (self.top + n).min(self.max_top());
        self.follow = self.top == self.max_top();
    }

    #[must_use]
    pub fn is_at_bottom(&self) -> bool {
        self.follow
    }

    /// The whole scrollback with attributes stripped, for File > Save As.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.iter_lines().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            // Spacer cells (the second column of a wide char) carry no
            // text of their own; skip them so the saved text round-trips
            // the original characters with no padding artifacts.
            out.extend(line.iter().map(|c| c.ch).filter(|&ch| ch != '\0'));
        }
        out
    }

    /// The whole scrollback with attributes intact, for tests and golden files.
    #[must_use]
    pub fn styled_lines(&self) -> Vec<Vec<Cell>> {
        self.iter_lines().cloned().collect()
    }

    fn iter_lines(&self) -> impl Iterator<Item = &Vec<Cell>> {
        self.lines.iter().chain(self.partial.iter())
    }

    fn trim(&mut self) {
        if self.lines.len() > self.max_lines {
            let drop = self.lines.len() - self.max_lines;
            self.lines.drain(..drop);
            self.top = self.top.saturating_sub(drop);
        }
    }
}

impl View for StreamView {
    fn bounds(&self) -> Rect {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
        if self.follow {
            self.scroll_to_bottom();
        } else {
            self.top = self.top.min(self.max_top());
        }
    }

    fn draw(&mut self, terminal: &mut Terminal) {
        if self.bounds.height() <= 0 {
            return;
        }
        let width = usize::try_from(self.bounds.width()).unwrap_or(0);
        let page = self.page();
        let lines: Vec<&Vec<Cell>> = self.iter_lines().skip(self.top).take(page).collect();

        for row in 0..page {
            let mut buf = DrawBuffer::new(width);
            for i in 0..width {
                buf.put_char(i, ' ', self.fill);
            }
            if let Some(line) = lines.get(row) {
                for (i, cell) in line.iter().skip(self.left).take(width).enumerate() {
                    buf.put_char(i, cell.ch, cell.attr);
                }
            }
            let y = self.bounds.a.y + i16::try_from(row).unwrap_or(i16::MAX);
            write_line_to_terminal(terminal, self.bounds.a.x, y, &buf);
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        if event.what != EventType::Keyboard {
            return;
        }
        let page = self.page();
        match event.key_code {
            KB_UP => self.scroll_up(1),
            KB_DOWN => self.scroll_down(1),
            KB_PGUP => self.scroll_up(page),
            KB_PGDN => self.scroll_down(page),
            KB_HOME => self.scroll_to_top(),
            KB_END => self.scroll_to_bottom(),
            _ => return,
        }
        event.clear();
    }

    fn can_focus(&self) -> bool {
        true
    }

    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        // Cells already carry resolved `Attr`s (from `AnsiLineAssembler`), so
        // there is no logical-color index for a palette to remap.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::Duration;
    use turbo_vision::core::palette::TvColor;
    use turbo_vision::terminal::Backend;

    fn line(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|c| Cell::new(c, Attr::new(TvColor::LightGray, TvColor::Black)))
            .collect()
    }

    fn view() -> StreamView {
        StreamView::new(Rect::new(0, 0, 40, 10))
    }

    /// An in-memory `Backend` for tests: no real TTY, fixed size, no I/O.
    /// `Terminal::write_line`/`write_cell` write straight into `Terminal`'s
    /// own in-memory buffer, so this stub only needs to satisfy
    /// initialization and size queries for `Terminal::with_backend`.
    struct FakeBackend {
        width: u16,
        height: u16,
    }

    impl Backend for FakeBackend {
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

    fn fake_terminal(width: u16, height: u16) -> Terminal {
        Terminal::with_backend(Box::new(FakeBackend { width, height }))
            .expect("fake backend never fails to init")
    }

    /// A `Backend` that records every byte `Terminal::flush` actually sends
    /// downstream, via a shared buffer -- the write-through path
    /// `FakeBackend` above stubs out. `Terminal::flush` is the one place
    /// that decides what physically reaches a real terminal (it does a
    /// diffed, escape-coded re-encode of the cell buffer, and knowingly
    /// skips `'\0'` filler cells), so a bug specific to *that* encoding is
    /// invisible to any test that only inspects `Terminal::read_cell`,
    /// which reflects the in-memory cell buffer `write_line` always
    /// updates unconditionally.
    #[derive(Clone, Default)]
    struct RecordingBackend {
        width: u16,
        height: u16,
        output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
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

    /// Builds a `Terminal` whose every `flush`-emitted byte lands in the
    /// returned buffer, so a test can inspect what actually reaches a real
    /// terminal rather than only the in-memory cell buffer.
    fn recording_terminal(
        width: u16,
        height: u16,
    ) -> (Terminal, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = RecordingBackend {
            width,
            height,
            output: output.clone(),
        };
        let terminal =
            Terminal::with_backend(Box::new(backend)).expect("fake backend never fails to init");
        (terminal, output)
    }

    /// Replays `flush`'s escape-coded byte stream onto a plain grid the way
    /// a real terminal would: `ESC[row;colH` repositions the cursor
    /// (1-indexed), an SGR color sequence is consumed and ignored, and every
    /// other character is placed at the cursor and advances it by its own
    /// display width -- 2 for a double-width glyph, 0 for a combining or
    /// selector character, exactly as a real terminal renders it (not by
    /// our internal one-`Cell`-per-logical-column bookkeeping, which is
    /// precisely what could drift from physical reality). Bytes from
    /// successive flushes are replayed in order onto the same grid, since a
    /// real terminal's screen persists across flushes the same way.
    fn replay_onto_grid(bytes: &[u8], grid: &mut [Vec<char>]) {
        let text = std::str::from_utf8(bytes).expect("flush emits valid UTF-8");
        let mut chars = text.chars().peekable();
        let mut row = 0usize;
        let mut col = 0usize;
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next(); // consume '['
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
                // An SGR ('m') sequence carries no cursor movement.
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
    fn scrollback_cap_drops_oldest_lines() {
        let mut v = view();
        v.set_max_lines(3);
        for i in 0..5 {
            v.push_line(&line(&i.to_string()));
        }
        assert_eq!(v.line_count(), 3);
        assert_eq!(v.plain_text(), "2\n3\n4");
    }

    #[test]
    fn autoscroll_holds_at_bottom_while_lines_arrive() {
        let mut v = view();
        for i in 0..50 {
            v.push_line(&line(&i.to_string()));
        }
        assert!(v.is_at_bottom());
    }

    #[test]
    fn scrolling_up_releases_autoscroll_and_end_rearms_it() {
        let mut v = view();
        for i in 0..50 {
            v.push_line(&line(&i.to_string()));
        }
        v.scroll_up(5);
        assert!(!v.is_at_bottom());
        v.push_line(&line("new"));
        assert!(
            !v.is_at_bottom(),
            "a new line must not yank a scrolled-back reader to the bottom"
        );
        v.scroll_to_bottom();
        assert!(v.is_at_bottom());
    }

    #[test]
    fn partial_line_is_replaced_not_appended() {
        let mut v = view();
        v.set_partial(&line("par"));
        v.set_partial(&line("part"));
        assert_eq!(v.plain_text(), "part");
        assert_eq!(v.line_count(), 1);
    }

    #[test]
    fn plain_text_strips_attributes() {
        let mut v = view();
        v.push_line(&[Cell::new('x', Attr::new(TvColor::LightRed, TvColor::Blue))]);
        assert_eq!(v.plain_text(), "x");
    }

    #[test]
    fn resize_larger_while_scrolled_back_reclamps_top_to_show_a_full_page() {
        let mut v = StreamView::new(Rect::new(0, 0, 40, 5));
        for i in 0..50 {
            v.push_line(&line(&i.to_string()));
        }
        // Scroll back so `top` sits well below the current max_top()
        // (line_count 50, page 5 -> max_top 45).
        v.scroll_to_top();
        v.scroll_down(40);
        assert!(!v.is_at_bottom());
        let old_top = v.top;
        assert!(old_top < v.max_top());

        // Grow the view a lot: max_top() shrinks to line_count - new_page
        // (50 - 48 = 2), which is now well below the old `top` (40). Left
        // unclamped, that would leave blank rows at the bottom of the
        // viewport even though unshown history sits above.
        v.set_bounds(Rect::new(0, 0, 40, 48));

        assert!(
            v.top <= v.max_top(),
            "top ({}) must not exceed max_top ({}) after growing",
            v.top,
            v.max_top()
        );
        let lines: Vec<&Vec<Cell>> = v.iter_lines().skip(v.top).take(v.page()).collect();
        assert_eq!(
            lines.len(),
            v.page().min(v.line_count()),
            "a full page of content should be visible after growing"
        );
    }

    #[test]
    fn draw_clips_to_bounds_and_applies_horizontal_offset() {
        let mut v = StreamView::new(Rect::new(2, 1, 8, 4));
        v.push_line(&line("abcdefghij")); // longer than the 6-wide view
        v.push_line(&line("short")); // shorter than the 6-wide view
        v.left = 2; // horizontal scroll offset

        let mut terminal = fake_terminal(20, 10);
        v.draw(&mut terminal);

        // Row 0 (bounds.a.y == 1): "abcdefghij" skipped by `left` 2, then
        // clipped to width 6, drawn starting at bounds.a.x == 2.
        for (i, expected) in "cdefgh".chars().enumerate() {
            let cell = terminal
                .read_cell(2 + i16::try_from(i).unwrap_or(i16::MAX), 1)
                .expect("cell within terminal bounds");
            assert_eq!(cell.ch, expected);
        }
        // Nothing is drawn past the view's width (x == 8 is out of bounds).
        assert_eq!(terminal.read_cell(8, 1).unwrap().ch, ' ');

        // Row 1 (bounds.a.y == 2): "short" skipped by `left` 2 -> "ort",
        // padded with the fill space for the remaining 3 columns.
        for (i, expected) in "ort   ".chars().enumerate() {
            let cell = terminal
                .read_cell(2 + i16::try_from(i).unwrap_or(i16::MAX), 2)
                .expect("cell within terminal bounds");
            assert_eq!(cell.ch, expected);
        }

        // Nothing above or below the view's rows was touched.
        assert_eq!(terminal.read_cell(2, 0).unwrap().ch, ' ');
    }

    /// The exact banner glyph plank emits: U+1F6E0 HAMMER AND WRENCH followed
    /// by U+FE0F VARIATION SELECTOR-16 (the emoji presentation selector).
    /// The base character alone is East-Asian-Width `Neutral` (width 1 per
    /// `unicode-width`'s plain per-`char` rule) -- it is only the
    /// *emoji presentation sequence* (base + U+FE0F) that is double-width,
    /// which is exactly the sequence a real tool-call banner sends and the
    /// case this fix targets. `line()` builds one `Cell` per `char` here
    /// too, since `.chars()` splits the base and the selector into two
    /// separate `char`s -- the same shape `tracefmt`'s `cells()` produces.
    const WRENCH: &str = "\u{1F6E0}\u{FE0F}";

    #[test]
    fn wide_character_row_paints_the_correct_total_number_of_columns() {
        // wrench (2 columns) + space + x = 4 columns total.
        let mut v = StreamView::new(Rect::new(0, 0, 10, 4));
        v.push_line(&line(&format!("{WRENCH} x")));
        let mut terminal = fake_terminal(20, 10);
        v.draw(&mut terminal);

        // Column 0 holds the wrench glyph itself.
        assert_eq!(terminal.read_cell(0, 0).unwrap().ch, '\u{1F6E0}');
        // Column 1 is the wrench's second column: the trailing presentation
        // selector itself, kept (not an invented '\0') because it is a real
        // character.
        assert_eq!(terminal.read_cell(1, 0).unwrap().ch, '\u{FE0F}');
        // The rest of the row lands at its true, width-aware columns.
        assert_eq!(terminal.read_cell(2, 0).unwrap().ch, ' ');
        assert_eq!(terminal.read_cell(3, 0).unwrap().ch, 'x');
        // And the row is blank-padded for the remaining columns of the view.
        for x in 4..10 {
            assert_eq!(terminal.read_cell(x, 0).unwrap().ch, ' ');
        }
    }

    #[test]
    fn text_after_a_wide_character_lands_at_the_right_column() {
        let mut v = StreamView::new(Rect::new(0, 0, 30, 4));
        v.push_line(&line(&format!("{WRENCH} Reading src/dsml.rs")));
        let mut terminal = fake_terminal(30, 10);
        v.draw(&mut terminal);

        let expected = "\u{1F6E0}\u{FE0F} Reading src/dsml.rs";
        for (i, expected_ch) in expected.chars().enumerate() {
            let cell = terminal
                .read_cell(i16::try_from(i).unwrap(), 0)
                .expect("cell within terminal bounds");
            assert_eq!(cell.ch, expected_ch, "column {i} mismatch");
        }
    }

    #[test]
    fn short_row_is_blank_padded_so_nothing_shows_through_from_beneath() {
        let mut v = StreamView::new(Rect::new(0, 0, 10, 4));
        // First paint a row that fills the whole width...
        v.push_line(&line("XXXXXXXXXX"));
        let mut terminal = fake_terminal(20, 10);
        v.draw(&mut terminal);
        // ...then a shorter, width-shrinking row should overwrite every
        // column the first row touched, leaving nothing behind.
        v.clear();
        v.push_line(&line(&format!("{WRENCH}hi")));
        v.draw(&mut terminal);

        assert_eq!(terminal.read_cell(0, 0).unwrap().ch, '\u{1F6E0}');
        assert_eq!(terminal.read_cell(1, 0).unwrap().ch, '\u{FE0F}');
        assert_eq!(terminal.read_cell(2, 0).unwrap().ch, 'h');
        assert_eq!(terminal.read_cell(3, 0).unwrap().ch, 'i');
        for x in 4..10 {
            assert_eq!(
                terminal.read_cell(x, 0).unwrap().ch,
                ' ',
                "column {x} must be blanked, not left over from the previous row"
            );
        }
    }

    #[test]
    fn horizontal_offset_clips_correctly_when_a_wide_character_straddles_the_cut() {
        let mut v = StreamView::new(Rect::new(0, 0, 6, 4));
        v.push_line(&line(&format!("ab{WRENCH}cd"))); // columns: a b [wrench] [spacer] c d
        v.left = 2; // skip "ab", landing on the wrench's first column
        let mut terminal = fake_terminal(20, 10);
        v.draw(&mut terminal);

        assert_eq!(terminal.read_cell(0, 0).unwrap().ch, '\u{1F6E0}');
        assert_eq!(terminal.read_cell(1, 0).unwrap().ch, '\u{FE0F}');
        assert_eq!(terminal.read_cell(2, 0).unwrap().ch, 'c');
        assert_eq!(terminal.read_cell(3, 0).unwrap().ch, 'd');

        // Now offset by 3, landing squarely on the spacer half of the
        // wrench: only the lone selector character (zero-advance-width in
        // any real terminal, so it paints as blank on its own) shows at
        // column 0, never half a glyph, and everything after it must still
        // be at its true column.
        v.left = 3;
        let mut terminal2 = fake_terminal(20, 10);
        v.draw(&mut terminal2);
        assert_eq!(terminal2.read_cell(0, 0).unwrap().ch, '\u{FE0F}');
        assert_eq!(terminal2.read_cell(1, 0).unwrap().ch, 'c');
        assert_eq!(terminal2.read_cell(2, 0).unwrap().ch, 'd');
    }

    #[test]
    fn plain_text_round_trips_a_wide_character_with_no_padding_artifacts() {
        let mut v = view();
        v.push_line(&line(&format!("{WRENCH} Reading src/dsml.rs")));
        assert_eq!(v.plain_text(), format!("{WRENCH} Reading src/dsml.rs"));
    }

    /// Reproduces the real, two-window bug: a lower window paints a row
    /// containing plank's real tool-call banner glyph and *flushes* it (not
    /// just `draw`s it -- the defect lives in what `Terminal::flush` sends
    /// downstream, invisible to any test that only checks
    /// `Terminal::read_cell`, since `write_line` updates the in-memory cell
    /// buffer unconditionally regardless of what flush later encodes). A
    /// second, unrelated window then opens on top with the same bounds and
    /// paints an all-blank row over the identical region, and flushes too.
    /// A real terminal's screen must show nothing left over from the first
    /// window afterwards.
    #[test]
    fn a_covering_window_s_flush_fully_blanks_a_row_that_held_a_wide_character() {
        let (mut terminal, output) = recording_terminal(30, 4);
        let mut grid = vec![vec![' '; 30]; 4];

        // Lower window: the real banner line at row 0, drawn and flushed.
        let mut lower = StreamView::new(Rect::new(0, 0, 30, 4));
        lower.push_line(&line(&format!("{WRENCH} Reading src/dsml.rs 1:500...")));
        lower.draw(&mut terminal);
        terminal
            .flush()
            .expect("flush never fails against a fake backend");
        replay_onto_grid(&output.lock().unwrap(), &mut grid);
        output.lock().unwrap().clear();

        // Upper window: same bounds, no content of its own at all -- opens
        // on top and must blank every column of row 0 that the lower
        // window's banner occupied.
        let mut upper = StreamView::new(Rect::new(0, 0, 30, 4));
        upper.draw(&mut terminal);
        terminal
            .flush()
            .expect("flush never fails against a fake backend");
        replay_onto_grid(&output.lock().unwrap(), &mut grid);

        // Row 0 must now be fully blank -- nothing from the lower window's
        // banner may still show through.
        for (col, &ch) in grid[0].iter().enumerate() {
            assert_eq!(
                ch, ' ',
                "row 0 column {col} still shows a leftover character from \
                 the window underneath: {grid:?}"
            );
        }
    }

    #[test]
    fn draw_on_zero_height_view_writes_nothing() {
        let mut v = StreamView::new(Rect::new(0, 0, 10, 0));
        v.push_line(&line("hello"));
        let mut terminal = fake_terminal(20, 10);
        v.draw(&mut terminal);
        for y in 0..10 {
            for x in 0..20 {
                assert_eq!(
                    terminal.read_cell(x, y).unwrap().ch,
                    ' ',
                    "zero-height view must not write any cell"
                );
            }
        }
    }
}
