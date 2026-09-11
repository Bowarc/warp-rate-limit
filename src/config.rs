use chrono::TimeDelta;
use serde::{Deserialize, Serialize};

/// Format options for the Retry-After header
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RetryAfterFormat {
    /// HTTP-date format (RFC 7231)
    #[default]
    HttpDate,
    /// Number of seconds
    Seconds,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum IpExtractionMethod {
    Header(&'static str),
    #[default]
    RemoteAddr,
}

/// Configuration for the rate limiter
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitConfig {
    /// Maximum number of requests allowed within the window
    pub max_requests: u32,
    /// Time window for rate limiting
    pub window: TimeDelta,
    /// Format for Retry-After header (RFC 7231 Date or Seconds)
    pub retry_after_format: RetryAfterFormat,

    /// Header used to extract the client's ip address
    // pub ip_header: String,

    // The method for extracting the client's ip
    // For untrusted environements or where the server is directly reachable from the internet
    // RemoteAddr is the safer solution, but if the warp server is behind a reverse proxy, this might not work
    // (i.e return the address of the proxy instead of the client's)
    // In this case, use 
    // Header("X-Forwarded-For")
    // And make sure your reverse proxy correctly sets the header
    pub ip_extraction_method: IpExtractionMethod,
}
/// Sensible (opinionated) defaults
impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 60, // 60 req/min baseline
            // window: Duration::from_secs(60),
            window: TimeDelta::seconds(60),
            retry_after_format: RetryAfterFormat::HttpDate,

            // ip_header: String::from("X-Forwarded-For"), // It's the one used by most of the revese proxies
            ip_extraction_method: IpExtractionMethod::Header({
                let b = Box::new(String::from("Test"));

                b.leak()
            })
        }
    }
}

/// Factory methods for quickly building a rate limiter
impl RateLimitConfig {
    /// Build a `RateLimitConfig` with sensible defaults for requests per minute
    pub fn max_per_minute(max: u32) -> Self {
        Self {
            max_requests: max,
            window: TimeDelta::seconds(60),
            ..Default::default()
        }
    }

    /// Build a `RateLimitConfig` with custom window size in seconds
    pub fn max_per_window(max_requests: u32, window_seconds: i64) -> Self {
        Self {
            max_requests,
            window: TimeDelta::seconds(window_seconds),
            ..Default::default()
        }
    }
}
