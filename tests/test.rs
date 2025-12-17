use chrono::Utc;
use warp_rate_limit::*;
use std::{convert::Infallible, time::Duration};
use tokio::task::JoinSet;
use warp::{reject::Rejection, Reply};
use warp::{http::StatusCode, test::request, Filter};
use warp::hyper::header;


// Helper function to create a test rate limiter with rejection handling
async fn create_test_route(
    config: RateLimitConfig,
) -> impl Filter<Extract = impl Reply, Error = Infallible> + Clone {
    with_rate_limit(config)
        .map(|info: RateLimitInfo| info.remaining.to_string())
        .recover(|rejection: Rejection| async move {
            if let Some(rate_limit) = rejection.find::<RateLimitRejection>() {
                let info = get_rate_limit_info(rate_limit);
                let mut resp =
                    warp::reply::with_status("Rate limit exceeded", StatusCode::TOO_MANY_REQUESTS)
                        .into_response();
                add_rate_limit_headers(resp.headers_mut(), &info).unwrap();
                Ok(resp)
            } else {
                Ok(
                    warp::reply::with_status("Internal error", StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response(),
                )
            }
        })
}

#[test]
fn test_config_builders() {
    // Test max_per_minute builder
    let per_minute = RateLimitConfig::max_per_minute(60);
    assert_eq!(per_minute.window, Duration::from_secs(60));
    assert_eq!(per_minute.max_requests, 60);
    assert_eq!(per_minute.retry_after_format, RetryAfterFormat::HttpDate);

    // Test max_per_window builder
    let custom = RateLimitConfig::max_per_window(30, 120);
    assert_eq!(custom.window, Duration::from_secs(120));
    assert_eq!(custom.max_requests, 30);
    assert_eq!(custom.retry_after_format, RetryAfterFormat::HttpDate);

    // Test default config
    let default = RateLimitConfig::default();
    assert_eq!(default.window, Duration::from_secs(60));
    assert_eq!(default.max_requests, 60);
    assert_eq!(default.retry_after_format, RetryAfterFormat::HttpDate);
}

#[tokio::test]
async fn test_comprehensive_rate_limit_rejection() {
    let config = RateLimitConfig {
        max_requests: 1,
        window: Duration::from_secs(5),
        retry_after_format: RetryAfterFormat::Seconds,
        ..Default::default()
    };

    let route = create_test_route(config.clone()).await;

    // First request succeeds
    let resp1 = request()
        .remote_addr("127.0.0.1:1234".parse().unwrap())
        .reply(&route)
        .await;
    assert_eq!(resp1.status(), 200);
    assert_eq!(resp1.body(), "0"); // Last remaining request

    // Second request gets rejected with proper headers
    let resp2 = request()
        .remote_addr("127.0.0.1:1234".parse().unwrap())
        .reply(&route)
        .await;

    assert_eq!(resp2.status(), 429);

    // Verify rate limit headers exist and have correct format
    let headers = resp2.headers();
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert!(headers.contains_key("X-RateLimit-Limit"));
    assert!(headers.contains_key("X-RateLimit-Remaining"));
    assert!(headers.contains_key("X-RateLimit-Reset"));

    // Verify header values
    assert_eq!(headers.get("X-RateLimit-Limit").unwrap(), "1");
    assert_eq!(headers.get("X-RateLimit-Remaining").unwrap(), "0");

    // Verify Retry-After is a number of seconds
    let retry_after = headers.get(header::RETRY_AFTER).unwrap().to_str().unwrap();
    assert!(retry_after.parse::<u64>().is_ok());
}

#[tokio::test]
async fn test_retry_after_formats() {
    // Test HttpDate format
    let http_date_config = RateLimitConfig {
        max_requests: 1,
        window: Duration::from_secs(15),
        retry_after_format: RetryAfterFormat::HttpDate,
        ..Default::default()
    };

    let http_date_route = create_test_route(http_date_config).await;

    // Trigger rate limit with HttpDate format
    let _ = request()
        .remote_addr("127.0.0.1:1234".parse().unwrap())
        .reply(&http_date_route)
        .await;

    let resp_http = request()
        .remote_addr("127.0.0.1:1234".parse().unwrap())
        .reply(&http_date_route)
        .await;

    // Verify HttpDate format
    let retry_after_http = resp_http
        .headers()
        .get(header::RETRY_AFTER)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!retry_after_http.is_empty()); // RFC2822 date contains GMT

    // Test Seconds format
    let seconds_config = RateLimitConfig {
        max_requests: 1,
        window: Duration::from_secs(5),
        retry_after_format: RetryAfterFormat::Seconds,
        ..Default::default()
    };

    let seconds_route = create_test_route(seconds_config).await;

    // Trigger rate limit with Seconds format
    let _ = request()
        .remote_addr("127.0.0.2:1234".parse().unwrap())
        .reply(&seconds_route)
        .await;

    let resp_sec = request()
        .remote_addr("127.0.0.2:1234".parse().unwrap())
        .reply(&seconds_route)
        .await;

    // Verify Seconds format
    let retry_after_sec = resp_sec
        .headers()
        .get(header::RETRY_AFTER)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(retry_after_sec.parse::<u64>().is_ok());
    assert!(retry_after_sec.parse::<u64>().unwrap() <= 5);
}

#[test]
fn test_rate_limit_info_extraction() {
    let now = Utc::now();
    let rejection = RateLimitRejection {
        retry_after: Duration::from_secs(60),
        limit: 100,
        reset_time: now,
        retry_after_format: RetryAfterFormat::Seconds,
    };

    let info = get_rate_limit_info(&rejection);

    assert_eq!(info.limit, 100);
    assert_eq!(info.remaining, 0);
    assert_eq!(info.reset_timestamp, now.timestamp());
    assert_eq!(info.retry_after, "60");

    // Test with HttpDate format
    let rejection_http = RateLimitRejection {
        retry_after: Duration::from_secs(60),
        limit: 100,
        reset_time: now,
        retry_after_format: RetryAfterFormat::HttpDate,
    };

    let info_http = get_rate_limit_info(&rejection_http);
    assert!(!info_http.retry_after.is_empty()); // RFC2822 date format
}

#[tokio::test]
async fn test_concurrent_requests() {
    let config = RateLimitConfig {
        max_requests: 5,
        window: Duration::from_secs(1),
        retry_after_format: RetryAfterFormat::Seconds,
        ..Default::default()
    };

    let route = create_test_route(config.clone()).await;
    let mut set = JoinSet::new();

    // Launch 10 concurrent requests
    for _ in 0..10 {
        let route = route.clone();
        set.spawn(async move {
            request()
                .remote_addr("127.0.0.1:1234".parse().unwrap())
                .reply(&route)
                .await
        });
    }

    let mut success_count = 0;
    let mut rate_limited_count = 0;

    while let Some(Ok(resp)) = set.join_next().await {
        match resp.status() {
            StatusCode::OK => success_count += 1,
            StatusCode::TOO_MANY_REQUESTS => rate_limited_count += 1,
            _ => panic!("Unexpected response status"),
        }
    }

    assert_eq!(success_count, 5, "Expected exactly 5 successful requests");
    assert_eq!(
        rate_limited_count, 5,
        "Expected exactly 5 rate-limited requests"
    );
}

#[test]
fn test_invalid_header_value_handling() {
    let mut headers = header::HeaderMap::new();
    let invalid_info = RateLimitInfo {
        retry_after: "invalid\u{0000}characters".to_string(),
        limit: 100,
        remaining: 50,
        reset_timestamp: 1234567890,
        retry_after_format: RetryAfterFormat::Seconds,
    };

    let result = add_rate_limit_headers(&mut headers, &invalid_info);
    assert!(matches!(result, Err(RateLimitError::HeaderError(_))));
}
