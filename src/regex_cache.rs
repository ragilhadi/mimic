//! Process-wide memoization for anything expensive to compile from a string.
//!
//! Compiling a regex is orders of magnitude more expensive than running one,
//! so every regex Mimic derives from a mock file — query/header/body matchers
//! and path templates alike — is compiled once per distinct pattern string and
//! shared from then on, rather than being rebuilt on every request.
//!
//! Failures are cached too (as `None`), so an invalid pattern is reported once
//! instead of once per request.

use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use tracing::warn;

/// A memo table mapping a source string to its compiled form, or to `None` if
/// compilation failed.
pub type CompileCache<T> = OnceLock<RwLock<HashMap<String, Option<Arc<T>>>>>;

/// Look `key` up in `cache`, compiling it with `build` on first sight.
///
/// The lock is never held while `build` runs, so a slow compile can't block
/// unrelated lookups. A concurrent duplicate compile is possible but harmless:
/// both produce equivalent values and the first one stored wins.
pub fn cached<T, F>(cache: &CompileCache<T>, key: &str, build: F) -> Option<Arc<T>>
where
    F: FnOnce(&str) -> Option<T>,
{
    let cache = cache.get_or_init(|| RwLock::new(HashMap::new()));

    if let Ok(read) = cache.read() {
        if let Some(hit) = read.get(key) {
            return hit.clone();
        }
    }

    let compiled = build(key).map(Arc::new);

    if let Ok(mut write) = cache.write() {
        write
            .entry(key.to_string())
            .or_insert_with(|| compiled.clone());
    }

    compiled
}

/// True if `key` has already been compiled into `cache`. Test-only probe used
/// to assert that a code path never reached the compiler.
#[cfg(test)]
pub fn is_cached<T>(cache: &CompileCache<T>, key: &str) -> bool {
    cache
        .get()
        .and_then(|c| c.read().ok().map(|r| r.contains_key(key)))
        .unwrap_or(false)
}

/// Compile (or reuse) the regex for `pattern`. Returns `None` if the pattern
/// is invalid, having logged the reason the first time it was seen.
pub fn compiled_regex(pattern: &str) -> Option<Arc<Regex>> {
    static CACHE: CompileCache<Regex> = OnceLock::new();
    cached(&CACHE, pattern, |p| match Regex::new(p) {
        Ok(re) => Some(re),
        Err(e) => {
            warn!("Invalid regex pattern '{}': {}", p, e);
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_same_pattern_compiles_once() {
        static CACHE: CompileCache<Regex> = OnceLock::new();
        static BUILDS: AtomicUsize = AtomicUsize::new(0);

        let build = |p: &str| {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            Regex::new(p).ok()
        };

        for _ in 0..100 {
            let re = cached(&CACHE, r"^\d+$", build).expect("valid pattern");
            assert!(re.is_match("42"));
        }

        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_distinct_patterns_get_distinct_entries() {
        static CACHE: CompileCache<Regex> = OnceLock::new();

        let digits = cached(&CACHE, r"^\d+$", |p| Regex::new(p).ok()).unwrap();
        let letters = cached(&CACHE, r"^[a-z]+$", |p| Regex::new(p).ok()).unwrap();

        assert!(digits.is_match("42"));
        assert!(!digits.is_match("abc"));
        assert!(letters.is_match("abc"));
        assert!(!letters.is_match("42"));
    }

    #[test]
    fn test_invalid_pattern_is_cached_as_none() {
        static CACHE: CompileCache<Regex> = OnceLock::new();
        static BUILDS: AtomicUsize = AtomicUsize::new(0);

        let build = |p: &str| {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            Regex::new(p).ok()
        };

        for _ in 0..10 {
            assert!(cached(&CACHE, "([unclosed", build).is_none());
        }

        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_compiled_regex_returns_shared_handle() {
        let a = compiled_regex(r"^shared-\w+$").unwrap();
        let b = compiled_regex(r"^shared-\w+$").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(a.is_match("shared-value"));
    }

    #[test]
    fn test_compiled_regex_rejects_invalid_pattern() {
        assert!(compiled_regex("([unclosed-group").is_none());
    }
}
