//! The page a browser lands on when a sign-in returns to this computer, for
//! Maple Work's own sign-in and for connecting apps.

/// A full HTTP response holding the page. The text is `'static` so that only
/// fixed outcome text enters the document, never request or account data.
pub fn sign_in_page_response(
    status: &'static str,
    title: &'static str,
    message: &'static str,
) -> String {
    let body = include_str!("sign_in_page.html")
        .replace("{{title}}", title)
        .replace("{{message}}", message);
    // Hashes authorize only the template's exact script/style contents. Update
    // the corresponding hash when editing either inline block in the template.
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'sha256-TT2/B5X4kutghZnTdCndWuEaf6uS6wbgBxuJIAzSdos='; style-src 'sha256-bEU9dVwNDbkfogept6azK2Ull7VrNOlUgl+5AqGnEOA='; frame-ancestors 'none'; base-uri 'none'\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn inline<'a>(page: &'a str, tag: &str) -> &'a str {
        let open = format!("<{tag}>");
        let start = page.find(&open).unwrap() + open.len();
        let end = page.find(&format!("</{tag}>")).unwrap();
        &page[start..end]
    }

    #[test]
    fn the_security_policy_allows_the_pages_own_script_and_style() {
        let response = sign_in_page_response("200 OK", "Title", "Message");
        for tag in ["script", "style"] {
            let digest = Sha256::digest(inline(&response, tag));
            let hash = base64::engine::general_purpose::STANDARD.encode(digest);
            assert!(
                response.contains(&format!("{tag}-src 'sha256-{hash}'")),
                "{tag}"
            );
        }
    }
}
