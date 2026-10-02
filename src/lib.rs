#![forbid(unsafe_code)]

use chrono::{DateTime, MAX_DATE, TimeDelta, Utc};
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
pub use rejection::{RateLimitRejection, RateLimitCapacityRejection};
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RateLimitKey {
    Ip(IpAddr),
    Unknown, // Unidentified, sharing the same bucket
}

// I really didn't want to have two different Arc<RwLock<T>> for data so interlinked
#[derive(Clone)]
struct RateLimiterMap {
    // key: IP of the user
    // value: (time of the first request of the window (window start), number of requests in the window)
    inner: HashMap<RateLimitKey, (DateTime<Utc>, u32)>,
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
    fn cleanup_internal_map(
        map: &mut RateLimiterMap,
        current_time: DateTime<Utc>,
        time_window: TimeDelta,
    ) {
        map.inner
            .retain(|_ip, (first_request, ..)| current_time - *first_request < time_window);
        map.last_cleanup = current_time;
    }

    async fn check_rate_limit(&self, key: RateLimitKey) -> Result<RateLimitInfo, Rejection> {
        let mut map = self.state.write().await;
        let now = Utc::now();

        // Cleanup: Remove all entries where the window has expired (window start earlier than now - config.window)
        if now - map.last_cleanup > self.config.window {
            Self::cleanup_internal_map(&mut map, now, self.config.window);
        }

        let current = map.inner.get(&key).copied();

        let (new_window_start, new_count) = match current {
            // Entry exist and has not expired
            // Check count, reject if too high, increment if not
            Some((window_start, count))
                if now.signed_duration_since(window_start) < self.config.window =>
            {
                if count >= self.config.max_requests {
                    // The request limit has been reached, reject the request
                    let reset_time = window_start + self.config.window;
                    let retry_after = reset_time - now;

                    return Err(reject::custom(RateLimitRejection {
                        retry_after,
                        limit: self.config.max_requests,
                        reset_time,
                        retry_after_format: self.config.retry_after_format.clone(),
                    }));
                } else {
                    // The limit has NOT been reached yet, increment the count and continue
                    (window_start, count + 1)
                }
            }
            // The entry does not exist OR the window has expired
            // Reset
            _ => {
                // FIXME: I'm wondering if there is a better solutiion
                //
                // The issue here is, if I don't limit the map's length, I leave the door open for unbounded memory growth.
                // If I limit the size of the map, since it is cleaned up every `config.window` seconds, there is potentnial for entries dead since at max `config.window` seconds to still be in the map, in other words, a 'full' map might have a lot of dead entries.
                // So I need to force run a cleanup
                // 
                // But if the map is full, this would run for every "new" (not in the map) request
                // Which is 1. inefficient, 2. could be used to DOS the server
                //
                // So, do I add a timer ? if so, what time ? 1/10 of the window ? 5 seconds ? custom ?
                // 1/10 window or 5 seconds is arbitrary and won't be perfect for everyone, custom would mean yet another config parametter
                //
                // Fuck it, 5 seconds it is
                
                // If the map's full and the last cleanup was more than a tenth of the window, cleanup again
                if let Some(max_length) = self.config.internal_map_max_length
                    && map.inner.len() >= max_length
                    // Add a timer of 5 seconds between force cleanup to limit potential DOS attacks
                    && now - map.last_cleanup > TimeDelta::seconds(5)
                {
                    Self::cleanup_internal_map(&mut map, now, self.config.window);

                    if map.inner.len() >= max_length {
                        return Err(reject::custom(rejection::RateLimitCapacityRejection));
                    }
                }

                (now, 1)
            }
        };

        // This updates the entry if it exists, insert otherwise
        map.inner.insert(key, (new_window_start, new_count));

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

        RateLimitInfo {
            limit: self.config.max_requests,
            remaining,
            reset_timestamp: reset_time.timestamp(),
            internal_map_len: map_len,
            last_cleanup_time,
        }
    }
}

/// Creates a rate limiting filter with the given configuration
pub fn with_rate_limit(
    config: RateLimitConfig,
) -> impl Filter<Extract = (RateLimitInfo,), Error = Rejection> + Clone {
    fn ip_header_filter(ip_header: &'static str) -> BoxedFilter<(RateLimitKey,)> {
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
                        })
                        .map(RateLimitKey::Ip)
                        // Requests for which no valid address can be extracted share one bucket.
                        // This intentionally applies a global limit to unidentified requests.
                        .unwrap_or(RateLimitKey::Unknown)
                },
            ))
            .boxed()
    }

    fn remote_addr_filter() -> BoxedFilter<(RateLimitKey,)> {
        warp::filters::addr::remote()
            .map(move |addr: Option<std::net::SocketAddr>| {
                addr.map(|addr| RateLimitKey::Ip(addr.ip()))
                    // Requests for which no valid address can be extracted share one bucket.
                    // This intentionally applies a global limit to unidentified requests.
                    .unwrap_or(RateLimitKey::Unknown)
            })
            .boxed()
    }

    // I thought about adding a way to use one and fallback on the other, but idk how to do that with warp's typesystem
    // filter.or(other_filter) does not match the BoxedFilter<(String,)> type
    let ip_filter = match config.ip_extraction_method {
        config::IpExtractionMethod::Header(ip_header) => ip_header_filter(ip_header),
        config::IpExtractionMethod::RemoteAddr => remote_addr_filter(),
    };
    let rate_limiter = RateLimiter::new(config);

    warp::any()
        .map(move || rate_limiter.clone())
        .and(ip_filter)
        .and_then(|rate_limiter: RateLimiter, ip: RateLimitKey| async move {
            rate_limiter.check_rate_limit(ip).await
        })
}

/// Adds rate limit headers to a response
pub fn add_rate_limit_headers(
    headers: &mut HeaderMap,
    info: &RateLimitInfo,
) -> Result<(), RateLimitError> {
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
