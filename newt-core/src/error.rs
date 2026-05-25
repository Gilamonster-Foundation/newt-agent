use thiserror::Error;

#[derive(Debug, Error)]
pub enum NewtError {
    #[error("no backend supports tier {0:?}")]
    NoBackendForTier(crate::router::Tier),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, NewtError>;
