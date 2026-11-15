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

    #[error("{codec} compressed archives are not supported yet (planned for {milestone})")]
    UnsupportedCodec {
        codec: &'static str,
        milestone: &'static str,
    },

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
}

impl Error {
    pub(crate) fn corrupt(offset: u64, reason: impl Into<String>) -> Self {
        Error::Corrupt {
            offset,
            reason: reason.into(),
        }
    }
}
