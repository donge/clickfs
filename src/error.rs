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

    #[error("clickhouse server error (status {status}): {body}")]
    Server { status: u16, body: String },

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
