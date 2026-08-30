# Trace stream kind — implementation notes

## Final grammar

```
client -> control 7878 :  HELLO <version> <kind> <name>\n
server ->              :  PORT <n>\n              (or  ERR <reason>\n)
client -> data <n>     :  raw bytes until close
```

Version stays `1`. `<kind>` is `tokens` or `trace`. The no-handshake
anonymous fallback (first line is not `HELLO` at all) is unchanged and
defaults to `tokens`.

## Every error string

| Situation | Wire error |
|---|---|
| No `HELLO ` prefix | *(not an error — anonymous `tokens` session)* |
| No version field at all | `ERR missing protocol version` |
| Version field not a bare non-negative integer | `ERR bad protocol version` |
| Version well-formed but not `1` | `ERR unsupported protocol version <v>` |
| Old two-field form `HELLO <version> <name>` (no kind field) | `ERR missing stream kind` |
| Kind present but not `tokens`/`trace` | `ERR unknown stream kind <k>` |
| Name empty, >64 bytes, or non-printable/whitespace | `ERR bad name` |
| A second writer dials an already-live session's data port | `ERR already attached` |
| `open_or_reuse` fails to bind a data port | `ERR no port` |

All implemented in `src/proto.rs` (`HelloError`, `StreamKind::parse`,
`parse_hello`), wired straight through in `src/registry.rs::handle_control`
(the new `HelloError` arms are just added to the existing "write `.wire()`
back" match arm; no new behavior needed there).

## Record fields handled and how each renders

Wire format: one `tracing-subscriber` JSON record per line (as produced by
`tracing_subscriber::fmt().json().with_writer(socket).init()`), parsed with
`serde_json` (with `preserve_order` so structured fields render in emission
order rather than alphabetically).

| Field | Required? | Rendering |
|---|---|---|
| `level` | Effectively yes — see below | Uppercased, colored: `ERROR` LightRed, `WARN` Yellow, `INFO` White, `DEBUG` LightGray, `TRACE` DarkGray. Accepted case-insensitively. |
| `timestamp` | No | If it parses as RFC3339-shaped (`YYYY-MM-DDTHH:MM:SS...`), only `HH:MM:SS` is shown, in DarkGray. If it doesn't match that shape, shown verbatim (still DarkGray) rather than dropped. Missing entirely → segment omitted. |
| `target` | No | Shown in LightCyan. Missing → segment omitted. |
| `fields.message` | No | Shown in the default foreground (LightGray, matching the rest of the app's default text color). |
| other `fields.*` | No | Rendered as dim (DarkGray) `key=value` pairs after the message, space-separated. String values are unquoted (`user=alice`, not `user="alice"`); non-string values use their JSON text form (`attempt=3`). |
| `filename`, `line_number`, `span`, `spans`, `threadName`, `threadId` | No | Not rendered — the spec only requires *not choking* on their presence; they aren't part of the one-line layout the spec specifies. |

**Design decision on a missing/unrecognized `level`:** the spec says every
field but `level` is optional, but doesn't say what to do if `level` is
present-but-unrecognized or the JSON simply lacks it. I chose to treat that
line as unparseable and fall through to the verbatim-render path (default
foreground, no styling) — same bucket as invalid JSON. Rationale: `level` is
load-bearing for the whole one-line layout (it drives the color and anchors
where the rest of the segments go), so a record without a usable one isn't a
partial structured record, it's a line this renderer doesn't understand — and
per the spec's own rule for non-JSON lines, showing it unstyled beats
dropping it.

A line that fails to parse as JSON at all renders verbatim in the default
attribute (LightGray on Black), never dropped, never an error.

## Window title for trace sessions

Tagged with a leading `[trace] ` before the existing `name :port` /
`[disconnected]` shape, e.g.:

```
[trace] myapp :54312
[trace] myapp :54312 [disconnected]
```

Rationale: `name :port` alone gives no visual hint that a window is
rendering structured log records rather than a token stream, and glancing at
a desktop full of windows to tell them apart seemed worth the few extra
characters. Implemented in `SessionState::window_title` (`src/session.rs`)
and mirrored in `main.rs`'s `ConsoleIntent::CreateWindow` handling for the
window's initial title (set before `SessionState` exists to report it back).

## Plumbing

- `ServerEvent::Opened` now carries `kind: StreamKind` (`src/registry.rs`).
- `SessionState` holds an internal `Renderer` enum (`Tokens(Box<Pipeline>)` |
  `Trace(TraceRenderer)`) instead of a bare `Pipeline` field; `Sessions::insert`
  takes the `StreamKind` and builds the right one. A trace session never
  constructs a `Pipeline` (boxed the `Tokens` variant to satisfy
  `clippy::large_enum_variant`, since `Pipeline` is ~1KB and `TraceRenderer`
  is a handful of bytes).
- `SessionState::feed`/`finish` dispatch to whichever renderer the session
  holds; `main.rs`'s capture-file path (`open_capture`) now calls those
  instead of reaching into `.pipeline` directly (kept as `StreamKind::Tokens`,
  since a saved capture is always a token stream).
- New module: `src/tracefmt.rs` — `render_line` (pure, one line in, one
  `Vec<Cell>` out) plus `TraceRenderer` (buffers bytes into lines the same
  way `Pipeline`/`AnsiLineAssembler` do, so partial-line delivery over TCP
  works the same for both kinds). Does not touch `trace-stream`/`Pipeline`.

## Tests added

- `src/proto.rs`: `accepts_a_well_formed_tokens_hello`,
  `accepts_a_well_formed_trace_hello`, `the_old_two_field_form_is_missing_stream_kind`,
  `an_unknown_stream_kind_is_rejected_by_name`, plus the existing tests
  updated to the three-field grammar.
- `src/tracefmt.rs`: full-record rendering, one test per level's color,
  case-insensitive levels, timestamp reduced to time-of-day, unparseable
  timestamp shown verbatim, missing timestamp/target still render, target
  color, extra structured fields dimmed after the message, non-JSON line
  verbatim, JSON missing a recognized level renders verbatim, `TraceRenderer`
  buffering across split `feed` calls and `finish` flushing a trailing
  partial line.
- `src/session.rs`: a trace session renders through `tracefmt` (not the
  token pipeline), and its window title carries the `[trace]` tag.
- `src/main.rs`: `Opened` carrying `StreamKind::Trace` decides to create a
  window with that kind (`opened_carries_the_trace_kind_through`).
- `tests/protocol.rs`: `HELLO 1 trace myapp` opens a session with
  `StreamKind::Trace`; `HELLO 1 tokens build` opens one with `StreamKind::Tokens`;
  unknown kind refused by name; the old two-field form is a hard error;
  the anonymous (no-`HELLO`) fallback still defaults to `tokens`. All
  pre-existing `HELLO 1 <name>` call sites were updated to the new
  `HELLO 1 tokens <name>` form (the old form is now a deliberate breaking
  change per the brief).

## Test commands and output

```
cargo test --all-targets       # 58 lib + 11 main + 1 golden + 22 protocol = 92, all pass
cargo clippy --all-targets -- -D warnings   # clean, no #[allow] used
cargo fmt --all -- --check     # clean
```

20x loop:

```sh
for i in $(seq 20); do cargo test --test protocol || break; done
```

Ran clean for all 20 iterations — 22 passed, 0 failed, every time
(~5.52s each, dominated by the real-concurrency reconnect-race test's
20 internal iterations).

## Left unverified

- No TTY in this environment, so the actual Turbo Vision rendering (window
  colors, the `[trace]` title tag as it appears in a real terminal, the
  black-background window frame) was never visually observed — only proven
  through the headless/unit-level tests above (`render_line`'s `Cell` colors,
  `window_title()` strings, the existing headless `title_render_tests`
  pattern for window placement, which this change didn't touch). Please
  verify visually.
- Did not manually run a real `tracing_subscriber::fmt().json()` program
  against a live `turbo-debug-console` process end-to-end (no TTY to view
  the result in); relied on hand-constructed JSON matching the documented
  `tracing-subscriber` 0.3.23 JSON shape instead.
- The exact default-foreground color choice for `fields.message` (LightGray)
  is an inference from the rest of the codebase's convention (`StreamView`'s
  fill color, `AnsiLineAssembler`'s default carry color), not stated in the
  spec — flagging in case a different "default foreground" was intended.
