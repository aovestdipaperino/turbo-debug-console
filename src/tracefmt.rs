// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! Renders `tracing-subscriber` JSON-lines records to styled [`Cell`]s.
//!
//! This is a separate, self-contained renderer for the `trace` stream kind —
//! it does not go through `trace-stream`/`Pipeline`; that pipeline is a
//! markdown/DSML renderer for model token streams and is the wrong tool for
//! a structured log line.
//!
//! One input line (JSON or not) becomes one output line:
//!
//! ```text
//! 12:04:01  WARN  myapp::db   retry  attempt=3
//! └ dim ┘  └level┘ └ cyan ─┘  └msg┘  └ dim ┘
//! ```
//!
//! A line that fails to parse as JSON is rendered verbatim in the default
//! attribute rather than dropped or reported as an error — a producer will
//! eventually emit a panic message or a stray `println!`, and losing it
//! would be worse than showing it unstyled.

use serde_json::Value;
use turbo_vision::core::draw::Cell;
use turbo_vision::core::palette::{Attr, TvColor};

use crate::streamview::StreamView;

const BG: TvColor = TvColor::Black;
const DEFAULT_FG: TvColor = TvColor::LightGray;
const DIM_FG: TvColor = TvColor::DarkGray;
const TARGET_FG: TvColor = TvColor::LightCyan;

/// The color a level renders in. Levels arrive uppercase from
/// `tracing-subscriber`; accepted case-insensitively anyway.
fn level_color(level: &str) -> Option<TvColor> {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => Some(TvColor::LightRed),
        "WARN" => Some(TvColor::Yellow),
        "INFO" => Some(TvColor::White),
        "DEBUG" => Some(TvColor::LightGray),
        "TRACE" => Some(TvColor::DarkGray),
        _ => None,
    }
}

fn cells(text: &str, fg: TvColor) -> Vec<Cell> {
    text.chars()
        .map(|c| Cell::new(c, Attr::new(fg, BG)))
        .collect()
}

/// If `ts` looks like an RFC3339 timestamp (`YYYY-MM-DDTHH:MM:SS...`),
/// returns just its time-of-day (`HH:MM:SS`) — a debug console shows a burst
/// of records seconds apart, and a full date on every line is noise.
/// Anything else is not recognized, so the caller shows it verbatim.
fn time_of_day(ts: &str) -> Option<&str> {
    let b = ts.as_bytes();
    if b.len() < 19 || b[10] != b'T' {
        return None;
    }
    let digits = |i: usize| b[i].is_ascii_digit();
    let date_ok = (0..4).all(digits)
        && b[4] == b'-'
        && (5..7).all(digits)
        && b[7] == b'-'
        && (8..10).all(digits);
    let time_ok = (11..13).all(digits)
        && b[13] == b':'
        && (14..16).all(digits)
        && b[16] == b':'
        && (17..19).all(digits);
    if date_ok && time_ok {
        Some(&ts[11..19])
    } else {
        None
    }
}

/// Renders one record's `fields.key=value` value, unquoting a plain string
/// so it reads as logfmt (`attempt=3`, `user=alice`) rather than
/// double-quoted JSON.
fn field_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Renders one line of a trace stream: a JSON `tracing-subscriber` record,
/// or -- if the line is not valid JSON, or lacks a recognized `level` --
/// verbatim in the default attribute.
#[must_use]
pub fn render_line(line: &str) -> Vec<Cell> {
    render_record(line).unwrap_or_else(|| cells(line, DEFAULT_FG))
}

fn render_record(line: &str) -> Option<Vec<Cell>> {
    let value: Value = serde_json::from_str(line).ok()?;
    let level = value.get("level")?.as_str()?;
    let level_fg = level_color(level)?;

    let mut out: Vec<Cell> = Vec::new();
    let push_sep = |out: &mut Vec<Cell>| {
        if !out.is_empty() {
            out.extend(cells("  ", DEFAULT_FG));
        }
    };

    if let Some(ts) = value.get("timestamp").and_then(Value::as_str) {
        let shown = time_of_day(ts).unwrap_or(ts);
        push_sep(&mut out);
        out.extend(cells(shown, DIM_FG));
    }

    push_sep(&mut out);
    out.extend(cells(&level.to_ascii_uppercase(), level_fg));

    if let Some(target) = value.get("target").and_then(Value::as_str) {
        push_sep(&mut out);
        out.extend(cells(target, TARGET_FG));
    }

    if let Some(fields) = value.get("fields").and_then(Value::as_object) {
        if let Some(message) = fields.get("message").and_then(Value::as_str) {
            push_sep(&mut out);
            out.extend(cells(message, DEFAULT_FG));
        }

        let extra: Vec<String> = fields
            .iter()
            .filter(|(k, _)| *k != "message")
            .map(|(k, v)| format!("{k}={}", field_value(v)))
            .collect();
        if !extra.is_empty() {
            push_sep(&mut out);
            out.extend(cells(&extra.join(" "), DIM_FG));
        }
    }

    Some(out)
}

/// Buffers a trace session's incoming bytes into lines and renders each one
/// into the view, exactly as [`crate::pipeline::Pipeline`] does for a
/// `tokens` session -- but through [`render_line`] instead of
/// `trace-stream`.
#[derive(Debug, Default)]
pub struct TraceRenderer {
    /// Bytes of the in-progress line (including any incomplete trailing
    /// UTF-8), not yet terminated by a newline.
    carry: Vec<u8>,
}

impl TraceRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes stream bytes into the buffer, emitting a completed line to
    /// `view` for every `\n` found, and updating the partial line.
    pub fn feed(&mut self, bytes: &[u8], view: &mut StreamView) {
        self.carry.extend_from_slice(bytes);
        while let Some(pos) = self.carry.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.carry.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            view.push_line(render_line(&line));
        }
        view.set_partial(render_line(&String::from_utf8_lossy(&self.carry)));
    }

    /// Ends the stream: flushes any trailing partial line as a completed one.
    pub fn finish(&mut self, view: &mut StreamView) {
        if !self.carry.is_empty() {
            let line = String::from_utf8_lossy(&self.carry).into_owned();
            self.carry.clear();
            view.push_line(render_line(&line));
        }
        view.set_partial(Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turbo_vision::core::geometry::Rect;

    fn plain(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    fn record(json: &str) -> Vec<Cell> {
        render_line(json)
    }

    #[test]
    fn a_full_record_renders_timestamp_level_target_message_and_fields() {
        let json = r#"{"timestamp":"2024-03-01T12:04:01.123456Z","level":"WARN","fields":{"message":"retry","attempt":3},"target":"myapp::db"}"#;
        let out = record(json);
        let text = plain(&out);
        assert_eq!(text, "12:04:01  WARN  myapp::db  retry  attempt=3");
    }

    #[test]
    fn level_colors_are_correct() {
        for (level, expected) in [
            ("ERROR", TvColor::LightRed),
            ("WARN", TvColor::Yellow),
            ("INFO", TvColor::White),
            ("DEBUG", TvColor::LightGray),
            ("TRACE", TvColor::DarkGray),
        ] {
            let json = format!(r#"{{"level":"{level}","fields":{{"message":"x"}}}}"#);
            let out = record(&json);
            assert_eq!(
                out[0].attr.fg, expected,
                "level {level} should render {expected:?}"
            );
        }
    }

    #[test]
    fn levels_are_accepted_case_insensitively() {
        let json = r#"{"level":"warn","fields":{"message":"x"}}"#;
        let out = record(json);
        assert_eq!(out[0].attr.fg, TvColor::Yellow);
        assert_eq!(plain(&out[..4]), "WARN");
    }

    #[test]
    fn timestamp_is_dim_and_reduced_to_time_of_day() {
        let json = r#"{"timestamp":"2024-03-01T12:04:01.123456Z","level":"INFO","fields":{"message":"hi"}}"#;
        let out = record(json);
        assert_eq!(plain(&out[..8]), "12:04:01");
        assert!(out[..8].iter().all(|c| c.attr.fg == TvColor::DarkGray));
    }

    #[test]
    fn an_unparseable_timestamp_is_shown_verbatim() {
        let json = r#"{"timestamp":"not-a-time","level":"INFO","fields":{"message":"hi"}}"#;
        let out = record(json);
        assert!(plain(&out).starts_with("not-a-time"));
    }

    #[test]
    fn missing_timestamp_and_target_still_render() {
        let json = r#"{"level":"INFO","fields":{"message":"hi"}}"#;
        let out = record(json);
        assert_eq!(plain(&out), "INFO  hi");
    }

    #[test]
    fn target_is_light_cyan() {
        let json = r#"{"level":"INFO","target":"myapp::db","fields":{"message":"hi"}}"#;
        let out = record(json);
        let target_cell = out
            .iter()
            .zip(plain(&out).chars())
            .find(|(_, c)| *c == 'm')
            .unwrap()
            .0;
        assert_eq!(target_cell.attr.fg, TvColor::LightCyan);
    }

    #[test]
    fn extra_structured_fields_appear_dim_after_the_message() {
        let json = r#"{"level":"INFO","fields":{"message":"retry","attempt":3,"user":"alice"}}"#;
        let out = record(json);
        let text = plain(&out);
        assert!(text.contains("attempt=3"));
        assert!(text.contains("user=alice"));
        let dim_start = text.find("attempt=3").unwrap();
        assert!(
            out[dim_start..]
                .iter()
                .take_while(|c| c.ch != '\0')
                .all(|c| c.attr.fg == TvColor::DarkGray)
        );
    }

    #[test]
    fn a_non_json_line_renders_verbatim_in_the_default_attribute() {
        let out = render_line("thread 'main' panicked at src/main.rs:1: boom");
        assert_eq!(plain(&out), "thread 'main' panicked at src/main.rs:1: boom");
        assert!(out.iter().all(|c| c.attr.fg == DEFAULT_FG));
    }

    #[test]
    fn json_missing_a_recognized_level_renders_verbatim() {
        let json = r#"{"fields":{"message":"hi"}}"#;
        let out = render_line(json);
        assert_eq!(plain(&out), json);
    }

    #[test]
    fn trace_renderer_buffers_split_lines() {
        let mut r = TraceRenderer::new();
        let mut v = StreamView::new(Rect::new(0, 0, 80, 24));
        let json = b"{\"level\":\"INFO\",\"fields\":{\"message\":\"hi\"}}\n";
        r.feed(&json[..10], &mut v);
        r.feed(&json[10..], &mut v);
        assert_eq!(v.plain_text(), "INFO  hi");
    }

    #[test]
    fn trace_renderer_finish_flushes_a_trailing_partial_line() {
        let mut r = TraceRenderer::new();
        let mut v = StreamView::new(Rect::new(0, 0, 80, 24));
        r.feed(b"no newline here", &mut v);
        r.finish(&mut v);
        assert_eq!(v.plain_text(), "no newline here");
    }
}
