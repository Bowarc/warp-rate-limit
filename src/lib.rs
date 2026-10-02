#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::{collections::HashMap, net::IpAddr, str::FromStr as _};
use tokio::sync::RwLock;
use warp::{
    Filter, Rejection,
    filters::BoxedFilter,
    http::header::{self, HeaderMap, HeaderValue},
    reject,
};

mod info;
pub use info::RateLimitInfo;
mod rejection;
pub use rejection::RateLimitRejection;
mod error;
pub use error::RateLimitError;
mod config;
pub use config::{IpExtractionMethod, RateLimitConfig, RetryAfterFormat};

// Re-exports
pub use chrono;
pub use serde;

#[derive(Clone)]
struct RateLimiter {
    state: Arc<RwLock<RateLimiterMap>>,
    config: RateLimitConfig,
}

// I really didn't want to have two different Arc<RwLock<T>> for data so interlinked
#[derive(Clone)]
struct RateLimiterMap {
    // key: IP of the user
    // value: (time of the first request of the window (window start), number of requests in the window)
    inner: HashMap<String, (DateTime<Utc>, u32)>,
    // Last time the map was cleaned up
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

        // Cleanup: Remove all entries where the window has expired (window start earlier than now - config.window)
        if now - map.last_cleanup > self.config.window {
            map.inner
                .retain(|_ip, (first_request, ..)| now - *first_request < self.config.window);
            map.last_cleanup = now;
        }

        let current = map.inner.get(key).copied();

        // match current {
        //     // There is no entry for that ip or the entry has expired
        //     // Reset
        //     current
        //         if current.is_none_or(|(window_start, _)| {
        //             now.signed_duration_since(window_start) >= self.config.window
        //         }) =>
        //     {
        //         map.inner.insert(key.to_owned(), (now, 1));
        //         Ok(self.create_info(
        //             self.config.max_requests - 1,
        //             now,
        //             map.inner.len(),
        //             map.last_cleanup,
        //         ))
        //     }
        //     // The entry request count has exceeded the maxium
        //     // Restrict
        //     Some((first_request, count)) if count > self.config.max_requests => {
        //         let retry_after = self.config.window - now.signed_duration_since(first_request);
        //         let reset_time = Utc::now() + retry_after;

        //         Err(reject::custom(RateLimitRejection {
        //             retry_after,
        //             limit: self.config.max_requests,
        //             reset_time,
        //             retry_after_format: self.config.retry_after_format.clone(),
        //         }))
        //     }
        //     // The entry exists, has not expired or exceeded the request limit
        //     // Increment
        //     Some((window_start, count)) => {
        //         map.inner.insert(key.to_owned(), (window_start, count + 1));
        //         Ok(self.create_info(
        //             self.config.max_requests - (count + 1),
        //             window_start,
        //             map.inner.len(),
        //             map.last_cleanup,
        //         ))
        //     }
        //     None => unreachable!(), // Taken care of by the first pattern
        // }

        let (new_window_start, new_count) = match current {
            // Entry exist and has not expired
            // Check count, reject if too high, increment if not
            Some((window_start, count))
                if now.signed_duration_since(window_start) < self.config.window =>
            {
                if count > self.config.max_requests {
                    // The request limit has been reached, reject the request
                    let retry_after = self.config.window - now.signed_duration_since(window_start);

                    return Err(reject::custom(RateLimitRejection {
                        retry_after,
                        limit: self.config.max_requests,
                        reset_time: now + retry_after,
                        retry_after_format: self.config.retry_after_format.clone(),
                    }));
                } else {
                    // The limit has NOT been reached yet, increment the count and continue
                    (window_start, count + 1)
                }
            }
            // The entry does not exist OR the window has expired
            // Reset
            _ => (now, 1),
        };

        // This updates the entry if it exists, insert otherwise
        map.inner
            .insert(key.to_owned(), (new_window_start, new_count));

        Ok(self.create_info(
            self.config.max_requests - new_count,
            new_window_start,
            map.inner.len(),
            map.last_cleanup,
        ))
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
