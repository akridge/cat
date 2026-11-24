use thiserror::Error;

#[derive(Debug, Error)]
pub enum CogError {
    #[error("network error: {0}")]
    Http(String),
    #[error("invalid COG: {0}")]
    Format(String),
    #[error("unsupported COG: {0}")]
    Unsupported(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("projection error: {0}")]
    Projection(String),
}

pub type Result<T> = std::result::Result<T, CogError>;
