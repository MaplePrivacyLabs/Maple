//! Shared safety boundaries for untrusted web-search metadata and page content.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use pulldown_cmark_to_cmark::cmark;
use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

pub(crate) const MAX_PUBLIC_URL_CHARS: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PublicUrlError {
    #[error("URL exceeds {MAX_PUBLIC_URL_CHARS} characters")]
    TooLong,
    #[error("URL is invalid")]
    Invalid,
    #[error("URL must use HTTPS")]
    NotHttps,
    #[error("URL must not contain credentials")]
    Credentials,
    #[error("URL must include a public host")]
    NonPublicHost,
}

/// Normalize a public HTTPS URL without resolving its hostname locally.
///
/// Kagi performs the remote fetch, so a local DNS lookup would inspect the
/// enclave's network rather than Kagi's and would add a DNS rebinding race.
/// Lexical hostname checks and comprehensive literal-IP checks still keep
/// local, private, reserved, and common cloud-metadata targets out of requests.
pub(crate) fn normalize_public_https_url(raw_url: &str) -> Result<String, PublicUrlError> {
    if raw_url.chars().count() > MAX_PUBLIC_URL_CHARS {
        return Err(PublicUrlError::TooLong);
    }
    if raw_url.chars().any(char::is_control) {
        return Err(PublicUrlError::Invalid);
    }

    let mut url = Url::parse(raw_url).map_err(|_| PublicUrlError::Invalid)?;
    if url.scheme() != "https" {
        return Err(PublicUrlError::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PublicUrlError::Credentials);
    }
    validate_public_host(url.host())?;

    url.set_fragment(None);
    if url.port() == Some(443) {
        url.set_port(None).map_err(|_| PublicUrlError::Invalid)?;
    }
    Ok(url.into())
}

fn validate_public_host(host: Option<Host<&str>>) -> Result<(), PublicUrlError> {
    match host {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            let is_single_label = !domain.contains('.');
            let is_private_name = matches!(
                domain.as_str(),
                "localhost"
                    | "localdomain"
                    | "metadata"
                    | "instance-data"
                    | "metadata.google.internal"
            ) || domain.ends_with(".localhost")
                || domain.ends_with(".local")
                || domain.ends_with(".internal")
                || domain.ends_with(".home.arpa");

            if domain.is_empty() || is_single_label || is_private_name {
                return Err(PublicUrlError::NonPublicHost);
            }
        }
        Some(Host::Ipv4(address)) if is_non_public_ipv4(address) => {
            return Err(PublicUrlError::NonPublicHost);
        }
        Some(Host::Ipv6(address)) if is_non_public_ipv6(address) => {
            return Err(PublicUrlError::NonPublicHost);
        }
        Some(_) => {}
        None => return Err(PublicUrlError::NonPublicHost),
    }
    Ok(())
}

fn is_non_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 168 && octets[1] == 63 && octets[2] == 129 && octets[3] == 16)
        || (octets[0] == 192 && octets[1] == 0 && matches!(octets[2], 0 | 2))
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn is_non_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address.to_ipv4().is_some_and(is_non_public_ipv4)
        || embedded_6to4_ipv4(address).is_some_and(is_non_public_ipv4)
        || embedded_well_known_nat64_ipv4(address).is_some_and(is_non_public_ipv4)
        || address.is_loopback()
        || address.is_unspecified()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || address.is_multicast()
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001)
        || (segments[0] == 0x2001 && matches!(segments[1], 0x0000 | 0x0db8))
}

fn embedded_6to4_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = address.segments();
    if segments[0] != 0x2002 {
        return None;
    }

    let high = segments[1].to_be_bytes();
    let low = segments[2].to_be_bytes();
    Some(Ipv4Addr::new(high[0], high[1], low[0], low[1]))
}

fn embedded_well_known_nat64_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    const WELL_KNOWN_PREFIX: [u8; 12] = [
        0x00, 0x64, 0xff, 0x9b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let octets = address.octets();
    if !octets.starts_with(&WELL_KNOWN_PREFIX) {
        return None;
    }

    Some(Ipv4Addr::new(
        octets[12], octets[13], octets[14], octets[15],
    ))
}

/// Remove Markdown and raw-HTML image embeds while preserving alt text,
/// ordinary links, code spans, fenced code blocks, and textual HTML content.
pub(crate) fn strip_image_embeds(markdown: &str) -> String {
    let events = Parser::new_ext(markdown, Options::all()).filter_map(|event| match event {
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Html(html) => {
            let text = strip_raw_html_tags(&html);
            (!text.is_empty()).then(|| Event::Text(text.into()))
        }
        Event::InlineHtml(html) => {
            let text = strip_raw_html_tags(&html);
            (!text.is_empty()).then(|| Event::Text(text.into()))
        }
        event => Some(event),
    });

    let mut sanitized = String::with_capacity(markdown.len());
    cmark(events, &mut sanitized).expect("writing sanitized Markdown to a String cannot fail");
    sanitized
}

fn strip_raw_html_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(relative_start) = html[cursor..].find('<') {
        let tag_start = cursor + relative_start;
        text.push_str(&html[cursor..tag_start]);

        let Some(tag_end) = find_html_tag_end(html, tag_start) else {
            text.push_str(&html[tag_start..]);
            return text;
        };

        cursor = tag_end;
    }

    text.push_str(&html[cursor..]);
    text
}

fn find_html_tag_end(html: &str, tag_start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None;

    for (offset, byte) in bytes[tag_start + 1..].iter().copied().enumerate() {
        match (quote, byte) {
            (Some(active_quote), current) if current == active_quote => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(tag_start + offset + 2),
            _ => {}
        }
    }

    None
}

pub(crate) fn compact_untrusted_markdown(value: &str, max_chars: usize) -> String {
    let sanitized = strip_image_embeds(value).replace("&#32;", " ");
    let (prefix, truncated) = truncate_sanitized_markdown(&sanitized, max_chars);
    let compact = prefix.split_whitespace().collect::<Vec<_>>().join(" ");
    if !truncated {
        return compact;
    }

    const MARKER: &str = "...";
    if max_chars <= MARKER.chars().count() {
        return MARKER.chars().take(max_chars).collect();
    }
    let content_limit = max_chars - MARKER.chars().count();
    let (prefix, _) = truncate_sanitized_markdown(&sanitized, content_limit);
    let compact = prefix.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{compact}{MARKER}")
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (prefix, truncated)
}

/// Bound already-sanitized Markdown without allowing a character cut to turn
/// inert image-looking code into an active Markdown image.
pub(crate) fn truncate_sanitized_markdown(value: &str, max_chars: usize) -> (String, bool) {
    let value_chars = value.chars().count();
    if value_chars <= max_chars {
        return (value.to_string(), false);
    }

    let mut prefix_limit = max_chars;
    loop {
        let (prefix, _) = truncate_chars(value, prefix_limit);
        let sanitized = strip_image_embeds(&prefix);
        let sanitized_chars = sanitized.chars().count();

        if sanitized_chars <= max_chars {
            return (sanitized, true);
        }

        let overflow = sanitized_chars.saturating_sub(max_chars).max(1);
        prefix_limit = prefix_limit.saturating_sub(overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_public_urls_and_rejects_local_metadata_and_reserved_targets() {
        assert_eq!(
            normalize_public_https_url("https://Example.com:443/page#fragment").unwrap(),
            "https://example.com/page"
        );

        for invalid in [
            "http://example.com",
            "https://localhost/page",
            "https://service.internal/page",
            "https://metadata.google.internal/computeMetadata/v1/",
            "https://metadata/latest",
            "https://127.0.0.1/page",
            "https://169.254.169.254/latest/meta-data/",
            "https://168.63.129.16/metadata/instance",
            "https://[::1]/page",
            "https://[2002:7f00:1::]/page",
            "https://[64:ff9b::7f00:1]/page",
            "https://[64:ff9b:1::c000:201]/page",
            "https://[100::1]/page",
            "https://user:password@example.com/page",
            "https://example.com\n.evil.test/page",
            "https://example.com/\tignored",
            "https://example.com/\u{0000}ignored",
        ] {
            assert!(
                normalize_public_https_url(invalid).is_err(),
                "expected {invalid} to be rejected"
            );
        }
    }

    #[test]
    fn image_sanitizer_preserves_text_links_and_code() {
        let sanitized = strip_image_embeds(
            "Before ![chart](data:image/png;base64,AAAA). [source](https://example.com).\n\
             <picture><img src='https://images.example/a.png'></picture> raw text\n\
             `![code](https://images.example/code.png)`",
        );

        assert!(sanitized.contains("chart"));
        assert!(sanitized.contains("[source](https://example.com)"));
        assert!(sanitized.contains("raw text"));
        assert!(sanitized.contains("`![code](https://images.example/code.png)`"));
        assert!(!sanitized.contains("data:image"));
        assert!(!sanitized.contains("images.example/a.png"));
        assert!(!sanitized.contains("<picture"));
        assert!(!sanitized.contains("<img"));
    }

    #[test]
    fn image_sanitizer_bounds_wikilink_event_growth() {
        // A short series of empty-display wikilinks used to expand parser events
        // exponentially before the output truncation could run.
        let markdown = "[[a|]] ".repeat(16);
        let sanitized = strip_image_embeds(&markdown);

        assert!(sanitized.len() < 1_024);
    }

    #[test]
    fn truncation_does_not_reactivate_image_syntax() {
        let image_url = "https://images.example/reactivated.png";
        let sanitized = strip_image_embeds(&format!("`![image]({image_url})` trailing"));
        let closing_backtick = sanitized.rfind('`').unwrap();
        let (bounded, truncated) = truncate_sanitized_markdown(&sanitized, closing_backtick);

        assert!(truncated);
        assert!(!bounded.contains(image_url));
        assert!(!Parser::new_ext(&bounded, Options::all())
            .any(|event| matches!(event, Event::Start(Tag::Image { .. }))));
    }

    #[test]
    fn compact_metadata_includes_marker_inside_the_hard_bound() {
        let compact = compact_untrusted_markdown(&"x".repeat(100), 10);
        assert_eq!(compact, "xxxxxxx...");
        assert_eq!(compact.chars().count(), 10);
    }
}
