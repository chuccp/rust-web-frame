//! Percent-encoding for URL components.

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

/// Percent-decodes a URL component. `+` is left alone — callers decoding a form body
/// translate it first.
#[must_use]
pub fn percent_decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// Percent-encodes a value for use in a query string.
#[must_use]
pub fn percent_encode(value: &str) -> String {
    utf8_percent_encode(value, QUERY_SET).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_handles_utf8() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("%E5%8C%97%E4%BA%AC"), "北京");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn encoding_escapes_reserved_characters() {
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_decode(&percent_encode("北京/x")), "北京/x");
    }
}
