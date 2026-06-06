use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("store not found: no .ken/ directory walking up from the working directory")]
    StoreNotFound,

    #[error("fact not found: {0}")]
    FactNotFound(String),

    #[error("invalid key: {0}")]
    Key(#[from] crate::schema::KeyError),

    #[error("jj command failed: {0}")]
    Jj(String),

    #[error("verifier error: {0}")]
    Verifier(String),

    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}
