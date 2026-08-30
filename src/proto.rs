// Copyright (c) 2026 Enzo Lombardi
// SPDX-License-Identifier: MIT

//! The `HELLO` handshake.
//!
//! ```text
//! client -> control 7878 :  HELLO <version> <kind> <name>\n
//! server ->              :  PORT <n>\n              (or  ERR <reason>\n)
//! ```
//!
//! `<kind>` is `tokens` or `trace`.
//!
//! A first line that is not a `HELLO` is not an error: the connection is
//! treated as a raw anonymous stream defaulting to the `tokens` kind, so
//! `nc host 7878 < capture.txt` works with no ceremony.

/// Maximum session-name length, in bytes.
pub const NAME_MAX: usize = 64;

/// The protocol version this console speaks. The single source of truth for
/// what a `HELLO` must claim to be accepted.
pub const PROTOCOL_VERSION: u32 = 1;

/// What kind of stream a session renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// A model token stream, rendered through the `trace-stream` pipeline.
    Tokens,
    /// A `tracing-subscriber` JSON-lines record stream.
    Trace,
}

impl StreamKind {
    /// Parses the `<kind>` field of a `HELLO`.
    ///
    /// # Errors
    /// [`HelloError::UnknownStreamKind`] for anything but `tokens` or `trace`.
    pub fn parse(s: &str) -> Result<Self, HelloError> {
        match s {
            "tokens" => Ok(Self::Tokens),
            "trace" => Ok(Self::Trace),
            other => Err(HelloError::UnknownStreamKind(other.to_string())),
        }
    }
}

/// Why a line was not a usable `HELLO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloError {
    /// Not a handshake at all — treat the connection as a raw stream.
    NotHello,
    /// A handshake with an unusable name.
    BadName,
    /// A `HELLO` with no version field at all. Nothing is deployed yet, so
    /// this ambiguity is cheapest to close now rather than silently
    /// assuming version 1.
    MissingVersion,
    /// A version field that isn't a bare non-negative integer.
    BadVersion,
    /// A well-formed version this console does not speak.
    UnsupportedVersion(u32),
    /// The old two-field `HELLO <version> <name>` form: there is no way to
    /// tell whether the missing field was meant to be a kind or a name, and
    /// guessing is exactly the ambiguity this error avoids.
    MissingStreamKind,
    /// A well-formed kind field that isn't `tokens` or `trace`.
    UnknownStreamKind(String),
}

impl HelloError {
    /// The line to send back, without its newline.
    #[must_use]
    pub fn wire(&self) -> String {
        match self {
            Self::NotHello => "ERR not a handshake".to_string(),
            Self::BadName => "ERR bad name".to_string(),
            Self::MissingVersion => "ERR missing protocol version".to_string(),
            Self::BadVersion => "ERR bad protocol version".to_string(),
            Self::UnsupportedVersion(v) => format!("ERR unsupported protocol version {v}"),
            Self::MissingStreamKind => "ERR missing stream kind".to_string(),
            Self::UnknownStreamKind(k) => format!("ERR unknown stream kind {k}"),
        }
    }
}

/// Parses one handshake line, returning the stream kind and session name.
///
/// # Errors
/// [`HelloError::NotHello`] when the line has no `HELLO ` prefix;
/// [`HelloError::MissingVersion`] when there is no version field at all;
/// [`HelloError::BadVersion`] when the version field is not a bare
/// non-negative integer; [`HelloError::UnsupportedVersion`] when the version
/// is well-formed but not [`PROTOCOL_VERSION`]; [`HelloError::MissingStreamKind`]
/// when there is no kind field (the old two-field form);
/// [`HelloError::UnknownStreamKind`] when the kind is neither `tokens` nor
/// `trace`; [`HelloError::BadName`] when the name is empty, longer than
/// [`NAME_MAX`], or contains anything but printable non-space ASCII.
pub fn parse_hello(line: &str) -> Result<(StreamKind, String), HelloError> {
    let line = line.trim_end_matches(['\r', '\n']);
    let rest = line.strip_prefix("HELLO ").ok_or(HelloError::NotHello)?;

    let (version, rest) = rest.split_once(' ').ok_or(HelloError::MissingVersion)?;
    if version.is_empty() {
        return Err(HelloError::MissingVersion);
    }
    let version: u32 = version.parse().map_err(|_| HelloError::BadVersion)?;
    if version != PROTOCOL_VERSION {
        return Err(HelloError::UnsupportedVersion(version));
    }

    let (kind, name) = rest.split_once(' ').ok_or(HelloError::MissingStreamKind)?;
    let kind = StreamKind::parse(kind)?;

    if name.is_empty() || name.len() > NAME_MAX {
        return Err(HelloError::BadName);
    }
    if !name.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(HelloError::BadName);
    }
    Ok((kind, name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_well_formed_tokens_hello() {
        assert_eq!(
            parse_hello("HELLO 1 tokens build-agent").unwrap(),
            (StreamKind::Tokens, "build-agent".to_string())
        );
    }

    #[test]
    fn accepts_a_well_formed_trace_hello() {
        assert_eq!(
            parse_hello("HELLO 1 trace myapp").unwrap(),
            (StreamKind::Trace, "myapp".to_string())
        );
    }

    #[test]
    fn trailing_cr_is_tolerated() {
        assert_eq!(
            parse_hello("HELLO 1 tokens x\r").unwrap(),
            (StreamKind::Tokens, "x".to_string())
        );
    }

    #[test]
    fn a_non_hello_line_is_not_an_error_but_a_raw_stream() {
        assert!(matches!(
            parse_hello("hello there"),
            Err(HelloError::NotHello)
        ));
        assert!(matches!(
            parse_hello("{\"tok\":1}"),
            Err(HelloError::NotHello)
        ));
    }

    #[test]
    fn hello_with_no_version_is_missing_version_not_assumed_v1() {
        assert!(matches!(
            parse_hello("HELLO build-agent"),
            Err(HelloError::MissingVersion)
        ));
    }

    #[test]
    fn non_numeric_version_is_bad_version_not_a_fallback() {
        assert!(matches!(
            parse_hello("HELLO v1 tokens build-agent"),
            Err(HelloError::BadVersion)
        ));
        assert!(matches!(
            parse_hello("HELLO -1 tokens build-agent"),
            Err(HelloError::BadVersion)
        ));
    }

    #[test]
    fn unsupported_version_is_rejected_by_number() {
        assert_eq!(
            parse_hello("HELLO 2 tokens build-agent"),
            Err(HelloError::UnsupportedVersion(2))
        );
        assert_eq!(
            HelloError::UnsupportedVersion(2).wire(),
            "ERR unsupported protocol version 2"
        );
    }

    /// The old two-field `HELLO <version> <name>` form is a distinct, honest
    /// error — not silently defaulted to `tokens`, and not confused with a
    /// bad-name rejection.
    #[test]
    fn the_old_two_field_form_is_missing_stream_kind() {
        assert_eq!(
            parse_hello("HELLO 1 build-agent"),
            Err(HelloError::MissingStreamKind)
        );
        assert_eq!(
            HelloError::MissingStreamKind.wire(),
            "ERR missing stream kind"
        );
    }

    #[test]
    fn an_unknown_stream_kind_is_rejected_by_name() {
        assert_eq!(
            parse_hello("HELLO 1 bogus build-agent"),
            Err(HelloError::UnknownStreamKind("bogus".to_string()))
        );
        assert_eq!(
            HelloError::UnknownStreamKind("bogus".to_string()).wire(),
            "ERR unknown stream kind bogus"
        );
    }

    #[test]
    fn empty_oversized_and_whitespace_names_are_rejected() {
        assert!(matches!(
            parse_hello("HELLO 1 tokens "),
            Err(HelloError::BadName)
        ));
        assert!(matches!(
            parse_hello("HELLO 1 tokens a b"),
            Err(HelloError::BadName)
        ));
        let long = "x".repeat(65);
        assert!(matches!(
            parse_hello(&format!("HELLO 1 tokens {long}")),
            Err(HelloError::BadName)
        ));
        assert!(parse_hello(&format!("HELLO 1 tokens {}", "x".repeat(64))).is_ok());
    }

    #[test]
    fn non_printable_names_are_rejected() {
        assert!(matches!(
            parse_hello("HELLO 1 tokens na\u{7}me"),
            Err(HelloError::BadName)
        ));
        assert!(matches!(
            parse_hello("HELLO 1 tokens café"),
            Err(HelloError::BadName)
        ));
    }
}
