use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Format options for the Retry-After header
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RetryAfterFormat {
    /// HTTP-date format (RFC 7231)
    #[default]
    HttpDate,
    /// Number of seconds
    Seconds,
}

/// Configuration for the rate limiter
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitConfig {
    /// Maximum number of requests allowed within the window
    pub max_requests: u32,
    /// Time window for rate limiting
    pub window: Duration,
    /// Format for Retry-After header (RFC 7231 Date or Seconds)
    pub retry_after_format: RetryAfterFormat,

    // Header used to extract the client's ip address
    pub ip_header: String,
}
/// Sensible (opinionated) defaults
impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 60, // 60 req/min baseline
            window: Duration::from_secs(60),
            retry_after_format: RetryAfterFormat::HttpDate,

            ip_header: String::from("X-Forwarded-For"), // It's the one used for most of there revese proxies
        }
    }
}

/// Factory methods for quickly building a rate limiter
impl RateLimitConfig {
    /// Build a `RateLimitConfig` with sensible defaults for requests per minute
    pub fn max_per_minute(max: u32) -> Self {
        Self {
            max_requests: max,
            window: Duration::from_secs(60),
            ..Default::default()
        }
    }

    /// Build a `RateLimitConfig` with custom window size in seconds
    pub fn max_per_window(max_requests: u32, window_seconds: u64) -> Self {
        Self {
            max_requests,
            window: Duration::from_secs(window_seconds),
            ..Default::default()
        }
    }
}
