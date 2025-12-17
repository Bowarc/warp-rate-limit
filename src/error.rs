/// Errors that can occur during rate limiting logic
#[derive(Debug)]
pub enum RateLimitError {
    /// Failed to set rate limit headers
    HeaderError(warp::http::header::InvalidHeaderValue),
    /// Other unexpected errors
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RateLimitError::HeaderError(e) => write!(f, "Failed to set rate limit header: {}", e),
            RateLimitError::Other(e) => write!(f, "Rate limit error: {}", e),
        }
    }
}

impl std::error::Error for RateLimitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RateLimitError::HeaderError(e) => Some(e),
            RateLimitError::Other(e) => Some(&**e),
        }
    }
}
