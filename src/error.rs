use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol framing error: invalid JSON in message")]
    FramingError,

    #[error(
        "File descriptor count mismatch: fds field specifies {expected}, but {found} FDs available"
    )]
    MismatchedCount { expected: usize, found: usize },

    #[error("System call error: {0}")]
    SystemCall(String),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Invalid message format: {0}")]
    InvalidMessage(String),
}

pub type Result<T> = std::result::Result<T, Error>;
