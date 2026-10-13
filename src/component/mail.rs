//! Address parsing and formatting.
//!
//! The Go original has no SMTP client either — `util/mail.go` is these three helpers and
//! nothing more. Sending mail is a dependency choice (an SMTP crate, a provider's API),
//! not something a web framework should decide.

/// Formats a name and address the way a mail header spells them.
///
/// ```
/// # use rust_web_frame::component::mail::format_address;
/// assert_eq!(format_address("Ops", "ops@example.com"), r#""Ops" <ops@example.com>"#);
/// assert_eq!(format_address("", "ops@example.com"), "ops@example.com");
/// ```
#[must_use]
pub fn format_address(name: &str, address: &str) -> String {
    if name.trim().is_empty() {
        return address.to_owned();
    }
    format!("\"{name}\" <{address}>")
}

/// Splits a recipient into its display name and address.
///
/// Accepts `"Name" <a@b>`, `Name <a@b>` and a bare `a@b`, which is what the Go original
/// does — the three shapes that turn up in a `To:` header.
///
/// # Errors
///
/// Fails when no address can be found in the input.
pub fn parse_address(recipient: &str) -> Result<(String, String), String> {
    let trimmed = recipient.trim();

    if let Some((name, rest)) = trimmed.rsplit_once('<') {
        let address = rest.trim_end_matches('>').trim();
        if is_address(address) {
            let name = name.trim().trim_matches('"').trim();
            return Ok((name.to_owned(), address.to_owned()));
        }
    }

    if is_address(trimmed) {
        return Ok((String::new(), trimmed.to_owned()));
    }

    Err(format!("`{recipient}` is not a usable recipient"))
}

/// Every distinct address in a block of text.
///
/// ```
/// # use rust_web_frame::component::mail::extract_emails;
/// let found = extract_emails("write to ops@example.com or sales@example.com, not ops@example.com");
/// assert_eq!(found, ["ops@example.com", "sales@example.com"]);
/// ```
#[must_use]
pub fn extract_emails(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut start = None;

    for (index, byte) in bytes.iter().enumerate() {
        let is_local = byte.is_ascii_alphanumeric() || b"._%+-".contains(byte);
        match start {
            None if is_local => start = Some(index),
            Some(from) if !is_local => {
                // The run ended; `@` starts the domain half.
                if *byte == b'@'
                    && let Some(address) = take_address(&text[from..])
                    && !found.contains(&address)
                {
                    found.push(address);
                }
                start = None;
            }
            _ => {}
        }
    }

    found
}

/// Reads an address from the start of `text`, if one is there.
fn take_address(text: &str) -> Option<String> {
    let (local, rest) = text.split_once('@')?;
    if local.is_empty() {
        return None;
    }

    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    let domain = &rest[..end];

    // A domain needs a dot with a label on each side, and the last label at least two
    // letters — which is what keeps `user@localhost` and `user@127.0.0.1` out while
    // letting `a@b.co` through.
    let (head, suffix) = domain.rsplit_once('.')?;
    if head.is_empty() || suffix.len() < 2 || !suffix.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(format!("{local}@{domain}"))
}

/// A rough shape check: something before the `@`, and a dotted domain after it.
fn is_address(candidate: &str) -> bool {
    let Some((local, domain)) = candidate.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && take_address(&format!("{local}@{domain}")).is_some()
        && !domain.contains(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_quoted_only_when_there_is_one() {
        assert_eq!(
            format_address("Ops", "ops@example.com"),
            r#""Ops" <ops@example.com>"#
        );
        assert_eq!(format_address("", "ops@example.com"), "ops@example.com");
        assert_eq!(format_address("   ", "ops@example.com"), "ops@example.com");
    }

    #[test]
    fn all_three_recipient_shapes_parse() {
        assert_eq!(
            parse_address(r#""Ops Team" <ops@example.com>"#).unwrap(),
            ("Ops Team".to_owned(), "ops@example.com".to_owned())
        );
        assert_eq!(
            parse_address("Ops <ops@example.com>").unwrap(),
            ("Ops".to_owned(), "ops@example.com".to_owned())
        );
        assert_eq!(
            parse_address("ops@example.com").unwrap(),
            (String::new(), "ops@example.com".to_owned())
        );
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed_at() {
        assert!(parse_address("not an address").is_err());
        assert!(parse_address("").is_err());
        assert!(parse_address("<broken@>").is_err());
    }

    #[test]
    fn addresses_are_extracted_and_deduplicated() {
        let found = extract_emails(
            "ping ops@example.com, then SALES@example.com; ops@example.com again. not-an-address@",
        );
        assert_eq!(found, ["ops@example.com", "SALES@example.com"]);
    }

    #[test]
    fn a_domain_without_a_real_suffix_is_not_an_address() {
        assert!(extract_emails("user@localhost").is_empty());
        assert!(extract_emails("user@127.0.0.1").is_empty());
        assert_eq!(extract_emails("user@example.co"), ["user@example.co"]);
    }

    #[test]
    fn text_with_no_addresses_yields_nothing() {
        assert!(extract_emails("nothing to see here").is_empty());
    }
}
