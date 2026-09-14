use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub(crate) const CACHE_NAMESPACE_ROOT_BYTES: usize = 32;
const CACHE_NAMESPACE_ROOT_BASE64_BYTES: usize = 44;
const TINFOIL_CACHE_NAMESPACE_V1_LABEL: &[u8] =
    b"opensecret/provider-cache/tinfoil/user-cache-namespace/v1";
const CONTINUUM_CACHE_NAMESPACE_V1_LABEL: &[u8] =
    b"opensecret/provider-cache/continuum/user-cache-namespace/v1";

type HmacSha256 = Hmac<Sha256>;

/// Client-generated secret material used to derive a provider cache namespace.
///
/// V2 carries this value only inside the encrypted request envelope. The
/// forwarding host never sees it, and the enclave does not persist it.
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct CacheNamespaceRoot([u8; CACHE_NAMESPACE_ROOT_BYTES]);

impl CacheNamespaceRoot {
    #[cfg(test)]
    pub(crate) const fn from_bytes(bytes: [u8; CACHE_NAMESPACE_ROOT_BYTES]) -> Self {
        Self(bytes)
    }

    const fn as_bytes(&self) -> &[u8; CACHE_NAMESPACE_ROOT_BYTES] {
        &self.0
    }
}

impl fmt::Debug for CacheNamespaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheNamespaceRoot([REDACTED])")
    }
}

impl Serialize for CacheNamespaceRoot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = Zeroizing::new(STANDARD.encode(self.0));
        serializer.serialize_str(encoded.as_str())
    }
}

impl<'de> Deserialize<'de> for CacheNamespaceRoot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CacheNamespaceRootVisitor;

        impl de::Visitor<'_> for CacheNamespaceRootVisitor {
            type Value = CacheNamespaceRoot;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("canonical padded base64 encoding of exactly 32 bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() != CACHE_NAMESPACE_ROOT_BASE64_BYTES {
                    return Err(E::custom("cache namespace root must decode to 32 bytes"));
                }

                let mut decoded = Zeroizing::new([0_u8; CACHE_NAMESPACE_ROOT_BYTES]);
                let decoded_bytes = STANDARD
                    .decode_slice(value.as_bytes(), &mut decoded[..])
                    .map_err(|_| E::custom("invalid base64 cache namespace root"))?;
                if decoded_bytes != CACHE_NAMESPACE_ROOT_BYTES {
                    return Err(E::custom("cache namespace root must decode to 32 bytes"));
                }

                let mut canonical = Zeroizing::new([0_u8; CACHE_NAMESPACE_ROOT_BASE64_BYTES]);
                let encoded_bytes = STANDARD
                    .encode_slice(&decoded[..], &mut canonical[..])
                    .map_err(|_| E::custom("invalid cache namespace root"))?;
                if encoded_bytes != value.len() || &canonical[..encoded_bytes] != value.as_bytes() {
                    return Err(E::custom("non-canonical base64 cache namespace root"));
                }

                Ok(CacheNamespaceRoot(*decoded))
            }
        }

        deserializer.deserialize_str(CacheNamespaceRootVisitor)
    }
}

#[derive(Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
struct DerivedCacheNamespaceBytes {
    tinfoil: [u8; 32],
    continuum: [u8; 32],
}

/// Provider-separated cache namespaces derived only after the enclave verifies the
/// request's user identity.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DerivedCacheNamespaces(Arc<DerivedCacheNamespaceBytes>);

impl DerivedCacheNamespaces {
    pub(crate) fn tinfoil_user_cache_secret(&self) -> String {
        hex::encode(self.0.tinfoil)
    }

    pub(crate) fn continuum_cache_salt(&self) -> String {
        hex::encode(self.0.continuum)
    }
}

impl fmt::Debug for DerivedCacheNamespaces {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DerivedCacheNamespaces([REDACTED])")
    }
}

pub(crate) fn derive_provider_cache_namespaces(
    root: &CacheNamespaceRoot,
    verified_user_id: Uuid,
) -> DerivedCacheNamespaces {
    DerivedCacheNamespaces(Arc::new(DerivedCacheNamespaceBytes {
        tinfoil: derive_cache_namespace(root, verified_user_id, TINFOIL_CACHE_NAMESPACE_V1_LABEL),
        continuum: derive_cache_namespace(
            root,
            verified_user_id,
            CONTINUUM_CACHE_NAMESPACE_V1_LABEL,
        ),
    }))
}

/// Legacy clients have no private cache root. Keep their Continuum namespaces
/// stable within this backend process, but secret from the host and other users.
/// Restarting the process intentionally cold-starts these namespaces.
pub(crate) fn legacy_continuum_cache_salt(verified_user_id: Uuid) -> String {
    static ROOT: OnceLock<CacheNamespaceRoot> = OnceLock::new();
    let root = ROOT.get_or_init(|| CacheNamespaceRoot(crate::encrypt::generate_random()));
    let bytes = Zeroizing::new(derive_cache_namespace(
        root,
        verified_user_id,
        CONTINUUM_CACHE_NAMESPACE_V1_LABEL,
    ));
    hex::encode(bytes.as_slice())
}

fn derive_cache_namespace(
    root: &CacheNamespaceRoot,
    verified_user_id: Uuid,
    label: &[u8],
) -> [u8; 32] {
    let mut hmac = HmacSha256::new_from_slice(root.as_bytes())
        .expect("HMAC-SHA256 accepts cache namespace roots of any length");
    hmac.update(label);
    hmac.update(&[0]);
    hmac.update(verified_user_id.as_bytes());

    let bytes = Zeroizing::new(<[u8; 32]>::from(hmac.finalize().into_bytes()));
    *bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn cache_root_wire_encoding_is_canonical_and_redacted() {
        let root = CacheNamespaceRoot::from_bytes([0x5a; 32]);
        let encoded = serde_json::to_string(&root).unwrap();
        let decoded: CacheNamespaceRoot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, root);
        assert_eq!(format!("{root:?}"), "CacheNamespaceRoot([REDACTED])");

        let unpadded = encoded.replace('=', "");
        assert!(serde_json::from_str::<CacheNamespaceRoot>(&unpadded).is_err());
        let wrong_length = format!("\"{}\"", STANDARD.encode([0x5a; 31]));
        assert!(serde_json::from_str::<CacheNamespaceRoot>(&wrong_length).is_err());
    }

    #[test]
    fn derivation_is_stable_but_separated_by_user_and_root() {
        let root = CacheNamespaceRoot::from_bytes([0x42; 32]);
        let user = Uuid::from_u128(1);
        let first = derive_provider_cache_namespaces(&root, user);
        assert_eq!(first, derive_provider_cache_namespaces(&root, user));
        assert_ne!(
            first,
            derive_provider_cache_namespaces(&root, Uuid::from_u128(2))
        );
        assert_ne!(
            first,
            derive_provider_cache_namespaces(&CacheNamespaceRoot::from_bytes([0x24; 32]), user)
        );
        assert_eq!(first.tinfoil_user_cache_secret().len(), 64);
        assert_eq!(first.continuum_cache_salt().len(), 64);
        assert_ne!(
            first.tinfoil_user_cache_secret(),
            first.continuum_cache_salt()
        );
        assert_eq!(format!("{first:?}"), "DerivedCacheNamespaces([REDACTED])");
    }

    #[test]
    fn provider_namespace_vectors_preserve_tinfoil_and_separate_continuum() {
        // Independent Python hmac/hashlib vectors: root = 0x42 * 32, UUID = 1.
        let namespaces = derive_provider_cache_namespaces(
            &CacheNamespaceRoot::from_bytes([0x42; 32]),
            Uuid::from_u128(1),
        );
        assert_eq!(
            namespaces.tinfoil_user_cache_secret(),
            "727c52e95f73161355f9075deedfc6590618de3f17080809ceba096136fd3375"
        );
        assert_eq!(
            namespaces.continuum_cache_salt(),
            "401dd4e7d9085fb325570cbfb84c29a5b27e4494c8f56c5765bf3a379bbd09b4"
        );
    }

    #[test]
    fn continuum_namespace_is_separated_by_user_and_root() {
        let root = CacheNamespaceRoot::from_bytes([0x42; 32]);
        let user = Uuid::from_u128(1);
        let salt = derive_provider_cache_namespaces(&root, user).continuum_cache_salt();
        assert_ne!(
            salt,
            derive_provider_cache_namespaces(&root, Uuid::from_u128(2)).continuum_cache_salt()
        );
        assert_ne!(
            salt,
            derive_provider_cache_namespaces(&CacheNamespaceRoot::from_bytes([0x24; 32]), user)
                .continuum_cache_salt()
        );
    }

    #[test]
    fn legacy_continuum_namespace_is_stable_and_user_bound() {
        let user = Uuid::from_u128(1);
        let salt = legacy_continuum_cache_salt(user);
        assert_eq!(salt.len(), 64);
        assert_eq!(salt, legacy_continuum_cache_salt(user));
        assert_ne!(salt, legacy_continuum_cache_salt(Uuid::from_u128(2)));
        assert_ne!(salt, hex::encode(Sha256::digest(user.as_bytes())));
    }
}
