use chrono::{DateTime, Utc};

/// Information about the current rate limit status
#[derive(Clone, Debug)]
pub struct RateLimitInfo {
    /// Maximum requests allowed in the window
    pub limit: u32,
    /// Remaining requests in the current window
    pub remaining: u32,
    /// Unix timestamp when the rate limit resets
    pub reset_timestamp: i64,

    /// Number of items in the internal map
    pub internal_map_len: usize,
    /// Last time the map was cleaned up
    pub last_cleanup_time: DateTime<Utc>,
}

