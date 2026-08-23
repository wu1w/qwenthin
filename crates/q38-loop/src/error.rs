use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("template: {0}")]
    Template(String),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("http: {0}")]
    Http(String),
    #[error("vendor: {0}")]
    Vendor(String),
    #[error("watchdog: think token cap")]
    Watchdog,
    #[error("{0}")]
    Msg(String),
}

impl Error {
    pub fn msg(m: impl fmt::Display) -> Self {
        Self::Msg(m.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Self::Config(e.to_string())
    }
}
