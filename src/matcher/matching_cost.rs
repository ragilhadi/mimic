//! Timing probe for per-request matching cost (#139). Ignored by default; run
//! with `cargo test --release -- --ignored --nocapture matching_cost`.

use super::*;
use crate::types::MockConfig;
use std::time::Instant;

fn big_mock(method: &str, path: &str) -> MockConfig {
    MockConfig {
        method: method.to_string(),
        path: path.to_string(),
        status: 200,
        // A response with some weight, so a per-candidate clone shows up.
        response: serde_json::json!({
            "items": (0..40).map(|i| serde_json::json!({"id": i, "name": format!("item {i}"), "tags": ["a","b","c"]})).collect::<Vec<_>>()
        }),
        ..Default::default()
    }
}

#[test]
#[ignore]
fn matching_cost() {
    let mut mocks: HashMap<String, Vec<MockConfig>> = HashMap::new();
    for i in 0..500 {
        let m = big_mock("GET", &format!("/exact/{i}"));
        mocks
            .entry(crate::types::create_mock_key(&m.method, &m.path))
            .or_default()
            .push(m);
        let m = big_mock("GET", &format!("/p{i}/:id"));
        mocks
            .entry(crate::types::create_mock_key(&m.method, &m.path))
            .or_default()
            .push(m);
    }
    // Several competing mocks on one path: the per-candidate clone case.
    for _ in 0..10 {
        let m = big_mock("GET", "/shared");
        mocks
            .entry(crate::types::create_mock_key(&m.method, &m.path))
            .or_default()
            .push(m);
    }

    let time = |label: &str, path: &str| {
        let ctx = RequestContext::new("GET".into(), path.into());
        let n = 5000;
        let start = Instant::now();
        for _ in 0..n {
            std::hint::black_box(find_matching_mock(&ctx, &mocks, None));
        }
        println!(
            "{label:<28} {:>8.1} µs/request",
            start.elapsed().as_secs_f64() * 1e6 / n as f64
        );
    };
    time("exact hit", "/exact/250");
    time("shared path (10 candidates)", "/shared");
    time("pattern hit (500 patterns)", "/p499/7");
    time("miss (scans every key)", "/nothing/here");
}
