//! Small helpers shared across modules.

use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Characters `encodeURIComponent` leaves alone: `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// Percent-encode like JavaScript's `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    utf8_percent_encode(value, URI_COMPONENT).to_string()
}

/// Milliseconds since the Unix epoch, as a float like JavaScript's `mtimeMs`.
pub fn system_time_ms(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as f64 * 1e3 + f64::from(d.subsec_nanos()) / 1e6,
        Err(e) => -(e.duration().as_secs_f64() * 1e3),
    }
}

/// The current time in whole milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Human-friendly ordering for paths: case-insensitive first, lowercase before
/// uppercase on ties, close to `String.prototype.localeCompare`.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    let folded = a
        .chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase));
    folded.then_with(|| b.cmp(a))
}

/// Strip leading and trailing `/` from a folder name.
pub fn trim_slashes(value: &str) -> &str {
    value.trim_matches('/')
}

/// Number of Unicode scalar values in `text`, the unit every character limit uses.
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// The first `n` characters of `text`.
pub fn take_chars(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// Format a millisecond timestamp like `Date.prototype.toISOString`.
pub fn iso_timestamp(ms: f64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "Invalid Date".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_uri_component("a b/c&d"), "a%20b%2Fc%26d");
        assert_eq!(encode_uri_component("it's (ok)!*~"), "it's%20(ok)!*~");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
    }

    #[test]
    fn locale_order_ignores_case() {
        let mut items = vec!["b.md", "A.md", "a.md", "C.md"];
        items.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(items, ["a.md", "A.md", "b.md", "C.md"]);
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(iso_timestamp(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(take_chars("héllo", 2), "hé");
    }
}
