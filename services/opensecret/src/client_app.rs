//! Optional, self-reported client attribution for usage telemetry.
//!
//! This label never establishes identity, permissions, routing or pricing.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

pub(crate) const CLIENT_APP_HEADER: &str = "x-maple-client-app";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientApp {
    Chat,
    Proxy,
    Work,
}

impl ClientApp {
    /// Accept exactly one canonical logical header. Unrecognized telemetry is
    /// omitted rather than rejecting an otherwise valid authenticated request.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let mut values = headers.get_all(CLIENT_APP_HEADER).iter();
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        match value.to_str().ok()? {
            "chat" => Some(Self::Chat),
            "proxy" => Some(Self::Proxy),
            "work" => Some(Self::Work),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn only_canonical_client_labels_are_attributed() {
        for (label, expected) in [
            ("chat", ClientApp::Chat),
            ("proxy", ClientApp::Proxy),
            ("work", ClientApp::Work),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CLIENT_APP_HEADER, HeaderValue::from_static(label));
            assert_eq!(ClientApp::from_headers(&headers), Some(expected));
            assert_eq!(serde_json::to_value(expected).unwrap(), label);
        }
    }

    #[test]
    fn absent_invalid_and_ambiguous_labels_are_unknown() {
        assert_eq!(ClientApp::from_headers(&HeaderMap::new()), None);
        for label in ["", "Chat", "WORK", " work", "work ", "chat,work", "other"] {
            let mut headers = HeaderMap::new();
            headers.insert(CLIENT_APP_HEADER, HeaderValue::from_str(label).unwrap());
            assert_eq!(ClientApp::from_headers(&headers), None);
        }
        let mut headers = HeaderMap::new();
        headers.append(CLIENT_APP_HEADER, HeaderValue::from_static("work"));
        headers.append(CLIENT_APP_HEADER, HeaderValue::from_static("work"));
        assert_eq!(ClientApp::from_headers(&headers), None);
        let mut headers = HeaderMap::new();
        headers.insert(CLIENT_APP_HEADER, HeaderValue::from_bytes(&[0xff]).unwrap());
        assert_eq!(ClientApp::from_headers(&headers), None);
    }
}
