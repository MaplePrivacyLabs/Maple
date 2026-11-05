use serde::{Deserialize, Serialize};

/// The application that configured an SDK client.
///
/// This optional, client-reported context is sent inside encrypted requests.
/// It does not select credentials, grant access, or change billing policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientApp {
    /// Maple Chat, including requests through its embedded proxy.
    Chat,
    /// The standalone Maple proxy.
    Proxy,
    /// Maple Work, including requests through its embedded proxy.
    Work,
}

impl ClientApp {
    /// The canonical value carried in encrypted request metadata.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Proxy => "proxy",
            Self::Work => "work",
        }
    }
}
