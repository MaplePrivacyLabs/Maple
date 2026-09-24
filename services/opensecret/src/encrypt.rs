use aes_gcm::{
    aead::{Aead as GcmAead, KeyInit as GcmKeyInit, Payload},
    Aes256Gcm, Nonce as GcmNonce,
};
use aes_siv::{Aes256SivAead, Nonce as SivNonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use generic_array::typenum;
use generic_array::GenericArray;
use hkdf::Hkdf;
use rand_core::RngCore;
use secp256k1::rand::rngs::OsRng;
use secp256k1::SecretKey;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256, Sha512};
use std::{process::Command, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::aws_credentials::AwsCredentialManager;

const AES_256_KEY_LENGTH: usize = 32;
const KMSTOOL_PLAINTEXT_PREFIX: &str = "PLAINTEXT: ";
const KMSTOOL_CIPHERTEXT_PREFIX: &str = "CIPHERTEXT: ";

#[derive(Debug, thiserror::Error)]
pub enum EncryptError {
    #[error("Failed to decrypt")]
    FailedToDecrypt,
    #[error("Bad data")]
    BadData,
    #[error("KMS encryption failed: {0}")]
    KmsError(String),
    #[error("No content to decrypt")]
    NoContent,
    #[error("Deserialization failed: {0}")]
    DeserializationFailed(String),
    #[error("Key derivation failed")]
    KeyDerivationFailed,
}

pub async fn encrypt_with_key(encryption_key: &SecretKey, bytes: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(&encryption_key.secret_bytes()).expect("should convert");

    // Generate a random 96-bit nonce
    let nonce: [u8; 12] = generate_random::<12>();

    let nonce = GcmNonce::from_slice(&nonce);

    let ciphertext = cipher.encrypt(nonce, bytes).expect("should encrypt");

    // Combine nonce and ciphertext
    let mut encrypted = nonce.to_vec();
    encrypted.extend(ciphertext);
    encrypted
}

pub fn decrypt_with_key(encryption_key: &SecretKey, bytes: &[u8]) -> Result<Vec<u8>, EncryptError> {
    if bytes.len() < 12 {
        tracing::error!(
            "Decrypt failed: Input too short (length {}), minimum 12 bytes required",
            bytes.len()
        );
        return Err(EncryptError::BadData);
    }

    // The first 12 bytes are the nonce
    let nonce = GcmNonce::from_slice(&bytes[..12]);

    // The rest is the ciphertext
    let ciphertext = &bytes[12..];

    let cipher = Aes256Gcm::new_from_slice(&encryption_key.secret_bytes()).map_err(|e| {
        tracing::error!("Failed to create cipher from key: {e}");
        EncryptError::FailedToDecrypt
    })?;

    let result = cipher.decrypt(nonce, ciphertext).map_err(|e| {
        // Log detailed error without revealing any sensitive data
        tracing::error!(
            "AES-GCM decryption failed: {:?}. Input length: {}, nonce length: 12, ciphertext length: {}",
            e, bytes.len(), ciphertext.len()
        );
        EncryptError::FailedToDecrypt
    })?;
    Ok(result)
}

pub type AeadKey = [u8; 32];

pub fn derive_key(root_key: &[u8], info: &[u8]) -> Result<AeadKey, EncryptError> {
    derive_key_with_optional_salt(root_key, None, info)
}

pub fn derive_key_with_salt(
    input_key: &[u8],
    salt: &[u8],
    info: &[u8],
) -> Result<AeadKey, EncryptError> {
    derive_key_with_optional_salt(input_key, Some(salt), info)
}

fn derive_key_with_optional_salt(
    input_key: &[u8],
    salt: Option<&[u8]>,
    info: &[u8],
) -> Result<AeadKey, EncryptError> {
    let hkdf = Hkdf::<Sha256>::new(salt, input_key);
    let mut output = [0u8; 32];
    hkdf.expand(info, &mut output)
        .map_err(|_| EncryptError::KeyDerivationFailed)?;
    Ok(output)
}

pub fn encrypt_aead_v1(
    encryption_key: &AeadKey,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptError> {
    let cipher =
        Aes256Gcm::new_from_slice(encryption_key).map_err(|_| EncryptError::FailedToDecrypt)?;
    let nonce: [u8; 12] = generate_random::<12>();
    let nonce = GcmNonce::from_slice(&nonce);

    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| EncryptError::FailedToDecrypt)?;

    let mut encrypted = nonce.to_vec();
    encrypted.extend(ciphertext);
    Ok(encrypted)
}

pub fn decrypt_aead_v1(
    encryption_key: &AeadKey,
    bytes: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, EncryptError> {
    if bytes.len() < 12 {
        return Err(EncryptError::BadData);
    }

    let nonce = GcmNonce::from_slice(&bytes[..12]);
    let ciphertext = &bytes[12..];
    let cipher =
        Aes256Gcm::new_from_slice(encryption_key).map_err(|_| EncryptError::FailedToDecrypt)?;

    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| EncryptError::FailedToDecrypt)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonicalBytes {
    bytes: Vec<u8>,
}

impl CanonicalBytes {
    pub fn new(domain: &str) -> Self {
        let mut canonical = Self::default();
        canonical.append_str(domain);
        canonical
    }

    pub fn append_str(&mut self, value: &str) -> &mut Self {
        self.append_field(b's', value.as_bytes())
    }

    pub fn append_bytes(&mut self, value: &[u8]) -> &mut Self {
        self.append_field(b'b', value)
    }

    pub fn append_i16(&mut self, value: i16) -> &mut Self {
        self.append_field(b'i', &value.to_be_bytes())
    }

    pub fn append_i32(&mut self, value: i32) -> &mut Self {
        self.append_field(b'I', &value.to_be_bytes())
    }

    pub fn append_uuid(&mut self, value: Uuid) -> &mut Self {
        self.append_field(b'u', value.as_bytes())
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn append_field(&mut self, tag: u8, value: &[u8]) -> &mut Self {
        let len: u32 = value
            .len()
            .try_into()
            .expect("canonical field length should fit in u32");
        self.bytes.push(tag);
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(value);
        self
    }
}

// ============================================================================
// High-Level Decryption Helpers
//
// The functions above (encrypt_with_key, decrypt_with_key) are LOW-LEVEL
// primitives that work with raw bytes.
//
// The functions below are HIGH-LEVEL helpers that add:
// - Automatic JSON deserialization
// - Type safety with generics
// - Proper error handling (never silent failures!)
//
// Use these helpers when decrypting structured data (JSON) from the database.
// Use the low-level functions when working with raw bytes or custom formats.
// ============================================================================

/// Decrypt and deserialize encrypted content with type safety
///
/// This is a high-level helper that decrypts content and automatically
/// deserializes it into the target type T using serde_json.
///
/// **Important**: If `encrypted` is `None`, this returns `Ok(None)` (not an error).
/// However, if decryption or deserialization FAILS, this returns `Err` (never silently fails!).
///
/// # Arguments
/// * `key` - The encryption key to use for decryption
/// * `encrypted` - Optional encrypted content bytes
///
/// # Returns
/// - `Ok(Some(T))` if data exists and decryption succeeds
/// - `Ok(None)` if `encrypted` is `None` (no data present)
/// - `Err(EncryptError)` if decryption or deserialization fails
///
/// # Example
/// ```ignore
/// let content: Option<MessageContent> = decrypt_content(&user_key, msg.content_enc.as_ref())
///     .map_err(|e| {
///         error!("Failed to decrypt message: {:?}", e);
///         ApiError::InternalServerError
///     })?;
/// ```
pub fn decrypt_content<T>(
    key: &SecretKey,
    encrypted: Option<&Vec<u8>>,
) -> Result<Option<T>, EncryptError>
where
    T: DeserializeOwned,
{
    let Some(encrypted) = encrypted else {
        return Ok(None);
    };

    let decrypted_bytes = decrypt_with_key(key, encrypted)?;

    let value = serde_json::from_slice(&decrypted_bytes)
        .map_err(|e| EncryptError::DeserializationFailed(e.to_string()))?;

    Ok(Some(value))
}

/// Decrypt encrypted content as a plain UTF-8 string
///
/// This is a high-level helper for decrypting plain text content (not JSON).
///
/// **Important**: If `encrypted` is `None`, this returns `Ok(None)` (not an error).
/// However, if decryption FAILS, this returns `Err` (never silently fails!).
///
/// # Arguments
/// * `key` - The encryption key to use for decryption
/// * `encrypted` - Optional encrypted content bytes
///
/// # Returns
/// - `Ok(Some(String))` if data exists and decryption succeeds
/// - `Ok(None)` if `encrypted` is `None` (no data present)
/// - `Err(EncryptError)` if decryption fails
///
/// # Example
/// ```ignore
/// let text: Option<String> = decrypt_string(&user_key, msg.content_enc.as_ref())
///     .map_err(|e| {
///         error!("Failed to decrypt message: {:?}", e);
///         ApiError::InternalServerError
///     })?;
/// ```
pub fn decrypt_string(
    key: &SecretKey,
    encrypted: Option<&Vec<u8>>,
) -> Result<Option<String>, EncryptError> {
    let Some(encrypted) = encrypted else {
        return Ok(None);
    };

    let bytes = decrypt_with_key(key, encrypted)?;
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}

pub fn encrypt_key_deterministic(encryption_key: &SecretKey, key: &[u8]) -> Vec<u8> {
    let key_bytes: [u8; 32] = encryption_key.secret_bytes();
    let extended_key = extend_key(&key_bytes);
    let cipher = Aes256SivAead::new(&extended_key);
    let nonce = SivNonce::default();
    cipher.encrypt(&nonce, key).expect("encryption failure!")
}

pub fn decrypt_key_deterministic(
    encryption_key: &SecretKey,
    encrypted: &[u8],
) -> Result<Vec<u8>, EncryptError> {
    let key_bytes: [u8; 32] = encryption_key.secret_bytes();
    let extended_key = extend_key(&key_bytes);
    let cipher = Aes256SivAead::new(&extended_key);
    let nonce = SivNonce::default();
    cipher
        .decrypt(&nonce, encrypted)
        .map_err(|_| EncryptError::FailedToDecrypt)
}

fn extend_key(key: &[u8; 32]) -> GenericArray<u8, typenum::U64> {
    let mut hasher = Sha512::new();
    hasher.update(key);
    GenericArray::clone_from_slice(&hasher.finalize())
}

pub fn decrypt_with_kms(
    aws_region: &str,
    aws_key_id: &str,
    aws_secret_key: &str,
    aws_session_token: &str,
    ciphertext: &str,
) -> Result<Vec<u8>, EncryptError> {
    tracing::debug!("Attempting KMS decryption");
    let output = Command::new("/bin/kmstool_enclave_cli")
        .arg("decrypt")
        .arg("--region")
        .arg(aws_region)
        .arg("--proxy-port")
        .arg("8000")
        .arg("--aws-access-key-id")
        .arg(aws_key_id)
        .arg("--aws-secret-access-key")
        .arg(aws_secret_key)
        .arg("--aws-session-token")
        .arg(aws_session_token)
        .arg("--ciphertext")
        .arg(ciphertext)
        .output()
        .map_err(|e| {
            tracing::error!(
                "Failed to execute kmstool_enclave_cli for decryption: {}",
                e
            );
            EncryptError::KmsError(e.to_string())
        })?;

    if !output.status.success() {
        return Err(EncryptError::KmsError("kmstool decrypt failed".to_string()));
    }

    let output_str =
        String::from_utf8(output.stdout).map_err(|e| EncryptError::KmsError(e.to_string()))?;

    parse_kmstool_plaintext(&output_str, "decrypt", None)
}

#[derive(Debug)]
pub struct GenKeyResult {
    pub key: Vec<u8>,
    pub encrypted_key: Vec<u8>,
}

fn strip_single_trailing_newline<'a>(
    output: &'a str,
    operation: &str,
) -> Result<&'a str, EncryptError> {
    if output.contains('\r') {
        return Err(EncryptError::KmsError(format!(
            "Unexpected carriage return in kmstool {operation} output"
        )));
    }

    let output = output.strip_suffix('\n').unwrap_or(output);
    if output.ends_with('\n') {
        return Err(EncryptError::KmsError(format!(
            "Unexpected blank line in kmstool {operation} output"
        )));
    }

    Ok(output)
}

fn decode_kmstool_line(
    line: &str,
    expected_prefix: &str,
    field_name: &str,
) -> Result<Vec<u8>, EncryptError> {
    let encoded = line
        .strip_prefix(expected_prefix)
        .ok_or_else(|| EncryptError::KmsError(format!("Failed to parse kmstool {field_name}")))?;

    if encoded.is_empty() {
        return Err(EncryptError::KmsError(format!(
            "Empty kmstool {field_name}"
        )));
    }

    STANDARD
        .decode(encoded)
        .map_err(|e| EncryptError::KmsError(format!("Failed to decode kmstool {field_name}: {e}")))
}

fn parse_kmstool_plaintext(
    output: &str,
    operation: &str,
    expected_length: Option<usize>,
) -> Result<Vec<u8>, EncryptError> {
    let line = strip_single_trailing_newline(output, operation)?;
    if line.contains('\n') {
        return Err(EncryptError::KmsError(format!(
            "Unexpected additional line in kmstool {operation} output"
        )));
    }

    let plaintext = decode_kmstool_line(line, KMSTOOL_PLAINTEXT_PREFIX, "plaintext")?;
    if let Some(expected_length) = expected_length {
        if plaintext.len() != expected_length {
            return Err(EncryptError::KmsError(format!(
                "Invalid kmstool {operation} plaintext length: expected {expected_length} bytes, got {}",
                plaintext.len()
            )));
        }
    }

    Ok(plaintext)
}

fn parse_kmstool_genkey(output: &str) -> Result<GenKeyResult, EncryptError> {
    let output = strip_single_trailing_newline(output, "genkey")?;
    let mut lines = output.split('\n');
    let ciphertext_line = lines
        .next()
        .ok_or_else(|| EncryptError::KmsError("Missing kmstool ciphertext".to_string()))?;
    let plaintext_line = lines
        .next()
        .ok_or_else(|| EncryptError::KmsError("Missing kmstool plaintext".to_string()))?;

    if lines.next().is_some() {
        return Err(EncryptError::KmsError(
            "Unexpected additional line in kmstool genkey output".to_string(),
        ));
    }

    let encrypted_key =
        decode_kmstool_line(ciphertext_line, KMSTOOL_CIPHERTEXT_PREFIX, "encrypted key")?;
    let key = decode_kmstool_line(plaintext_line, KMSTOOL_PLAINTEXT_PREFIX, "plaintext key")?;
    validate_aes_256_key_length(&key)?;

    Ok(GenKeyResult { key, encrypted_key })
}

pub(crate) fn validate_aes_256_key_length(key: &[u8]) -> Result<(), EncryptError> {
    if key.len() != AES_256_KEY_LENGTH {
        return Err(EncryptError::KmsError(format!(
            "Invalid AES-256 key length: expected {AES_256_KEY_LENGTH} bytes, got {}",
            key.len()
        )));
    }

    Ok(())
}

pub fn create_new_encryption_key(
    aws_region: &str,
    aws_key_id: &str,
    aws_secret_key: &str,
    aws_session_token: &str,
    aws_kms_key_id: &str,
) -> Result<GenKeyResult, EncryptError> {
    tracing::info!("Creating new encryption key");
    tracing::debug!("Attempting to run kmstool_enclave_cli");
    let output = Command::new("/bin/kmstool_enclave_cli")
        .arg("genkey")
        .arg("--region")
        .arg(aws_region)
        .arg("--proxy-port")
        .arg("8000")
        .arg("--aws-access-key-id")
        .arg(aws_key_id)
        .arg("--aws-secret-access-key")
        .arg(aws_secret_key)
        .arg("--aws-session-token")
        .arg(aws_session_token)
        .arg("--key-id")
        .arg(aws_kms_key_id)
        .arg("--key-spec")
        .arg("AES-256")
        .output()
        .map_err(|e| {
            tracing::error!("Failed to execute kmstool_enclave_cli: {}", e);
            EncryptError::KmsError(e.to_string())
        })?;

    if !output.status.success() {
        return Err(EncryptError::KmsError("kmstool genkey failed".to_string()));
    }

    let output_str =
        String::from_utf8(output.stdout).map_err(|e| EncryptError::KmsError(e.to_string()))?;
    parse_kmstool_genkey(&output_str)
}

pub fn generate_random<const LENGTH: usize>() -> [u8; LENGTH] {
    let mut buffer = [0u8; LENGTH];
    getrandom::getrandom(&mut buffer).expect("Failed to generate random bytes");
    buffer
}

pub async fn generate_random_enclave<const LENGTH: usize>(
    aws_credential_manager: Arc<tokio::sync::RwLock<Option<AwsCredentialManager>>>,
) -> [u8; LENGTH] {
    let nonce = if let Some(cred_manager) = aws_credential_manager.read().await.as_ref().cloned() {
        let aws_creds = cred_manager
            .get_credentials()
            .await
            .expect("should have creds");

        generate_random_bytes_from_enclave(
            &aws_creds.region,
            &aws_creds.access_key_id,
            &aws_creds.secret_access_key,
            &aws_creds.token,
            LENGTH,
        )
        .await
        .expect("should generate random bytes")
    } else {
        // Use OS random if aws_credential_manager is None
        let mut nonce = [0u8; LENGTH];
        OsRng.fill_bytes(&mut nonce);
        nonce.to_vec()
    };
    nonce.try_into().expect("Length mismatch")
}

pub async fn generate_random_bytes_from_enclave(
    aws_region: &str,
    aws_key_id: &str,
    aws_secret_key: &str,
    aws_session_token: &str,
    length: usize,
) -> Result<Vec<u8>, EncryptError> {
    tracing::debug!("Attempting to run kmstool_enclave_cli for random byte generation");
    // An abandoned caller must not leave an orphaned helper process running.
    let output = tokio::process::Command::new("/bin/kmstool_enclave_cli")
        .kill_on_drop(true)
        .arg("genrandom")
        .arg("--region")
        .arg(aws_region)
        .arg("--proxy-port")
        .arg("8000")
        .arg("--aws-access-key-id")
        .arg(aws_key_id)
        .arg("--aws-secret-access-key")
        .arg(aws_secret_key)
        .arg("--aws-session-token")
        .arg(aws_session_token)
        .arg("--length")
        .arg(length.to_string())
        .output()
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to execute kmstool_enclave_cli for random byte generation: {}",
                e
            );
            EncryptError::KmsError(e.to_string())
        })?;

    if !output.status.success() {
        return Err(EncryptError::KmsError(
            "kmstool genrandom failed".to_string(),
        ));
    }

    let output_str =
        String::from_utf8(output.stdout).map_err(|e| EncryptError::KmsError(e.to_string()))?;

    parse_kmstool_plaintext(&output_str, "genrandom", Some(length))
}

pub struct CustomRng {
    buffer: Mutex<Vec<u8>>,
}

impl CustomRng {
    pub fn new() -> Self {
        CustomRng {
            buffer: Mutex::new(Vec::new()),
        }
    }

    async fn fill_buffer(&self) {
        let bytes: [u8; 1024] = generate_random();
        let mut buffer = self.buffer.lock().await;
        buffer.extend_from_slice(&bytes);
    }

    pub async fn fill_bytes(&self, dest: &mut [u8]) {
        let mut buffer = self.buffer.lock().await;
        while buffer.len() < dest.len() {
            drop(buffer); // Release the lock before filling the buffer
            self.fill_buffer().await;
            buffer = self.buffer.lock().await;
        }

        let n = dest.len();
        dest.copy_from_slice(&buffer[..n]);
        *buffer = buffer[n..].to_vec();
    }

    pub async fn next_u32(&self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes).await;
        u32::from_le_bytes(bytes)
    }

    pub async fn next_u64(&self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes).await;
        u64::from_le_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kmstool_plaintext_output(bytes: &[u8]) -> String {
        format!("{KMSTOOL_PLAINTEXT_PREFIX}{}\n", STANDARD.encode(bytes))
    }

    fn kmstool_genkey_output(encrypted_key: &[u8], plaintext_key: &[u8]) -> String {
        format!(
            "{KMSTOOL_CIPHERTEXT_PREFIX}{}\n{KMSTOOL_PLAINTEXT_PREFIX}{}\n",
            STANDARD.encode(encrypted_key),
            STANDARD.encode(plaintext_key)
        )
    }

    #[tokio::test]
    async fn test_encryption_with_key() {
        let key = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let content = [6u8; 32].to_vec();

        let encrypted = encrypt_with_key(&key, &content).await;

        let decrypted = decrypt_with_key(&key, &encrypted).unwrap();
        assert_eq!(content, decrypted);
    }

    #[tokio::test]
    async fn legacy_aes_gcm_ciphertext_has_no_owner_context_binding() {
        let root_key = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let wrong_root_key = SecretKey::from_slice(&[2u8; 32]).unwrap();
        let victim_seed = b"victim seed mnemonic bytes";

        let encrypted_seed = encrypt_with_key(&root_key, victim_seed).await;

        assert!(
            decrypt_with_key(&wrong_root_key, &encrypted_seed).is_err(),
            "legacy AES-GCM still authenticates the root key"
        );

        let attacker_row_decrypt = decrypt_with_key(&root_key, &encrypted_seed).unwrap();
        assert_eq!(
            victim_seed.to_vec(),
            attacker_row_decrypt,
            "legacy ciphertext has no AAD parameter for user, credential, table, or row ownership"
        );
    }

    #[test]
    fn canonical_bytes_are_length_delimited() {
        let mut first = CanonicalBytes::new("domain");
        first.append_str("ab").append_str("c");

        let mut second = CanonicalBytes::new("domain");
        second.append_str("a").append_str("bc");

        assert_ne!(first.into_bytes(), second.into_bytes());
    }

    #[test]
    fn aead_v1_authenticates_aad() {
        let key = derive_key(&[1u8; 32], b"test.aead-v1").unwrap();
        let content = b"plaintext";
        let encrypted = encrypt_aead_v1(&key, content, b"aad:victim").unwrap();

        let decrypted = decrypt_aead_v1(&key, &encrypted, b"aad:victim").unwrap();
        assert_eq!(content.to_vec(), decrypted);

        assert!(decrypt_aead_v1(&key, &encrypted, b"aad:attacker").is_err());
    }

    #[test]
    fn test_deterministic_encryption() {
        let key = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let content = b"test_key";

        let encrypted = encrypt_key_deterministic(&key, content);
        let decrypted = decrypt_key_deterministic(&key, &encrypted).unwrap();
        assert_eq!(content.to_vec(), decrypted);
    }

    #[test]
    fn kmstool_decrypt_stdout_contract_is_exact() {
        let plaintext = b"existing KMS plaintext";
        let output = kmstool_plaintext_output(plaintext);

        assert_eq!(
            parse_kmstool_plaintext(&output, "decrypt", None).unwrap(),
            plaintext
        );

        let without_final_newline = output.strip_suffix('\n').unwrap();
        assert_eq!(
            parse_kmstool_plaintext(without_final_newline, "decrypt", None).unwrap(),
            plaintext
        );

        for malformed in [
            format!("LOG: message\n{output}"),
            format!("{output}EXTRA: line\n"),
            format!("{output}\n"),
            output.replace(KMSTOOL_PLAINTEXT_PREFIX, "PLAINTEXT:"),
            output.replace(KMSTOOL_PLAINTEXT_PREFIX, "plaintext: "),
            output.replace('\n', "\r\n"),
        ] {
            assert!(
                parse_kmstool_plaintext(&malformed, "decrypt", None).is_err(),
                "accepted malformed decrypt output: {malformed:?}"
            );
        }
    }

    #[test]
    fn kmstool_genrandom_stdout_requires_requested_length() {
        let requested_length = 32;
        let output = kmstool_plaintext_output(&[7u8; 32]);
        assert_eq!(
            parse_kmstool_plaintext(&output, "genrandom", Some(requested_length)).unwrap(),
            vec![7u8; requested_length]
        );

        for wrong_length in [requested_length - 1, requested_length + 1] {
            let malformed = kmstool_plaintext_output(&vec![7u8; wrong_length]);
            assert!(
                parse_kmstool_plaintext(&malformed, "genrandom", Some(requested_length)).is_err(),
                "accepted {wrong_length} bytes for a {requested_length}-byte request"
            );
        }
    }

    #[test]
    fn kmstool_genkey_stdout_contract_preserves_aes_256_key() {
        let encrypted_key = vec![9u8; 48];
        let plaintext_key = vec![7u8; AES_256_KEY_LENGTH];
        let output = kmstool_genkey_output(&encrypted_key, &plaintext_key);

        let parsed = parse_kmstool_genkey(&output).unwrap();
        assert_eq!(parsed.encrypted_key, encrypted_key);
        assert_eq!(parsed.key, plaintext_key);
    }

    #[test]
    fn kmstool_genkey_stdout_rejects_schema_drift_without_panicking() {
        let encrypted_key_b64 = STANDARD.encode([9u8; 48]);
        let plaintext_key_b64 = STANDARD.encode([7u8; AES_256_KEY_LENGTH]);
        let valid = format!(
            "{KMSTOOL_CIPHERTEXT_PREFIX}{encrypted_key_b64}\n{KMSTOOL_PLAINTEXT_PREFIX}{plaintext_key_b64}\n"
        );

        for malformed in [
            String::new(),
            format!("{KMSTOOL_CIPHERTEXT_PREFIX}{encrypted_key_b64}\n"),
            format!(
                "{KMSTOOL_PLAINTEXT_PREFIX}{plaintext_key_b64}\n{KMSTOOL_CIPHERTEXT_PREFIX}{encrypted_key_b64}\n"
            ),
            valid.replace(KMSTOOL_CIPHERTEXT_PREFIX, "ENCRYPTED: "),
            format!("{valid}EXTRA: line\n"),
            format!("{valid}\n"),
            valid.replace('\n', "\r\n"),
        ] {
            assert!(
                parse_kmstool_genkey(&malformed).is_err(),
                "accepted malformed genkey output: {malformed:?}"
            );
        }
    }

    #[test]
    fn kmstool_genkey_stdout_rejects_non_aes_256_key_lengths() {
        let encrypted_key = [9u8; 48];

        for wrong_length in [AES_256_KEY_LENGTH - 1, AES_256_KEY_LENGTH + 1] {
            let output = kmstool_genkey_output(&encrypted_key, &vec![7u8; wrong_length]);
            assert!(
                parse_kmstool_genkey(&output).is_err(),
                "accepted {wrong_length}-byte plaintext as an AES-256 key"
            );
        }
    }
}
