#![forbid(unsafe_code)]

use chrono::{DateTime, TimeDelta, Utc};
use std::sync::Arc;
use std::{collections::HashMap, net::IpAddr, str::FromStr as _};
use tokio::sync::RwLock;
use warp::{
    filters::BoxedFilter,
    http::header::{self, HeaderMap, HeaderValue},
    reject, Filter, Rejection,
};

mod error;
pub use error::RateLimitError;
mod config;
pub use config::{RateLimitConfig, RetryAfterFormat, IpExtractionMethod};

// Re-exports
pub use chrono;
pub use serde;

/// Information about the current rate limit status
#[derive(Clone, Debug)]
pub struct RateLimitInfo {
    /// Time until the rate limit resets
    pub retry_after: String,
    /// Maximum requests allowed in the window
    pub limit: u32,
    /// Remaining requests in the current window
    pub remaining: u32,
    /// Unix timestamp when the rate limit resets
    pub reset_timestamp: i64,
    /// Format used for retry-after header
    pub retry_after_format: RetryAfterFormat,

    /// Number of items in the internal map
    pub internal_map_len: usize,
    /// Least time the map was cleaned up
    pub last_cleanup_time: DateTime<Utc>,
}

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

#[derive(Clone)]
struct RateLimiter {
    state: Arc<RwLock<RateLimiterMap>>,
    config: RateLimitConfig,
}

// I really didn't want to have two different Arc<RwLock<T>> for data so interlinked
#[derive(Clone)]
struct RateLimiterMap {
    inner: HashMap<String, (DateTime<Utc>, u32)>,
    last_cleanup: DateTime<Utc>,
}

impl RateLimiter {
    fn new(config: RateLimitConfig) -> Self {
        Self {
            state: Arc::new(RwLock::new(RateLimiterMap {
                last_cleanup: Utc::now(),
                inner: HashMap::default(),
            })),
            config,
        }
    }

    async fn check_rate_limit(&self, key: &str) -> Result<RateLimitInfo, Rejection> {
        let mut map = self.state.write().await;
        let now = Utc::now();

        // Cleanup the map to remove old entries
        if now - map.last_cleanup > self.config.window {
            map.inner
                .retain(|_ip, (first_request, ..)| now - *first_request < self.config.window);
            map.last_cleanup = now;
        }

        let current = map.inner.get(key).copied();

        match current {
            Some((first_request, count)) => {
                if now.signed_duration_since(first_request) > self.config.window {
                    // Window has passed, reset counter
                    map.inner.insert(key.to_owned(), (now, 1));
                    Ok(self.create_info(
                        self.config.max_requests - 1,
                        now,
                        map.inner.len(),
                        map.last_cleanup,
                    ))
                } else if count >= self.config.max_requests {
                    // Rate limit exceeded
                    let retry_after = self.config.window - now.signed_duration_since(first_request);
                    let reset_time = Utc::now() + retry_after;

                    Err(reject::custom(RateLimitRejection {
                        retry_after,
                        limit: self.config.max_requests,
                        reset_time,
                        retry_after_format: self.config.retry_after_format.clone(),
                    }))
                } else {
                    // Increment counter
                    map.inner.insert(key.to_owned(), (first_request, count + 1));
                    Ok(self.create_info(
                        self.config.max_requests - (count + 1),
                        first_request,
                        map.inner.len(),
                        map.last_cleanup,
                    ))
                }
            }
            None => {
                // First request
                map.inner.insert(key.to_owned(), (now, 1));
                Ok(self.create_info(
                    self.config.max_requests - 1,
                    now,
                    map.inner.len(),
                    map.last_cleanup,
                ))
            }
        }
    }

    fn create_info(
        &self,
        remaining: u32,
        start: DateTime<Utc>,
        map_len: usize,
        last_cleanup_time: DateTime<Utc>,
    ) -> RateLimitInfo {
        let reset_time = start + self.config.window;
        let retry_after = match self.config.retry_after_format {
            RetryAfterFormat::HttpDate => {
                // (Utc::now() + self.config.window).to_rfc2822()
                reset_time.to_rfc2822()
            }
            RetryAfterFormat::Seconds => {
                // self.config.window.as_seconds_f64().to_string(),
                (reset_time - Utc::now()).as_seconds_f64().to_string()
            }
        };

        RateLimitInfo {
            retry_after,
            limit: self.config.max_requests,
            remaining,
            // reset_timestamp: (Utc::now()
            //     + ChronoDuration::from_std(reset_time.duration_since(start)).unwrap())
            // .timestamp(),
            reset_timestamp: reset_time.timestamp(),
            retry_after_format: self.config.retry_after_format.clone(),
            internal_map_len: map_len,
            last_cleanup_time,
        }
    }
}

/// Creates a rate limiting filter with the given configuration
pub fn with_rate_limit(
    config: RateLimitConfig,
) -> impl Filter<Extract = (RateLimitInfo,), Error = Rejection> + Clone {
    fn ip_header_filter(ip_header: &'static str) -> BoxedFilter<(String,)> {
        warp::filters::any::any()
            .and(warp::filters::header::optional::<String>(ip_header).map(
                |header_value: Option<String>| {
                    // Try splitting it at ',' and parse the first element as this is the client ip on most reverse proxies
                    // If that does not result in a valid IpAddr, abort and return 'unknown'
                    header_value
                        .and_then(|s| {
                            s.split(",")
                                .next()
                                .map(str::trim)
                                .map(IpAddr::from_str)
                                .and_then(Result::ok)
                                .as_ref()
                                .map(ToString::to_string)
                        })
                        .unwrap_or("unknown".to_owned())
                },
            ))
            .boxed()
    }

    fn remote_addr_filter() -> BoxedFilter<(String,)> {
        warp::filters::addr::remote()
            .map(move |addr: Option<std::net::SocketAddr>| {
                addr.map(|a| a.ip().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .boxed()
    }

    let ip_filter = match config.ip_extraction_method {
        config::IpExtractionMethod::Header(ip_header) => ip_header_filter(ip_header),
        config::IpExtractionMethod::RemoteAddr => remote_addr_filter(),
    };
    let rate_limiter = RateLimiter::new(config);

    warp::any()
        .map(move || rate_limiter.clone())
        .and(ip_filter)
        .and_then(|rate_limiter: RateLimiter, ip: String| async move {
            rate_limiter.check_rate_limit(&ip).await
        })
}

/// Adds rate limit headers to a response
pub fn add_rate_limit_headers(
    headers: &mut HeaderMap,
    info: &RateLimitInfo,
) -> Result<(), RateLimitError> {
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&info.retry_after).map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Limit",
        HeaderValue::from_str(&info.limit.to_string()).map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Remaining",
        HeaderValue::from_str(&info.remaining.to_string()).map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Reset",
        HeaderValue::from_str(&info.reset_timestamp.to_string())
            .map_err(RateLimitError::HeaderError)?,
    );
    Ok(())
}

/// Adds rate limit headers to a response
pub fn add_rate_limit_headers_from_rejection(
    headers: &mut HeaderMap,
    rejection: &RateLimitRejection,
) -> Result<(), RateLimitError> {
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&rejection.formated_retry_after())
            .map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Limit",
        HeaderValue::from_str(&rejection.limit.to_string()).map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Remaining",
        HeaderValue::from_str("0").map_err(RateLimitError::HeaderError)?,
    );
    headers.insert(
        "X-RateLimit-Reset",
        HeaderValue::from_str(&rejection.reset_time.timestamp().to_string())
            .map_err(RateLimitError::HeaderError)?,
    );
    Ok(())
}
