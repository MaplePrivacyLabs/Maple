use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::encrypt::{
    decrypt_aead_v1, derive_key, derive_key_with_salt, encrypt_aead_v1, AeadKey, CanonicalBytes,
    EncryptError,
};

type HmacSha256 = Hmac<Sha256>;

const AUTH_BINDING_MAC_INFO: &[u8] = b"os.auth-binding-mac.v1";
const CREDENTIAL_LOOKUP_MAC_INFO: &[u8] = b"os.credential-lookup-mac.v1";
const PASSWORD_RESET_CODE_MAC_INFO: &[u8] = b"os.password-reset-code-mac.v1";
const SEED_WRAP_ROOT_INFO: &[u8] = b"os.seed-wrap-root.v1";
const SEED_WRAP_AEAD_KEY_INFO: &[u8] = b"os.seed-wrap-aead-key.v1";

const PASSWORD_AUTH_BINDING_DOMAIN: &str = "os.password-auth-binding.v1";
const PASSWORD_LOOKUP_DOMAIN: &str = "os.password-lookup.v1";
const PASSWORD_RESET_CODE_DOMAIN: &str = "os.password-reset-code.v1";
const OAUTH_AUTH_BINDING_DOMAIN: &str = "os.oauth-auth-binding.v1";
const OAUTH_LOOKUP_DOMAIN: &str = "os.oauth-lookup.v1";
const SEED_WRAP_DOMAIN: &str = "os.seed-wrap.v1";

pub const SEED_WRAP_VERSION_V1: i16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Password,
    OAuth,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::Password => "password",
            CredentialKind::OAuth => "oauth",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordLoginIdentifierKind {
    Email,
    GuestUuid,
}

impl PasswordLoginIdentifierKind {
    fn as_str(self) -> &'static str {
        match self {
            PasswordLoginIdentifierKind::Email => "email",
            PasswordLoginIdentifierKind::GuestUuid => "guest_uuid",
        }
    }
}

pub fn normalize_email_login_identifier(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

pub fn normalize_guest_login_identifier(user_uuid: Uuid) -> String {
    user_uuid.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthBinding([u8; 32]);

impl AuthBinding {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialLookupHash([u8; 32]);

impl CredentialLookupHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub fn compute_password_auth_binding(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
    login_identifier_kind: PasswordLoginIdentifierKind,
    normalized_login_identifier: &str,
    decrypted_password_verifier: &str,
) -> Result<AuthBinding, EncryptError> {
    let mut facts = CanonicalBytes::new(PASSWORD_AUTH_BINDING_DOMAIN);
    facts
        .append_i32(project_id)
        .append_uuid(user_uuid)
        .append_str(login_identifier_kind.as_str())
        .append_str(normalized_login_identifier)
        .append_str(decrypted_password_verifier);

    Ok(AuthBinding(hmac_with_root_domain_key(
        root_key,
        AUTH_BINDING_MAC_INFO,
        &facts.into_bytes(),
    )?))
}

pub fn compute_oauth_auth_binding(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
    provider: &str,
    provider_user_id: &str,
) -> Result<AuthBinding, EncryptError> {
    let mut facts = CanonicalBytes::new(OAUTH_AUTH_BINDING_DOMAIN);
    facts
        .append_i32(project_id)
        .append_uuid(user_uuid)
        .append_str(provider)
        .append_str(provider_user_id);

    Ok(AuthBinding(hmac_with_root_domain_key(
        root_key,
        AUTH_BINDING_MAC_INFO,
        &facts.into_bytes(),
    )?))
}

pub fn password_credential_lookup_hash(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
) -> Result<CredentialLookupHash, EncryptError> {
    let mut facts = CanonicalBytes::new(PASSWORD_LOOKUP_DOMAIN);
    facts.append_i32(project_id).append_uuid(user_uuid);

    Ok(CredentialLookupHash(hmac_with_root_domain_key(
        root_key,
        CREDENTIAL_LOOKUP_MAC_INFO,
        &facts.into_bytes(),
    )?))
}

pub fn oauth_credential_lookup_hash(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
    provider: &str,
    provider_user_id: &str,
) -> Result<CredentialLookupHash, EncryptError> {
    let mut facts = CanonicalBytes::new(OAUTH_LOOKUP_DOMAIN);
    facts
        .append_i32(project_id)
        .append_uuid(user_uuid)
        .append_str(provider)
        .append_str(provider_user_id);

    Ok(CredentialLookupHash(hmac_with_root_domain_key(
        root_key,
        CREDENTIAL_LOOKUP_MAC_INFO,
        &facts.into_bytes(),
    )?))
}

pub fn password_reset_code_mac(
    root_key: &[u8],
    project_id: i32,
    user_uuid: Uuid,
    alphanumeric_code: &str,
) -> Result<[u8; 32], EncryptError> {
    let mut facts = CanonicalBytes::new(PASSWORD_RESET_CODE_DOMAIN);
    facts
        .append_i32(project_id)
        .append_uuid(user_uuid)
        .append_str(alphanumeric_code);

    hmac_with_root_domain_key(root_key, PASSWORD_RESET_CODE_MAC_INFO, &facts.into_bytes())
}

pub fn encrypt_seed_v1(
    root_key: &[u8],
    plaintext_seed: &[u8],
    user_uuid: Uuid,
    project_id: i32,
    credential_kind: CredentialKind,
    auth_binding: &AuthBinding,
) -> Result<Vec<u8>, EncryptError> {
    let aead_key = derive_seed_wrap_aead_key(root_key, auth_binding)?;
    let aad = seed_wrap_aad_v1(user_uuid, project_id, credential_kind, auth_binding);
    encrypt_aead_v1(&aead_key, plaintext_seed, &aad)
}

pub fn decrypt_seed_v1(
    root_key: &[u8],
    encrypted_seed: &[u8],
    user_uuid: Uuid,
    project_id: i32,
    credential_kind: CredentialKind,
    auth_binding: &AuthBinding,
) -> Result<Vec<u8>, EncryptError> {
    let aead_key = derive_seed_wrap_aead_key(root_key, auth_binding)?;
    let aad = seed_wrap_aad_v1(user_uuid, project_id, credential_kind, auth_binding);
    decrypt_aead_v1(&aead_key, encrypted_seed, &aad)
}

fn derive_seed_wrap_aead_key(
    root_key: &[u8],
    auth_binding: &AuthBinding,
) -> Result<AeadKey, EncryptError> {
    let seed_wrap_root_key = derive_key(root_key, SEED_WRAP_ROOT_INFO)?;
    derive_key_with_salt(
        &seed_wrap_root_key,
        auth_binding.as_bytes(),
        SEED_WRAP_AEAD_KEY_INFO,
    )
}

fn seed_wrap_aad_v1(
    user_uuid: Uuid,
    project_id: i32,
    credential_kind: CredentialKind,
    auth_binding: &AuthBinding,
) -> Vec<u8> {
    let mut aad = CanonicalBytes::new(SEED_WRAP_DOMAIN);
    aad.append_uuid(user_uuid)
        .append_i32(project_id)
        .append_str(credential_kind.as_str())
        .append_i16(SEED_WRAP_VERSION_V1)
        .append_bytes(auth_binding.as_bytes());
    aad.into_bytes()
}

fn hmac_with_root_domain_key(
    root_key: &[u8],
    key_info: &[u8],
    message: &[u8],
) -> Result<[u8; 32], EncryptError> {
    let mac_key = derive_key(root_key, key_info)?;
    let mut mac =
        HmacSha256::new_from_slice(&mac_key).map_err(|_| EncryptError::KeyDerivationFailed)?;
    mac.update(message);

    let bytes = mac.finalize().into_bytes();
    let mut output = [0u8; 32];
    output.copy_from_slice(&bytes);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_KEY: [u8; 32] = [7u8; 32];
    const USER_UUID: Uuid = Uuid::from_u128(0x2f4f7d9c1cf84c0c8c1a5e9b3e8bde12);
    const ATTACKER_UUID: Uuid = Uuid::from_u128(0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa);
    const PROJECT_ID: i32 = 42;
    const PASSWORD_VERIFIER: &str = "$argon2id$v=19$m=19456,t=2,p=1$fake-salt$fake-password-hash";
    const ATTACKER_PASSWORD_VERIFIER: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$attacker-salt$attacker-password-hash";

    fn password_binding() -> AuthBinding {
        compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "alice@example.com",
            PASSWORD_VERIFIER,
        )
        .unwrap()
    }

    #[test]
    fn password_auth_binding_changes_with_verified_facts() {
        let base = password_binding();

        let changed_verifier = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "alice@example.com",
            "$argon2id$v=19$m=19456,t=2,p=1$other-salt$other-password-hash",
        )
        .unwrap();

        let changed_identifier = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "bob@example.com",
            PASSWORD_VERIFIER,
        )
        .unwrap();

        assert_ne!(base, changed_verifier);
        assert_ne!(base, changed_identifier);
    }

    #[test]
    fn oauth_auth_binding_changes_with_verified_provider_subject() {
        let base = compute_oauth_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "google",
            "google-sub-123",
        )
        .unwrap();

        let changed_subject = compute_oauth_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "google",
            "google-sub-456",
        )
        .unwrap();

        assert_ne!(base, changed_subject);
    }

    #[test]
    fn guest_password_auth_binding_uses_guest_identifier_kind() {
        let email_binding = password_binding();
        let guest_identifier = normalize_guest_login_identifier(USER_UUID);
        let guest_binding = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::GuestUuid,
            &guest_identifier,
            PASSWORD_VERIFIER,
        )
        .unwrap();

        assert_ne!(email_binding, guest_binding);
    }

    #[test]
    fn login_identifier_normalization_is_deterministic() {
        assert_eq!(
            normalize_email_login_identifier("  Alice@Example.COM "),
            "alice@example.com"
        );
        assert_eq!(
            normalize_guest_login_identifier(USER_UUID),
            "2f4f7d9c-1cf8-4c0c-8c1a-5e9b3e8bde12"
        );
    }

    #[test]
    fn credential_lookup_hash_is_not_the_auth_binding() {
        let binding = password_binding();
        let lookup_hash =
            password_credential_lookup_hash(&ROOT_KEY, PROJECT_ID, USER_UUID).unwrap();

        assert_ne!(binding.as_bytes(), lookup_hash.as_bytes());
    }

    #[test]
    fn oauth_credential_lookup_hash_is_scoped_to_provider_subject() {
        let google_lookup = oauth_credential_lookup_hash(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "google",
            "google-sub-123",
        )
        .unwrap();
        let github_lookup = oauth_credential_lookup_hash(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "github",
            "github-user-123",
        )
        .unwrap();

        assert_ne!(google_lookup, github_lookup);
    }

    #[test]
    fn password_reset_code_mac_is_bound_to_user_and_project() {
        let base = password_reset_code_mac(&ROOT_KEY, PROJECT_ID, USER_UUID, "ABC12345").unwrap();
        let other_user = password_reset_code_mac(
            &ROOT_KEY,
            PROJECT_ID,
            Uuid::from_u128(0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb),
            "ABC12345",
        )
        .unwrap();
        let other_project =
            password_reset_code_mac(&ROOT_KEY, PROJECT_ID + 1, USER_UUID, "ABC12345").unwrap();

        assert_ne!(base, other_user);
        assert_ne!(base, other_project);
    }

    #[test]
    fn seed_wrap_round_trips_with_matching_auth_binding() {
        let binding = password_binding();
        let seed = b"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

        let encrypted = encrypt_seed_v1(
            &ROOT_KEY,
            seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding,
        )
        .unwrap();

        let decrypted = decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding,
        )
        .unwrap();

        assert_eq!(seed.to_vec(), decrypted);
    }

    #[test]
    fn seed_wrap_rejects_wrong_auth_binding() {
        let victim_binding = password_binding();
        let attacker_binding = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "attacker@example.com",
            PASSWORD_VERIFIER,
        )
        .unwrap();
        let seed = b"victim mnemonic";

        let encrypted = encrypt_seed_v1(
            &ROOT_KEY,
            seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &victim_binding,
        )
        .unwrap();

        assert!(decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &attacker_binding,
        )
        .is_err());
    }

    #[test]
    fn copied_password_seed_wrap_cannot_unwrap_under_attacker_account_context() {
        let victim_binding = password_binding();
        let attacker_binding = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            ATTACKER_UUID,
            PasswordLoginIdentifierKind::Email,
            "attacker@example.com",
            ATTACKER_PASSWORD_VERIFIER,
        )
        .unwrap();
        let victim_seed = b"victim mnemonic";

        let victim_seed_enc = encrypt_seed_v1(
            &ROOT_KEY,
            victim_seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &victim_binding,
        )
        .unwrap();

        let copied_into_attacker_row = victim_seed_enc;
        let attacker_unwrap = decrypt_seed_v1(
            &ROOT_KEY,
            &copied_into_attacker_row,
            ATTACKER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &attacker_binding,
        );

        assert!(attacker_unwrap.is_err());
    }

    #[test]
    fn swapped_password_verifier_cannot_unwrap_victim_seed() {
        let victim_binding = password_binding();
        let binding_after_attacker_password_enc_swap = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "alice@example.com",
            ATTACKER_PASSWORD_VERIFIER,
        )
        .unwrap();
        let victim_seed = b"victim mnemonic";

        let victim_seed_enc = encrypt_seed_v1(
            &ROOT_KEY,
            victim_seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &victim_binding,
        )
        .unwrap();

        let victim_shell_unwrap = decrypt_seed_v1(
            &ROOT_KEY,
            &victim_seed_enc,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding_after_attacker_password_enc_swap,
        );

        assert!(victim_shell_unwrap.is_err());
    }

    #[test]
    fn password_change_rewrap_invalidates_old_password_binding() {
        let old_binding = password_binding();
        let new_password_verifier = "$argon2id$v=19$m=19456,t=2,p=1$new-salt$new-password-hash";
        let new_binding = compute_password_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            PasswordLoginIdentifierKind::Email,
            "alice@example.com",
            new_password_verifier,
        )
        .unwrap();
        let seed = b"same user seed after password change";

        let rewrapped_seed = encrypt_seed_v1(
            &ROOT_KEY,
            seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &new_binding,
        )
        .unwrap();

        assert!(decrypt_seed_v1(
            &ROOT_KEY,
            &rewrapped_seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &old_binding,
        )
        .is_err());

        let decrypted = decrypt_seed_v1(
            &ROOT_KEY,
            &rewrapped_seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &new_binding,
        )
        .unwrap();
        assert_eq!(seed.to_vec(), decrypted);
    }

    #[test]
    fn copied_oauth_seed_wrap_cannot_unwrap_under_attacker_provider_subject() {
        let victim_binding = compute_oauth_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "google",
            "victim-google-sub",
        )
        .unwrap();
        let attacker_binding = compute_oauth_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            ATTACKER_UUID,
            "google",
            "attacker-google-sub",
        )
        .unwrap();
        let victim_seed = b"victim oauth mnemonic";

        let victim_seed_enc = encrypt_seed_v1(
            &ROOT_KEY,
            victim_seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::OAuth,
            &victim_binding,
        )
        .unwrap();

        let copied_into_attacker_row = victim_seed_enc;
        let attacker_unwrap = decrypt_seed_v1(
            &ROOT_KEY,
            &copied_into_attacker_row,
            ATTACKER_UUID,
            PROJECT_ID,
            CredentialKind::OAuth,
            &attacker_binding,
        );

        assert!(attacker_unwrap.is_err());
    }

    #[test]
    fn seed_wrap_rejects_wrong_account_context() {
        let binding = password_binding();
        let seed = b"victim mnemonic";
        let encrypted = encrypt_seed_v1(
            &ROOT_KEY,
            seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding,
        )
        .unwrap();

        assert!(decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            ATTACKER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding,
        )
        .is_err());

        assert!(decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            USER_UUID,
            PROJECT_ID + 1,
            CredentialKind::Password,
            &binding,
        )
        .is_err());
    }

    #[test]
    fn oauth_seed_wrap_uses_oauth_credential_domain() {
        let binding = compute_oauth_auth_binding(
            &ROOT_KEY,
            PROJECT_ID,
            USER_UUID,
            "google",
            "google-sub-123",
        )
        .unwrap();
        let seed = b"oauth mnemonic";
        let encrypted = encrypt_seed_v1(
            &ROOT_KEY,
            seed,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::OAuth,
            &binding,
        )
        .unwrap();

        assert!(decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::Password,
            &binding,
        )
        .is_err());

        let decrypted = decrypt_seed_v1(
            &ROOT_KEY,
            &encrypted,
            USER_UUID,
            PROJECT_ID,
            CredentialKind::OAuth,
            &binding,
        )
        .unwrap();

        assert_eq!(seed.to_vec(), decrypted);
    }
}
