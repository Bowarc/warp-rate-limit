use chrono::{DateTime, TimeDelta, Utc};
use crate::config::RetryAfterFormat;

/// Custom rejection type for rate limiting
#[derive(Debug)]
pub struct RateLimitRejection {
    /// Duration until the client can retry
    pub retry_after: TimeDelta,
    /// Maximum requests allowed in the window
    pub limit: u32,
    /// Unix timestamp when the rate limit resets
    pub reset_time: DateTime<Utc>,
    /// Format to use for Retry-After header
    pub retry_after_format: RetryAfterFormat,
}
impl RateLimitRejection {
    pub fn formated_retry_after(&self) -> String {
        match self.retry_after_format {
            RetryAfterFormat::HttpDate => self.reset_time.to_rfc2822(),
            RetryAfterFormat::Seconds => self.retry_after.as_seconds_f64().to_string(),
        }
    }
}

impl warp::reject::Reject for RateLimitRejection {}

