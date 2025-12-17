#![forbid(unsafe_code)]
//! This crate provides RFC 6585 compliant in-memory rate limiting with
//! configurable windows and limits as lightweight middleware for
//! Warp web applications.
//!
//! It provides a Filter you add to your routes that exposes rate-limiting
//! information to your handlers, and a Rejection Type for error recovery.
//!
//! It does not yet provide persistence, nor is the HashMap that stores IPs
//! bounded. Both of these may be changed in a future version.
//!
//! # Quickstart
//!
//! 1. Include the crate:
//!
//! `cargo add warp-rate-limit`
//!
//! 2. Define one or more rate limit configurations. Following are some
//! examples of available builder methods. The variable names are arbitrary:
//!
//! ```rust,no_run,ignore
//! // Limit: 60 requests per 60 Earth seconds
//! let public_routes_rate_limit = RateLimitConfig::default();
//!
//! // Limit: 100 requests per 60 Earth seconds
//! let parter_routes_rate_limit = RateLimitConfig::max_per_minute(100);
//!
//! // Limit: 10 requests per 20 Earth seconds
//! let static_route_limit = RateLimitConfig::max_per_window(10,20);
//! ```
//!
//! 3. Use rate limiting information in request handler. If you don't want
//! to use rate-limiting information related to the IP address associated
//! with this request, you can skip this part.
//!
//! ```rust,no_run,ignore
//! // Example route handler
//! async fn hande_request(rate_limit_info: RateLimitInfo) -> Result<impl Reply, Rejection> {
//!     // Create a base response
//!     let mut response = warp::reply::with_status(
//!         "Hello world",
//!         StatusCode::OK
//!     ).into_response();
//!
//!     // Optionally add rate limit headers to your response.
//!     if let Err(e) = add_rate_limit_headers(response.headers_mut(), &rate_limit_info) {
//!         match e {
//!             RateLimitError::HeaderError(e) => {
//!                 eprintln!("Failed to set rate limit headers due to invalid value: {}", e);
//!             }
//!             RateLimitError::Other(e) => {
//!                 eprintln!("Unexpected error setting rate limit headers: {}", e);
//!             }
//!         }
//!     }
//!
//!     // You could also replace the above `if let Err(e)` block with:
//!     // let _ = add_rate_limit_headers(response.headers_mut(), &rate_limit_info);
//!
//!     Ok(response)
//! }
//! ```
//!
//! 4. Handle rate limit errors in your rejection handler:
//!
//! ```rust,no_run,ignore
//! // Example rejection handler
//! async fn handle_rejection(rejection: Rejection) -> Result<impl Reply, Infallible> {
//!     // Somewhere in your rejection handling:
//!     if let Some(rate_limit_rejection) = rejection.find::<RateLimitRejection>() {
//!         // We have a rate limit rejection -- so let's get some info about it:
//!         let info = get_rate_limit_info(rate_limit_rejection);
//!
//!         // Let's use that info to create a response:
//!         let message = format!(
//!             "Rate limit exceeded. Try again after {}.",
//!             info.retry_after
//!         );
//!
//!         // Let's build that response:
//!         let mut response = warp::reply::with_status(
//!             message,
//!             StatusCode::TOO_MANY_REQUESTS
//!         ).into_response();
//!
//!         // Then, let's add the rate-limiting headers to that response:
//!         if let Err(e) = add_rate_limit_headers(response.headers_mut(), &info) {
//!             // Whether or not you use the specific RateLimitError in
//!             // your handler, consider handling errors explicitly here.
//!             // Again, though, you're free to `if let _ = add_rate_limit_headers(...`
//!             // if you don't care about these errors.
//!             match e {
//!                 RateLimitError::HeaderError(e) => {
//!                     eprintln!("Failed to set rate limit headers due to invalid value: {}", e);
//!                 }
//!                 RateLimitError::Other(e) => {
//!                     eprintln!("Unexpected error setting rate limit headers: {}", e);
//!                 }
//!             }
//!         }
//!
//!         Ok(response)    
//!     } else {
//!         // Handle other types of rejections, e.g.
//!         Ok(warp::reply::with_status(
//!             "Internal Server Error",
//!             StatusCode::INTERNAL_SERVER_ERROR,
//!         ).into_response())
//!     }
//! }
//! ```

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{collections::HashMap, net::IpAddr, str::FromStr as _};
use tokio::sync::RwLock;
use warp::{
    http::header::{self, HeaderMap, HeaderValue},
    reject, Filter, Rejection,
};

mod error;
pub use error::RateLimitError;
mod config;
pub use config::{RateLimitConfig, RetryAfterFormat};

// Re-exports
pub use chrono;
pub use serde;

/// Information about the current rate limit status
#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

/// Custom rejection type for rate limiting
#[derive(Debug)]
pub struct RateLimitRejection {
    /// Duration until the client can retry
    pub retry_after: Duration,
    /// Maximum requests allowed in the window
    pub limit: u32,
    /// Unix timestamp when the rate limit resets
    pub reset_time: DateTime<Utc>,
    /// Format to use for Retry-After header
    pub retry_after_format: RetryAfterFormat,
}

impl warp::reject::Reject for RateLimitRejection {}

#[derive(Clone)]
struct RateLimiter {
    state: Arc<RwLock<HashMap<String, (Instant, u32)>>>,
    config: RateLimitConfig,
}

impl RateLimiter {
    fn new(config: RateLimitConfig) -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    async fn check_rate_limit(&self, key: &str) -> Result<RateLimitInfo, Rejection> {
        let mut state = self.state.write().await;
        let now = Instant::now();
        let current = state.get(key).copied();

        match current {
            Some((last_request, count)) => {
                if now.duration_since(last_request) > self.config.window {
                    // Window has passed, reset counter
                    state.insert(key.to_owned(), (now, 1));
                    Ok(self.create_info(self.config.max_requests - 1, now))
                } else if count >= self.config.max_requests {
                    // Rate limit exceeded
                    let retry_after = self.config.window - now.duration_since(last_request);
                    let reset_time = Utc::now() + ChronoDuration::from_std(retry_after).unwrap();

                    Err(reject::custom(RateLimitRejection {
                        retry_after,
                        limit: self.config.max_requests,
                        reset_time,
                        retry_after_format: self.config.retry_after_format.clone(),
                    }))
                } else {
                    // Increment counter
                    state.insert(key.to_owned(), (last_request, count + 1));
                    Ok(self.create_info(self.config.max_requests - (count + 1), last_request))
                }
            }
            None => {
                // First request
                state.insert(key.to_owned(), (now, 1));
                Ok(self.create_info(self.config.max_requests - 1, now))
            }
        }
    }

    fn create_info(&self, remaining: u32, start: Instant) -> RateLimitInfo {
        let reset_time = start + self.config.window;
        let retry_after = match self.config.retry_after_format {
            RetryAfterFormat::HttpDate => {
                (Utc::now() + ChronoDuration::from_std(self.config.window).unwrap()).to_rfc2822()
            }
            RetryAfterFormat::Seconds => self.config.window.as_secs().to_string(),
        };

        RateLimitInfo {
            retry_after,
            limit: self.config.max_requests,
            remaining,
            reset_timestamp: (Utc::now()
                + ChronoDuration::from_std(reset_time.duration_since(start)).unwrap())
            .timestamp(),
            retry_after_format: self.config.retry_after_format.clone(),
        }
    }
}

/// Creates a rate limiting filter with the given configuration
pub fn with_rate_limit(
    config: RateLimitConfig,
) -> impl Filter<Extract = (RateLimitInfo,), Error = Rejection> + Clone {
    // Leaking the ip_header is fine as this function will only be executed at most once per route creation
    let ip_header = config.ip_header.clone().leak();

    let rate_limiter = RateLimiter::new(config);

    // With a service implementation, it is possible to get the original remote() functionality
    // https://github.com/seanmonstar/warp/issues/1127

    warp::filters::any::any()
        .map(move || rate_limiter.clone())
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

/// Gets rate limit information from a rejection
pub fn get_rate_limit_info(rejection: &RateLimitRejection) -> RateLimitInfo {
    let retry_after = match rejection.retry_after_format {
        RetryAfterFormat::HttpDate => rejection.reset_time.to_rfc2822(),
        RetryAfterFormat::Seconds => rejection.retry_after.as_secs().to_string(),
    };

    RateLimitInfo {
        retry_after,
        limit: rejection.limit,
        remaining: 0,
        reset_timestamp: rejection.reset_time.timestamp(),
        retry_after_format: rejection.retry_after_format.clone(),
    }
}
