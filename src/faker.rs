//! Faker-style random data generators for response templating.
//!
//! Backs the `{{faker.key}}` source handled in [`crate::template`]: `{{faker.uuid}}`,
//! `{{faker.int}}`, `{{faker.int min=1 max=100}}`, `{{faker.bool}}`, `{{faker.name}}`,
//! `{{faker.email}}` and `{{faker.timestamp}}`.
//!
//! Every occurrence is resolved independently, so two `{{faker.uuid}}` expressions in
//! one response produce two different UUIDs — fresh data on every call is the point.
//! Generators are deliberately hand-rolled on top of the `rand` dependency Mimic
//! already carries (used for `delay_ms` ranges) so the binary stays small.

use chrono::Utc;
use rand::{Rng, RngExt};
use std::fmt::Write;

/// Range used by `{{faker.int}}` when no (or no usable) `min`/`max` argument is given.
const DEFAULT_INT_MIN: i64 = 0;
const DEFAULT_INT_MAX: i64 = 1_000_000;

const FIRST_NAMES: [&str; 40] = [
    "Alice", "Amara", "Bob", "Bianca", "Carlos", "Chen", "Diana", "Dmitri", "Elena", "Emeka",
    "Fatima", "Felix", "Grace", "Gustavo", "Hana", "Hugo", "Ines", "Ivan", "Julia", "Jonas",
    "Kamal", "Keiko", "Lars", "Layla", "Marco", "Mei", "Nadia", "Noah", "Olga", "Omar", "Priya",
    "Pablo", "Quinn", "Rosa", "Ravi", "Sofia", "Tomas", "Uma", "Viktor", "Yuki",
];

const LAST_NAMES: [&str; 40] = [
    "Adeyemi",
    "Alvarez",
    "Baumann",
    "Bianchi",
    "Costa",
    "Chen",
    "Dubois",
    "Delgado",
    "Eriksen",
    "Espinoza",
    "Fischer",
    "Ferrari",
    "Garcia",
    "Gupta",
    "Hansen",
    "Haddad",
    "Ibrahim",
    "Ivanov",
    "Jensen",
    "Jimenez",
    "Kowalski",
    "Kimura",
    "Lindqvist",
    "Lopez",
    "Martins",
    "Mbeki",
    "Novak",
    "Nakamura",
    "Okafor",
    "Olsen",
    "Petrov",
    "Pereira",
    "Rossi",
    "Reyes",
    "Silva",
    "Schmidt",
    "Tanaka",
    "Thompson",
    "Vargas",
    "Wagner",
];

/// Resolve a `{{faker.…}}` spec, e.g. `uuid` or `int min=1 max=100`.
///
/// The spec is the template's key: a generator name optionally followed by
/// space-separated `key=value` arguments. An unknown generator resolves to `None`,
/// which the templating layer renders as an empty string — the same as any other
/// unknown template. Malformed arguments degrade to that generator's defaults and
/// never panic.
pub fn resolve(spec: &str) -> Option<String> {
    let mut parts = spec.split_whitespace();
    let value = match parts.next()? {
        "uuid" => uuid_v4(),
        "int" => random_int(parts).to_string(),
        "bool" => rand::rng().random_bool(0.5).to_string(),
        "name" => random_name(),
        "email" => format!("{}@example.com", slugify(&random_name())),
        "timestamp" => Utc::now().to_rfc3339(),
        _ => return None,
    };
    Some(value)
}

/// RFC 4122 version 4 UUID built straight from 16 random bytes, keeping the
/// dependency list flat.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant

    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        // Writing into a String is infallible.
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// Sample an integer, honouring `min=` / `max=` arguments when both bounds end up
/// usable. An unparsable bound, or an inverted range, falls back to the default
/// range rather than failing the response.
fn random_int<'a, I: Iterator<Item = &'a str>>(args: I) -> i64 {
    let mut min = DEFAULT_INT_MIN;
    let mut max = DEFAULT_INT_MAX;
    let mut malformed = false;

    for arg in args {
        let Some((name, raw)) = arg.split_once('=') else {
            malformed = true;
            continue;
        };
        match (name, raw.parse::<i64>()) {
            ("min", Ok(v)) => min = v,
            ("max", Ok(v)) => max = v,
            _ => malformed = true,
        }
    }

    if malformed || min > max {
        min = DEFAULT_INT_MIN;
        max = DEFAULT_INT_MAX;
    }

    rand::rng().random_range(min..=max)
}

fn random_name() -> String {
    let mut rng = rand::rng();
    let first = FIRST_NAMES[rng.random_range(0..FIRST_NAMES.len())];
    let last = LAST_NAMES[rng.random_range(0..LAST_NAMES.len())];
    format!("{} {}", first, last)
}

/// Lowercase a name into an email local part: spaces become dots, anything that
/// isn't alphanumeric is dropped.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() && !out.ends_with('.') && !out.is_empty() {
            out.push('.');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_has_v4_shape() {
        let uuid = resolve("uuid").unwrap();
        assert_eq!(uuid.len(), 36);

        let groups: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(uuid
            .chars()
            .all(|c| c == '-' || (c.is_ascii_hexdigit() && !c.is_ascii_uppercase())));
        // Version nibble is 4, variant nibble is one of 8/9/a/b.
        assert!(groups[2].starts_with('4'));
        assert!(matches!(
            groups[3].chars().next().unwrap(),
            '8' | '9' | 'a' | 'b'
        ));
    }

    #[test]
    fn test_uuids_are_independent() {
        let a = resolve("uuid").unwrap();
        let b = resolve("uuid").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_int_default_range() {
        for _ in 0..100 {
            let n: i64 = resolve("int").unwrap().parse().unwrap();
            assert!((DEFAULT_INT_MIN..=DEFAULT_INT_MAX).contains(&n));
        }
    }

    #[test]
    fn test_int_respects_min_and_max() {
        for _ in 0..100 {
            let n: i64 = resolve("int min=1 max=10").unwrap().parse().unwrap();
            assert!((1..=10).contains(&n), "{} out of range", n);
        }
    }

    #[test]
    fn test_int_single_bound_argument() {
        for _ in 0..100 {
            let n: i64 = resolve("int max=5").unwrap().parse().unwrap();
            assert!((0..=5).contains(&n), "{} out of range", n);
        }
    }

    #[test]
    fn test_int_negative_bounds() {
        for _ in 0..100 {
            let n: i64 = resolve("int min=-10 max=-5").unwrap().parse().unwrap();
            assert!((-10..=-5).contains(&n), "{} out of range", n);
        }
    }

    #[test]
    fn test_int_fixed_range_when_min_equals_max() {
        assert_eq!(resolve("int min=7 max=7").unwrap(), "7");
    }

    #[test]
    fn test_int_malformed_args_fall_back_to_defaults() {
        for spec in [
            "int min=abc",
            "int max=",
            "int min=1 max=oops",
            "int minmax",
            "int step=2",
        ] {
            let n: i64 = resolve(spec)
                .unwrap_or_else(|| panic!("{} did not resolve", spec))
                .parse()
                .unwrap();
            assert!(
                (DEFAULT_INT_MIN..=DEFAULT_INT_MAX).contains(&n),
                "{} produced {}",
                spec,
                n
            );
        }
    }

    #[test]
    fn test_int_inverted_range_falls_back_to_defaults() {
        let n: i64 = resolve("int min=100 max=1").unwrap().parse().unwrap();
        assert!((DEFAULT_INT_MIN..=DEFAULT_INT_MAX).contains(&n));
    }

    #[test]
    fn test_bool_is_true_or_false() {
        let mut seen_true = false;
        let mut seen_false = false;
        for _ in 0..200 {
            match resolve("bool").unwrap().as_str() {
                "true" => seen_true = true,
                "false" => seen_false = true,
                other => panic!("unexpected bool: {}", other),
            }
        }
        assert!(seen_true && seen_false, "bool never varied");
    }

    #[test]
    fn test_name_comes_from_the_embedded_lists() {
        for _ in 0..50 {
            let name = resolve("name").unwrap();
            let (first, last) = name.split_once(' ').expect("name has two parts");
            assert!(FIRST_NAMES.contains(&first), "unknown first name {}", first);
            assert!(LAST_NAMES.contains(&last), "unknown last name {}", last);
        }
    }

    #[test]
    fn test_email_is_slugified_name_at_example_com() {
        for _ in 0..50 {
            let email = resolve("email").unwrap();
            let (local, domain) = email.split_once('@').expect("email has a domain");
            assert_eq!(domain, "example.com");
            assert!(
                local
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'),
                "unexpected local part: {}",
                local
            );
            assert!(local.contains('.'), "expected first.last, got {}", local);
        }
    }

    #[test]
    fn test_timestamp_is_rfc3339_and_parseable() {
        let ts = resolve("timestamp").unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).expect("valid RFC 3339");
        assert_eq!(parsed.to_rfc3339(), ts);
    }

    #[test]
    fn test_unknown_generator_resolves_to_none() {
        assert_eq!(resolve("credit_card"), None);
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("   "), None);
    }

    #[test]
    fn test_extra_whitespace_between_args_is_tolerated() {
        let n: i64 = resolve("int   min=2   max=3").unwrap().parse().unwrap();
        assert!((2..=3).contains(&n));
    }

    #[test]
    fn test_slugify_drops_punctuation_and_collapses_spaces() {
        assert_eq!(slugify("Alice O'Brien"), "alice.obrien");
        assert_eq!(slugify("  Bob  Smith "), "bob.smith");
    }
}
