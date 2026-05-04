use std::fmt;

#[derive(thiserror::Error, Debug)]
#[allow(dead_code)] // some variants reserved for future write/cancel paths
pub enum ClickFsError {
    #[error("path not found")]
    NotFound,

    #[error("not a directory")]
    NotADir,

    #[error("read-only filesystem")]
    ReadOnly,

    #[error("read sequence violation: expected offset {expected}, got {got}")]
    ReadOrder { expected: u64, got: u64 },

    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    #[error("clickhouse: {0}")]
    Query(#[from] QueryError),

    #[error("operation cancelled")]
    Cancelled,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(thiserror::Error, Debug)]
pub enum QueryError {
    #[error("http transport: {0}")]
    Transport(String),

    /// HTTP error response we could not parse as a structured ClickHouse
    /// exception (no `X-ClickHouse-Exception-Code` header, no recognizable
    /// "Code: NNN. DB::Exception:" body).
    #[error("clickhouse server error (status {status}): {body}")]
    Server { status: u16, body: String },

    /// Structured ClickHouse exception parsed from the response.
    /// `name` is the canonical short name (e.g. "UNKNOWN_TABLE") for the
    /// codes we map; arbitrary unknown codes get name "UNKNOWN".
    #[error("clickhouse {name} (code {code}): {message}")]
    ClickHouse {
        code: i32,
        name: &'static str,
        message: String,
    },

    #[error("auth failed")]
    Auth,

    #[error("stream io: {0}")]
    Stream(String),
}

impl ClickFsError {
    /// Map error to POSIX errno for fuser ReplyError.
    pub fn to_errno(&self) -> i32 {
        match self {
            ClickFsError::NotFound => libc::ENOENT,
            ClickFsError::NotADir => libc::ENOTDIR,
            ClickFsError::ReadOnly => libc::EROFS,
            ClickFsError::ReadOrder { .. } => libc::EIO,
            ClickFsError::InvalidIdentifier(_) => libc::ENOENT,
            ClickFsError::Cancelled => libc::EINTR,
            ClickFsError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            ClickFsError::Query(qe) => match qe {
                QueryError::Transport(_) => libc::EIO,
                QueryError::Server { status, .. } => match *status {
                    401 | 403 => libc::EACCES,
                    404 => libc::ENOENT,
                    _ => libc::EIO,
                },
                QueryError::ClickHouse { code, .. } => clickhouse_code_to_errno(*code),
                QueryError::Auth => libc::EACCES,
                QueryError::Stream(_) => libc::EIO,
            },
            ClickFsError::Internal(_) => libc::EIO,
        }
    }
}

pub type Result<T> = std::result::Result<T, ClickFsError>;

impl From<reqwest::Error> for QueryError {
    fn from(e: reqwest::Error) -> Self {
        if let Some(status) = e.status() {
            QueryError::Server {
                status: status.as_u16(),
                body: e.to_string(),
            }
        } else {
            QueryError::Transport(e.to_string())
        }
    }
}

/// Compact display for tracing.
#[allow(dead_code)]
pub struct Compact<'a, T: fmt::Display>(pub &'a T);
impl<'a, T: fmt::Display> fmt::Display for Compact<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.to_string();
        if s.len() > 200 {
            write!(f, "{}...", &s[..200])
        } else {
            f.write_str(&s)
        }
    }
}

// ---------------------------------------------------------------------------
// ClickHouse exception parsing
// ---------------------------------------------------------------------------

/// Map a known ClickHouse error code to a short canonical name.
/// Returns "UNKNOWN" for codes we don't recognize.
pub fn clickhouse_code_name(code: i32) -> &'static str {
    match code {
        60 => "UNKNOWN_TABLE",
        81 => "UNKNOWN_DATABASE",
        159 => "TIMEOUT_EXCEEDED",
        164 => "READONLY",
        192 => "AUTHENTICATION_FAILED",
        194 => "WRONG_PASSWORD",
        202 => "TOO_MANY_SIMULTANEOUS_QUERIES",
        241 => "MEMORY_LIMIT_EXCEEDED",
        394 => "QUERY_WAS_CANCELLED",
        _ => "UNKNOWN",
    }
}

/// Map a ClickHouse error code to a POSIX errno.
/// Defaults to EIO for unknown codes.
pub fn clickhouse_code_to_errno(code: i32) -> i32 {
    match code {
        60 | 81 => libc::ENOENT,
        159 => libc::ETIMEDOUT,
        164 => libc::EROFS,
        192 | 194 => libc::EACCES,
        202 => libc::EAGAIN,
        241 => libc::ENOMEM,
        394 => libc::ECANCELED,
        _ => libc::EIO,
    }
}

/// Attempt to parse a structured ClickHouse exception out of an HTTP error
/// response.
///
/// Two sources, in order:
///
/// 1. The `X-ClickHouse-Exception-Code` header (definitive when present).
///    The body is then taken verbatim as the message.
/// 2. The body itself, scanned for `Code: NNN. DB::Exception: <msg>`
///    (the canonical text form used by ClickHouse for HTTP errors).
///
/// Returns `Some((code, message))` on success, `None` if neither source
/// yields a parseable code.
pub fn parse_clickhouse_error(header_code: Option<&str>, body: &str) -> Option<(i32, String)> {
    // 1. Header takes precedence.
    if let Some(s) = header_code {
        if let Ok(code) = s.trim().parse::<i32>() {
            // Strip trailing whitespace and the trailing
            // "(VERSION ...)" parens that CH likes to append.
            let msg = clean_message(body);
            return Some((code, msg));
        }
    }

    // 2. Fall back to parsing the body.
    //
    // Format examples:
    //   "Code: 60. DB::Exception: Table default.foo does not exist."
    //   "Code: 192. DB::Exception: ..."
    //
    // We look for "Code:" followed by digits; everything after the next
    // "DB::Exception:" (or just the rest of the line if absent) is the
    // message.
    let idx = body.find("Code:")?;
    let after = &body[idx + "Code:".len()..];
    let digits: String = after
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let code: i32 = digits.parse().ok()?;

    let msg_start = body
        .find("DB::Exception:")
        .map(|i| i + "DB::Exception:".len())
        .unwrap_or(idx);
    let msg = clean_message(&body[msg_start..]);
    Some((code, msg))
}

fn clean_message(s: &str) -> String {
    let s = s.trim();
    // Drop the trailing "(version 25.x.x.x ...)" suffix CH likes to add.
    let s = match s.rfind(" (version") {
        Some(i) => &s[..i],
        None => s,
    };
    s.trim_end_matches('.').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_from_header() {
        let body = "Code: 60. DB::Exception: Table foo doesn't exist.";
        let got = parse_clickhouse_error(Some("60"), body).unwrap();
        assert_eq!(got.0, 60);
        assert!(got.1.contains("DB::Exception") || got.1.contains("doesn't exist"));
    }

    #[test]
    fn parse_from_body_only() {
        let body = "Code: 81. DB::Exception: Database `nope` doesn't exist. \
                    (UNKNOWN_DATABASE) (version 25.3.2.39 (official build))";
        let got = parse_clickhouse_error(None, body).unwrap();
        assert_eq!(got.0, 81);
        assert!(got.1.contains("doesn't exist"));
        assert!(!got.1.contains("(version"));
    }

    #[test]
    fn parse_unknown_code_returns_some_with_unknown_name() {
        let body = "Code: 99999. DB::Exception: weird";
        let (code, _msg) = parse_clickhouse_error(None, body).unwrap();
        assert_eq!(code, 99999);
        assert_eq!(clickhouse_code_name(99999), "UNKNOWN");
        assert_eq!(clickhouse_code_to_errno(99999), libc::EIO);
    }

    #[test]
    fn parse_header_takes_precedence_over_body_mismatch() {
        // Header says 192, body says 60 — header wins.
        let body = "Code: 60. DB::Exception: ignored";
        let got = parse_clickhouse_error(Some("192"), body).unwrap();
        assert_eq!(got.0, 192);
    }

    #[test]
    fn parse_returns_none_on_empty() {
        assert!(parse_clickhouse_error(None, "").is_none());
        assert!(parse_clickhouse_error(None, "garbage with no marker").is_none());
        // Header that doesn't parse as int -> fall back to body, which has nothing.
        assert!(parse_clickhouse_error(Some("not-a-number"), "").is_none());
    }

    #[test]
    fn errno_mapping_table() {
        assert_eq!(clickhouse_code_to_errno(60), libc::ENOENT);
        assert_eq!(clickhouse_code_to_errno(81), libc::ENOENT);
        assert_eq!(clickhouse_code_to_errno(192), libc::EACCES);
        assert_eq!(clickhouse_code_to_errno(194), libc::EACCES);
        assert_eq!(clickhouse_code_to_errno(164), libc::EROFS);
        assert_eq!(clickhouse_code_to_errno(159), libc::ETIMEDOUT);
        assert_eq!(clickhouse_code_to_errno(202), libc::EAGAIN);
        assert_eq!(clickhouse_code_to_errno(241), libc::ENOMEM);
        assert_eq!(clickhouse_code_to_errno(394), libc::ECANCELED);
    }
}
