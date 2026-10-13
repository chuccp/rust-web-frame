//! String, URL and time helpers.

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// Everything that must be escaped in a query-string component.
const QUERY_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Reports whether `value` is empty or only whitespace.
pub fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// The inverse of [`is_blank`].
pub fn is_not_blank(value: &str) -> bool {
    !is_blank(value)
}

/// Percent-decodes a URL component. `+` is left alone — callers decoding a form body
/// translate it first.
pub fn url_decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// Percent-encodes a value for use in a query string.
pub fn url_encode(value: &str) -> String {
    utf8_percent_encode(value, QUERY_SET).to_string()
}

/// Milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Formats a UTC timestamp the way the Go original does.
pub fn format_time(time: chrono::DateTime<chrono::Utc>, layout: &str) -> String {
    time.format(layout).to_string()
}

/// Returns the UTF-8 byte length of `value`, capped at `max`.
pub fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_detection_ignores_whitespace() {
        assert!(is_blank("   "));
        assert!(is_blank(""));
        assert!(is_not_blank(" a "));
    }

    #[test]
    fn url_decoding_handles_utf8() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("%E5%8C%97%E4%BA%AC"), "北京");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn url_encoding_escapes_reserved_characters() {
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn truncate_never_splits_a_character() {
        let text = "北京";
        assert_eq!(truncate(text, 4), "北");
        assert_eq!(truncate(text, 100), text);
    }

    #[test]
    fn now_millis_is_a_plausible_epoch() {
        assert!(now_millis() > 1_700_000_000_000);
    }
}
