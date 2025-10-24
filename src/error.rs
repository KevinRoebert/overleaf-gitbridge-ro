use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("project not found: {0}")]
    ProjectNotFound(String),

    #[error("git command failed: {0} - {1}")]
    GitFailed(String, String),

    #[error("invalid header name: {0}")]
    HeaderName(String),

    #[error("invalid header value: {0}")]
    HeaderValue(String),

    #[error("internal: {0}")]
    Other(String),
}
