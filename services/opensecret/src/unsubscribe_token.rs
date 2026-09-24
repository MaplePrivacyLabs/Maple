//! Tokens for email unsubscribe links.
//!
//! A token is `(version, project_id, user_uuid)` sealed with AES-SIV under a
//! key derived from the enclave key. That makes it:
//! - opaque: the link carries no email address or user ID, only an encrypted
//!   value. Tokens are deterministic, so two emails to the same user carry the
//!   same token; that equality is the one thing a token reveals;
//! - tamper-proof: an edited or forged token fails to open;
//! - stable: a user gets the same token in every email, so no rows are stored.
//!
//! Unsubscribe links have to keep working long after an email is sent, so
//! tokens don't expire. Bumping `TOKEN_VERSION` rejects every existing token.
//! That's a cutoff, not a key rotation: recovering from a compromised key
//! means changing the key-derivation labels too.

use aes_siv::aead::{Aead, KeyInit, Payload};
use aes_siv::{Aes256SivAead, Nonce as SivNonce};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use generic_array::GenericArray;
use uuid::Uuid;

use crate::encrypt::{derive_key, EncryptError};

const KEY_INFO_1: &[u8] = b"os.email-unsubscribe-token.k1.v1";
const KEY_INFO_2: &[u8] = b"os.email-unsubscribe-token.k2.v1";
const TOKEN_AAD: &[u8] = b"os.email-unsubscribe-token.v1";
const TOKEN_VERSION: u8 = 1;
const PLAINTEXT_LEN: usize = 1 + 4 + 16;
/// Real tokens are 50 characters. Anything much longer is rejected before
/// decoding.
const MAX_TOKEN_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsubscribeSubject {
    pub project_id: i32,
    pub user_uuid: Uuid,
}

pub fn issue_unsubscribe_token(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
) -> Result<String, EncryptError> {
    seal(root_key, TOKEN_VERSION, project_id, user_uuid)
}

/// Returns the subject for a token this server issued, and `None` for anything
/// else: malformed, edited, sealed under another key, or an old version. Every
/// failure returns the same value, so callers can't report which check failed.
/// (Timing isn't uniform: malformed input returns before decryption.)
pub fn verify_unsubscribe_token(root_key: &[u8], token: &str) -> Option<UnsubscribeSubject> {
    if token.len() > MAX_TOKEN_LEN {
        return None;
    }
    let sealed = URL_SAFE_NO_PAD.decode(token).ok()?;
    let plaintext = cipher(root_key)
        .ok()?
        .decrypt(
            &SivNonce::default(),
            Payload {
                msg: &sealed,
                aad: TOKEN_AAD,
            },
        )
        .ok()?;
    if plaintext.len() != PLAINTEXT_LEN || plaintext[0] != TOKEN_VERSION {
        return None;
    }
    let project_id = i32::from_be_bytes(plaintext[1..5].try_into().ok()?);
    let user_uuid = Uuid::from_slice(&plaintext[5..]).ok()?;
    Some(UnsubscribeSubject {
        project_id,
        user_uuid,
    })
}

fn seal(
    root_key: &[u8],
    version: u8,
    project_id: i32,
    user_uuid: Uuid,
) -> Result<String, EncryptError> {
    let mut plaintext = Vec::with_capacity(PLAINTEXT_LEN);
    plaintext.push(version);
    plaintext.extend_from_slice(&project_id.to_be_bytes());
    plaintext.extend_from_slice(user_uuid.as_bytes());

    let sealed = cipher(root_key)?
        .encrypt(
            &SivNonce::default(),
            Payload {
                msg: &plaintext,
                aad: TOKEN_AAD,
            },
        )
        .map_err(|_| EncryptError::BadData)?;
    Ok(URL_SAFE_NO_PAD.encode(sealed))
}

/// AES-256-SIV takes a 64-byte key: two independent 32-byte halves.
fn cipher(root_key: &[u8]) -> Result<Aes256SivAead, EncryptError> {
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(&derive_key(root_key, KEY_INFO_1)?);
    key[32..].copy_from_slice(&derive_key(root_key, KEY_INFO_2)?);
    Ok(Aes256SivAead::new(GenericArray::from_slice(&key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: [u8; 32] = [7u8; 32];

    #[test]
    fn round_trips_to_the_same_subject() {
        let user_uuid = Uuid::new_v4();
        let token = issue_unsubscribe_token(&ROOT, 42, user_uuid).unwrap();
        assert_eq!(
            verify_unsubscribe_token(&ROOT, &token),
            Some(UnsubscribeSubject {
                project_id: 42,
                user_uuid
            })
        );
    }

    #[test]
    fn is_stable_per_user_and_distinct_across_users_and_projects() {
        let user = Uuid::new_v4();
        let token = issue_unsubscribe_token(&ROOT, 1, user).unwrap();
        assert_eq!(token, issue_unsubscribe_token(&ROOT, 1, user).unwrap());
        assert_ne!(
            token,
            issue_unsubscribe_token(&ROOT, 1, Uuid::new_v4()).unwrap()
        );
        assert_ne!(token, issue_unsubscribe_token(&ROOT, 2, user).unwrap());
    }

    #[test]
    fn does_not_contain_the_user_uuid() {
        let user_uuid = Uuid::new_v4();
        let token = issue_unsubscribe_token(&ROOT, 1, user_uuid).unwrap();
        assert!(!token.contains(&user_uuid.simple().to_string()));
        assert!(!token.contains(&URL_SAFE_NO_PAD.encode(user_uuid.as_bytes())));
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(token.len() <= MAX_TOKEN_LEN);
    }

    /// Every emailed link depends on this exact encoding. If this fails, the
    /// token format changed and previously sent unsubscribe links will break.
    /// The expected value was computed independently with Python's
    /// `cryptography` (HKDF-SHA256, then RFC 5297 AES-SIV with headers
    /// `[TOKEN_AAD, 16 zero bytes]`).
    #[test]
    fn v1_tokens_stay_valid() {
        const V1_TOKEN: &str = "79R1nw6hFKb8OM3PLSrO-ZSjyHPOQQsiDUqwHb2e_sqc-hOfgQ";
        let subject = UnsubscribeSubject {
            project_id: 1,
            user_uuid: Uuid::parse_str("0b8f5a3c-6d1e-4f27-9a44-2c7e1d9b3f60").unwrap(),
        };
        assert_eq!(
            issue_unsubscribe_token(&ROOT, subject.project_id, subject.user_uuid).unwrap(),
            V1_TOKEN
        );
        assert_eq!(verify_unsubscribe_token(&ROOT, V1_TOKEN), Some(subject));
    }

    #[test]
    fn rejects_padded_and_noncanonical_encodings() {
        const V1_TOKEN: &str = "79R1nw6hFKb8OM3PLSrO-ZSjyHPOQQsiDUqwHb2e_sqc-hOfgQ";
        // 37 bytes leave 4 unused bits in the last character; "Q" sets none.
        // "R" encodes the same bytes with a nonzero unused bit.
        let noncanonical = format!("{}R", &V1_TOKEN[..V1_TOKEN.len() - 1]);
        for input in [
            format!("{V1_TOKEN}="),
            format!("{V1_TOKEN}=="),
            noncanonical,
            format!(" {V1_TOKEN}"),
            V1_TOKEN.replace('-', "+").replace('_', "/"),
        ] {
            assert_eq!(verify_unsubscribe_token(&ROOT, &input), None, "{input:?}");
        }
    }

    #[test]
    fn rejects_any_single_character_edit() {
        let token = issue_unsubscribe_token(&ROOT, 1, Uuid::new_v4()).unwrap();
        for (i, original) in token.char_indices() {
            let replacement = if original == 'A' { 'B' } else { 'A' };
            let mut edited = token.clone();
            edited.replace_range(i..i + 1, &replacement.to_string());
            assert_eq!(verify_unsubscribe_token(&ROOT, &edited), None, "index {i}");
        }
    }

    #[test]
    fn rejects_tokens_from_another_key() {
        let token = issue_unsubscribe_token(&[9u8; 32], 1, Uuid::new_v4()).unwrap();
        assert_eq!(verify_unsubscribe_token(&ROOT, &token), None);
    }

    #[test]
    fn rejects_other_versions() {
        let token = seal(&ROOT, TOKEN_VERSION + 1, 1, Uuid::new_v4()).unwrap();
        assert_eq!(verify_unsubscribe_token(&ROOT, &token), None);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        let too_long = "A".repeat(MAX_TOKEN_LEN + 1);
        for input in ["", "abc", "!!!!", "not a token at all", too_long.as_str()] {
            assert_eq!(verify_unsubscribe_token(&ROOT, input), None, "{input:?}");
        }
    }
}
