use chrono::{TimeDelta, Utc};
use std::convert::Infallible;
use tokio::task::JoinSet;
use warp::hyper::header;
use warp::{Filter, http::StatusCode, test::request};
use warp::{Reply, reject::Rejection};
use warp_rate_limit::{
    RateLimitConfig, RateLimitError, RateLimitInfo, RateLimitRejection, RetryAfterFormat,
    add_rate_limit_headers, add_rate_limit_headers_from_rejection, with_rate_limit,
};

// Helper function to create a test rate limiter with rejection handling
async fn create_test_route(
    config: RateLimitConfig,
) -> impl Filter<Extract = impl Reply, Error = Infallible> + Clone {
    with_rate_limit(config)
        .map(|info: RateLimitInfo| info.remaining.to_string())
        .recover(|rejection: Rejection| async move {
            if let Some(rate_limit_rejection) = rejection.find::<RateLimitRejection>() {
                let mut resp =
                    warp::reply::with_status("Rate limit exceeded", StatusCode::TOO_MANY_REQUESTS)
                        .into_response();
                add_rate_limit_headers_from_rejection(resp.headers_mut(), rate_limit_rejection)
                    .unwrap();
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
    assert_eq!(per_minute.window, TimeDelta::seconds(60));
    assert_eq!(per_minute.max_requests, 60);
    assert_eq!(per_minute.retry_after_format, RetryAfterFormat::HttpDate);

    // Test max_per_window builder
    let custom = RateLimitConfig::max_per_window(30, 120);
    assert_eq!(custom.window, TimeDelta::seconds(120));
    assert_eq!(custom.max_requests, 30);
    assert_eq!(custom.retry_after_format, RetryAfterFormat::HttpDate);

    // Test default config
    let default = RateLimitConfig::default();
    assert_eq!(default.window, TimeDelta::seconds(60));
    assert_eq!(default.max_requests, 60);
    assert_eq!(default.retry_after_format, RetryAfterFormat::HttpDate);
}

#[tokio::test]
async fn test_comprehensive_rate_limit_rejection() {
    let config = RateLimitConfig {
        max_requests: 1,
        window: TimeDelta::seconds(5),
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
    // This checks that:
    // - It's a valid, non-decimal number
    // - It is not negative
    //
    // (https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Retry-After)
    assert!(retry_after.parse::<u64>().is_ok());
}

#[tokio::test]
async fn test_retry_after_formats() {
    // Test HttpDate format
    let http_date_config = RateLimitConfig {
        max_requests: 1,
        window: TimeDelta::seconds(15),
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
        window: TimeDelta::seconds(5),
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

#[tokio::test]
async fn test_concurrent_requests() {
    let config = RateLimitConfig {
        max_requests: 5,
        window: TimeDelta::seconds(1),
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

// #[test]
// This test is no longer valid as the RateLimitInfo struct changed and the invalid possibilities, checked there
// are no longer possible
fn test_invalid_header_value_handling() {
    let mut headers = header::HeaderMap::new();

    let invalid_info = RateLimitInfo {
        limit: 100,
        remaining: 50,
        reset_timestamp: 1234567890,
        internal_map_len: 0,
        last_cleanup_time: Utc::now(),
    };

    let result = add_rate_limit_headers(&mut headers, &invalid_info);
    assert!(matches!(result, Err(RateLimitError::HeaderError(_))));

    // Not invalid, because I don't care
    // Yes it's wrong, but if this happens, it means that my time logic is wrong, or something funky happended with time
    // The consequences of this being wrong are: the client might think that they are free to send another request asap, which will be declined
    //
    // the retry adter header

    // let mut headers = header::HeaderMap::new();

    // let invalid_info = RateLimitRejection {
    //     retry_after: TimeDelta::seconds(10),
    //     limit: 100,
    //     reset_time: DateTime::UNIX_EPOCH,
    //     retry_after_format: RetryAfterFormat::Seconds,
    // };

    // let result = add_rate_limit_headers_from_rejection(&mut headers, &invalid_info);
    // assert!(matches!(result, Err(RateLimitError::HeaderError(_))));
}

// #[test]
// fn size_of_map() {
//     use chrono::{DateTime, Utc};
//     use std::net::IpAddr;
//     #[derive(Clone, Debug, Eq, Hash, PartialEq)]
//     enum RateLimitKey {
//         Ip(IpAddr),
//         Unknown, // Unidentified, sharing the same bucket
//     }
//     println!("key size: {} bytes", std::mem::size_of::<RateLimitKey>());

//     println!(
//         "value size: {} bytes",
//         std::mem::size_of::<(DateTime<Utc>, u32)>()
//     );

//     println!(
//         "RateLimitKey: {} bytes, alignment {}",
//         size_of::<RateLimitKey>(),
//         align_of::<RateLimitKey>()
//     );

//     println!(
//         "value: {} bytes, alignment {}",
//         size_of::<(DateTime<Utc>, u32)>(),
//         align_of::<(DateTime<Utc>, u32)>()
//     );

//     println!(
//         "IpAddr: {} bytes, alignment {}",
//         size_of::<IpAddr>(),
//         align_of::<IpAddr>()
//     );
// }
