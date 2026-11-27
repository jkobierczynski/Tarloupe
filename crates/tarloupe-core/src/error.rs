use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("index database error")]
    Sqlite(#[from] rusqlite::Error),

    #[error("corrupt tar archive at offset {offset}: {reason}")]
    Corrupt { offset: u64, reason: String },

    #[error("not a tar archive (no valid tar header found)")]
    NotTar,

    #[error("index {path} is stale or incompatible: {reason}")]
    StaleIndex { path: String, reason: String },

    #[error("no such entry: {0}")]
    NotFound(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("cancelled")]
    Cancelled,

    /// Building the index in the background failed (see
    /// [`crate::Archive::open_live`]).
    #[error("indexing failed: {0}")]
    IndexingFailed(String),
}

impl Error {
    pub(crate) fn corrupt(offset: u64, reason: impl Into<String>) -> Self {
        Error::Corrupt {
            offset,
            reason: reason.into(),
        }
    }
}
