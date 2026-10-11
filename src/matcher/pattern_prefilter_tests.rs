use super::*;

/// The prefilter is only a shortcut: it must never turn away a path the
/// compiled template would have matched.
#[test]
fn template_may_match_never_rejects_a_path_the_pattern_matches() {
    let cases = [
        ("/users/:id", "/users/42"),
        ("/users/{id}", "/users/42"),
        ("/:resource/:id", "/orders/7"),
        ("/{a}/x/{b}", "/anything/x/else"),
        ("/api/v1/users/:id/posts/:post", "/api/v1/users/3/posts/9"),
        ("/users/:id/", "/users/42/"),
        ("/:id", "/42"),
    ];
    for (template, path) in cases {
        let pattern = compile_path_pattern(template).expect(template);
        assert!(
            match_path_pattern(&pattern, path).is_some(),
            "{template} should match {path}"
        );
        assert!(template_may_match(template, path), "{template} vs {path}");
    }
}

#[test]
fn template_may_match_rejects_the_obvious_misses() {
    assert!(!template_may_match("/users/:id", "/users/42/extra"));
    assert!(!template_may_match("/users/:id", "/orders/42"));
    assert!(!template_may_match("/users/:id", "/users"));
    assert!(template_may_match("/:any/:id", "/orders/42"));
}

#[test]
fn is_pattern_path_still_recognises_every_parameter_syntax() {
    assert!(is_pattern_path("/users/:id"));
    assert!(is_pattern_path("/users/{id}"));
    assert!(!is_pattern_path("/users/42"));
    assert!(!is_pattern_path("/users/:"));
    assert!(!is_pattern_path("/users/{}"));
}
