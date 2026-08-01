use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("No API provider registered for api: {0}")]
    UnknownApi(String),
    #[error("No API key for provider: {0}")]
    MissingApiKey(String),
    #[error("{0}")]
    Provider(String),
}
