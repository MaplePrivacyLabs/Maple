//! The static Noise key of a host or a device.
//!
//! One X25519 key pair per host and per device, generated on first use and
//! kept at mode 0600. The public key is the identity: a host is known to
//! its clients by it, and a device to its hosts.

use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

const ENGINE: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Clone)]
pub struct StaticKey {
    private: [u8; 32],
    public: [u8; 32],
}

impl std::fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticKey")
            .field("public", &self.public_id())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredKey {
    private: String,
    public: String,
}

impl StaticKey {
    pub fn generate() -> Result<Self, String> {
        let keypair = snow::Builder::new(
            crate::noise::SESSION_PATTERN
                .parse()
                .map_err(|error| format!("noise pattern: {error}"))?,
        )
        .generate_keypair()
        .map_err(|error| format!("cannot generate a key: {error}"))?;
        Ok(Self {
            private: to_array(&keypair.private)?,
            public: to_array(&keypair.public)?,
        })
    }

    /// The key at `path`, generated and saved there when there is none.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let stored: StoredKey = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("{} is not a key file: {error}", path.display()))?;
                Ok(Self {
                    private: decode_key(&stored.private)?,
                    public: decode_key(&stored.public)?,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate()?;
                maple_agent::private_file::write_private_json(
                    path,
                    &StoredKey {
                        private: encode_key(&key.private),
                        public: encode_key(&key.public),
                    },
                )
                .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
                Ok(key)
            }
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn private(&self) -> &[u8; 32] {
        &self.private
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    /// The public key as the string identity used everywhere else.
    pub fn public_id(&self) -> String {
        encode_key(&self.public)
    }
}

pub fn encode_key(bytes: &[u8]) -> String {
    ENGINE.encode(bytes)
}

pub fn decode_key(text: &str) -> Result<[u8; 32], String> {
    let bytes = ENGINE
        .decode(text.trim())
        .map_err(|error| format!("not a key: {error}"))?;
    to_array(&bytes)
}

fn to_array(bytes: &[u8]) -> Result<[u8; 32], String> {
    bytes
        .try_into()
        .map_err(|_| format!("a key is 32 bytes, not {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_persist_and_encode_round_trip() {
        let dir = std::env::temp_dir().join(format!("maple-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.json");
        let first = StaticKey::load_or_create(&path).unwrap();
        let again = StaticKey::load_or_create(&path).unwrap();
        assert_eq!(first.public(), again.public());
        assert_eq!(first.private(), again.private());
        assert_eq!(decode_key(&first.public_id()).unwrap(), *first.public());
        assert!(decode_key("short").is_err());
        assert_ne!(StaticKey::generate().unwrap().public(), first.public());
        let _ = std::fs::remove_dir_all(dir);
    }
}
