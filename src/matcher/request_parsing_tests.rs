//! Parsing helpers fixed in #138.

use super::*;
use axum::http::{HeaderMap, HeaderValue};

#[test]
fn decode_path_decodes_each_segment_but_keeps_encoded_slashes() {
    assert_eq!(decode_path("/plain/path"), "/plain/path");
    assert_eq!(decode_path("/a%20b/c%C3%A9"), "/a b/cé");
    assert_eq!(decode_path("/a%2Fb"), "/a%2Fb");
    assert_eq!(decode_path("/a%2fb%20c"), "/a%2Fb c");
    // `+` is a space only in query strings and forms.
    assert_eq!(decode_path("/a+b"), "/a+b");
    // A literal percent that isn't an escape is left alone.
    assert_eq!(decode_path("/100%"), "/100%");
}

#[test]
fn decode_path_keeps_a_segment_that_is_not_valid_utf8() {
    assert_eq!(decode_path("/x/%FF%FE/y"), "/x/%FF%FE/y");
}

#[test]
fn media_type_ignores_case_parameters_and_whitespace() {
    assert_eq!(
        media_type(" Application/JSON ; charset=UTF-8"),
        "application/json"
    );
    assert!(is_json_media_type("application/vnd.api+json"));
    assert!(is_json_media_type("APPLICATION/Problem+JSON"));
    assert!(!is_json_media_type("application/jsonp"));
    assert!(!is_json_media_type("text/plain"));
}

#[test]
fn parse_headers_joins_repeated_headers_in_arrival_order() {
    let mut headers = HeaderMap::new();
    headers.append("x-tag", HeaderValue::from_static("a"));
    headers.append("x-other", HeaderValue::from_static("1"));
    headers.append("x-tag", HeaderValue::from_static("b"));

    let parsed = parse_headers(&headers);

    assert_eq!(parsed["x-tag"], "a, b");
    assert_eq!(parsed["x-other"], "1");
}

#[test]
fn parse_headers_keeps_a_non_utf8_value_lossily() {
    let mut headers = HeaderMap::new();
    headers.insert("x-raw", HeaderValue::from_bytes(b"caf\xe9").unwrap());

    let parsed = parse_headers(&headers);

    assert_eq!(parsed["x-raw"], "caf\u{fffd}");
}
