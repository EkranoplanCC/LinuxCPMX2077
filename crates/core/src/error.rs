use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("7z error: {0}")]
    SevenZ(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsafe archive: {0}")]
    UnsafeArchive(String),
    #[error("unsupported archive format: {0}")]
    UnsupportedArchive(String),
    #[error("file conflict: {0}")]
    Conflict(String),
    #[error("game not found: {0}")]
    GameNotFound(String),
    #[error("Nexus API error: {0}")]
    Nexus(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("secret storage error: {0}")]
    Secret(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
