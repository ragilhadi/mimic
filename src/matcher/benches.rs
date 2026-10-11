//! Coarse micro-benchmarks backing the numbers in `ADVANCED_MATCHING.md`'s
//! "Performance Notes". Excluded from the normal suite; reproduce with:
//!
//! ```text
//! cargo test --release -- --ignored --nocapture
//! ```

use super::*;
use std::time::Instant;

#[test]
#[ignore]
fn bench_regex_matcher() {
    let pattern = "^[A-Za-z0-9]{32}$";
    let value = "abcdefghijklmnopqrstuvwxyz012345";
    let n = 100_000;

    let start = Instant::now();
    for _ in 0..n {
        let re = Regex::new(pattern).unwrap();
        assert!(re.is_match(value));
    }
    let uncached = start.elapsed();

    let start = Instant::now();
    for _ in 0..n {
        let re = compiled_regex(pattern).unwrap();
        assert!(re.is_match(value));
    }
    let cached_time = start.elapsed();

    println!(
        "regex: uncached {:?}/op, cached {:?}/op",
        uncached / n,
        cached_time / n
    );
}

#[test]
#[ignore]
fn bench_path_pattern() {
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for i in 0..20 {
        let path = format!("/bench{}/:id", i);
        mocks.insert(
            format!("GET:{}", path),
            vec![MockConfig {
                method: "GET".to_string(),
                path,
                status: 200,
                response: serde_json::json!({}),
                consume_body: false,
                query_params: None,
                headers: None,
                body: None,
                delay_ms: None,
                response_headers: None,
                source: None,
                sequence: None,
                tags: Vec::new(),
                response_file: None,
                template: None,
                response_bytes: None,
                template_escape: None,
            }],
        );
    }
    mocks.insert(
        "GET:/bench-exact".to_string(),
        vec![MockConfig {
            method: "GET".to_string(),
            path: "/bench-exact".to_string(),
            status: 200,
            response: serde_json::json!({}),
            consume_body: false,
            query_params: None,
            headers: None,
            body: None,
            delay_ms: None,
            response_headers: None,
            source: None,
            sequence: None,
            tags: Vec::new(),
            response_file: None,
            template: None,
            response_bytes: None,
            template_escape: None,
        }],
    );

    let n = 50_000;
    let ctx = RequestContext::new("GET".to_string(), "/bench-exact".to_string());
    let start = Instant::now();
    for _ in 0..n {
        assert!(find_matching_mock(&ctx, &mocks, None).is_some());
    }
    println!(
        "exact match with 20 pattern routes loaded: {:?}/op",
        start.elapsed() / n
    );

    let ctx = RequestContext::new("GET".to_string(), "/bench19/42".to_string());
    let start = Instant::now();
    for _ in 0..n {
        assert!(find_matching_mock(&ctx, &mocks, None).is_some());
    }
    println!(
        "pattern match with 20 pattern routes loaded: {:?}/op",
        start.elapsed() / n
    );

    // Approximates the old per-request cost: recompiling all 20 templates.
    let templates: Vec<String> = (0..20).map(|i| format!("/bench{}/:id", i)).collect();
    let start = Instant::now();
    for _ in 0..n {
        for t in &templates {
            assert!(build_path_pattern(t).is_some());
        }
    }
    println!(
        "recompiling 20 path templates: {:?}/op",
        start.elapsed() / n
    );
}
