use std::{fmt, str::FromStr};

use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::ZeroizeOnDrop;

use super::{envelope::RequestId, Result, TransportV2Error};

pub(super) const HANDSHAKE_CHALLENGE_BYTES: usize = 32;
pub(super) const X25519_PUBLIC_KEY_BYTES: usize = 32;
pub(super) const SESSION_ID_BYTES: usize = 16;
pub(super) const RECORD_TAG_BYTES: usize = 16;
pub(super) const MIN_REQUEST_RECORD_BYTES: usize = 16 + RECORD_TAG_BYTES;

const RECORD_NONCE_BYTES: usize = 12;
const HANDSHAKE_DOMAIN: &[u8] = b"opensecret/transport-v2/session/v1";
const ATTESTATION_USER_DATA_DOMAIN: &[u8] = b"opensecret/transport-v2/session/v1/client-public-key";
const REQUEST_KEY_INFO: &[u8] = b"opensecret/transport-v2/request-key/v1";
const RESPONSE_KEY_INFO: &[u8] = b"opensecret/transport-v2/response-key/v1";
const SESSION_ID_INFO: &[u8] = b"opensecret/transport-v2/session-id/v1";
const REQUEST_SUBKEY_INFO: &[u8] = b"opensecret/transport-v2/request-subkey/v1";
const RESPONSE_SUBKEY_INFO: &[u8] = b"opensecret/transport-v2/response-subkey/v1";
const REQUEST_RECORD_DOMAIN: &[u8] = b"opensecret/transport-v2/request-record/v1";
const RESPONSE_RECORD_DOMAIN: &[u8] = b"opensecret/transport-v2/response-record/v1";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct SessionId([u8; SESSION_ID_BYTES]);

impl SessionId {
    #[cfg(test)]
    pub(super) const fn from_bytes(bytes: [u8; SESSION_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub(super) const fn as_bytes(&self) -> &[u8; SESSION_ID_BYTES] {
        &self.0
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionId(")?;
        formatter.write_str(&hex::encode(self.0))?;
        formatter.write_str(")")
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl FromStr for SessionId {
    type Err = TransportV2Error;

    fn from_str(value: &str) -> Result<Self> {
        let encoded = value.as_bytes();
        if encoded.len() != SESSION_ID_BYTES * 2
            || !encoded
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(TransportV2Error::InvalidSessionResponse);
        }

        let mut decoded = [0; SESSION_ID_BYTES];
        hex::decode_to_slice(encoded, &mut decoded)
            .map_err(|_| TransportV2Error::InvalidSessionResponse)?;
        Ok(Self(decoded))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HandshakeTranscript {
    challenge: [u8; HANDSHAKE_CHALLENGE_BYTES],
    client_public_key: [u8; X25519_PUBLIC_KEY_BYTES],
    server_public_key: [u8; X25519_PUBLIC_KEY_BYTES],
}

impl HandshakeTranscript {
    pub(super) const fn new(
        challenge: [u8; HANDSHAKE_CHALLENGE_BYTES],
        client_public_key: [u8; X25519_PUBLIC_KEY_BYTES],
        server_public_key: [u8; X25519_PUBLIC_KEY_BYTES],
    ) -> Self {
        Self {
            challenge,
            client_public_key,
            server_public_key,
        }
    }

    pub(super) const fn challenge(&self) -> &[u8; HANDSHAKE_CHALLENGE_BYTES] {
        &self.challenge
    }

    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(HANDSHAKE_DOMAIN);
        hasher.update([0]);
        hasher.update(self.challenge);
        hasher.update(self.client_public_key);
        hasher.update(self.server_public_key);
        hasher.finalize().into()
    }
}

pub(super) fn attestation_user_data(client_public_key: &[u8; X25519_PUBLIC_KEY_BYTES]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ATTESTATION_USER_DATA_DOMAIN.len() + 1 + 32);
    bytes.extend_from_slice(ATTESTATION_USER_DATA_DOMAIN);
    bytes.push(0);
    bytes.extend_from_slice(client_public_key);
    bytes
}

#[derive(ZeroizeOnDrop)]
struct TrafficKey([u8; 32]);

/// Direction-separated secrets derived from one verified attested transcript.
#[derive(ZeroizeOnDrop)]
pub(super) struct SessionSecrets {
    #[zeroize(skip)]
    session_id: SessionId,
    request_key: TrafficKey,
    response_key: TrafficKey,
}

/// Per-request response authority. Deriving the subkey once avoids HKDF work
/// per chunk and lets an admitted response finish without retaining the
/// session object that created it.
#[derive(ZeroizeOnDrop)]
pub(super) struct ResponseOpener {
    #[zeroize(skip)]
    session_id: SessionId,
    #[zeroize(skip)]
    request_id: RequestId,
    key: TrafficKey,
    #[zeroize(skip)]
    next_sequence: u64,
}

impl fmt::Debug for ResponseOpener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseOpener")
            .field("session_id", &self.session_id)
            .field("request_id", &self.request_id)
            .field("next_sequence", &self.next_sequence)
            .finish_non_exhaustive()
    }
}

impl ResponseOpener {
    pub(super) fn open_next(&mut self, record: &[u8]) -> Result<Vec<u8>> {
        let sequence = self.next_sequence;
        let plaintext = open(
            &self.key,
            &response_aad(self.session_id, self.request_id, sequence),
            response_nonce(sequence),
            record,
        )?;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or(TransportV2Error::InvalidSequence)?;
        Ok(plaintext)
    }
}

impl fmt::Debug for SessionSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionSecrets")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionSecrets {
    pub(super) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(super) fn encrypt_request(
        &self,
        request_id: RequestId,
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        let key = derive_record_subkey(
            &self.request_key,
            REQUEST_SUBKEY_INFO,
            self.session_id,
            request_id,
        )?;
        let ciphertext = seal(
            &key,
            &request_aad(self.session_id, request_id),
            [0; RECORD_NONCE_BYTES],
            plaintext,
        )?;
        let mut record = Vec::with_capacity(request_id.as_bytes().len() + ciphertext.len());
        record.extend_from_slice(request_id.as_bytes());
        record.extend_from_slice(&ciphertext);
        Ok(record)
    }

    pub(super) fn response_opener(&self, request_id: RequestId) -> Result<ResponseOpener> {
        Ok(ResponseOpener {
            session_id: self.session_id,
            request_id,
            key: derive_record_subkey(
                &self.response_key,
                RESPONSE_SUBKEY_INFO,
                self.session_id,
                request_id,
            )?,
            next_sequence: 0,
        })
    }
}

pub(super) fn derive_client_session(
    client_secret: EphemeralSecret,
    transcript: &HandshakeTranscript,
) -> Result<SessionSecrets> {
    let server_public_key = PublicKey::from(transcript.server_public_key);
    derive_session_secrets(
        client_secret.diffie_hellman(&server_public_key).as_bytes(),
        transcript,
    )
}

fn derive_session_secrets(
    shared_secret: &[u8; 32],
    transcript: &HandshakeTranscript,
) -> Result<SessionSecrets> {
    if bool::from(shared_secret.ct_eq(&[0; 32])) {
        return Err(TransportV2Error::NonContributoryKey);
    }

    let transcript_digest = transcript.digest();
    let hkdf = Hkdf::<Sha256>::new(Some(transcript.challenge()), shared_secret);
    let mut request_key = [0; 32];
    let mut response_key = [0; 32];
    let mut session_id = [0; SESSION_ID_BYTES];
    hkdf.expand(
        &key_info(REQUEST_KEY_INFO, &transcript_digest),
        &mut request_key,
    )
    .map_err(|_| TransportV2Error::KeyDerivation)?;
    hkdf.expand(
        &key_info(RESPONSE_KEY_INFO, &transcript_digest),
        &mut response_key,
    )
    .map_err(|_| TransportV2Error::KeyDerivation)?;
    hkdf.expand(
        &key_info(SESSION_ID_INFO, &transcript_digest),
        &mut session_id,
    )
    .map_err(|_| TransportV2Error::KeyDerivation)?;

    Ok(SessionSecrets {
        session_id: SessionId(session_id),
        request_key: TrafficKey(request_key),
        response_key: TrafficKey(response_key),
    })
}

fn key_info(label: &[u8], transcript_digest: &[u8; 32]) -> Vec<u8> {
    let mut info = Vec::with_capacity(label.len() + 1 + transcript_digest.len());
    info.extend_from_slice(label);
    info.push(0);
    info.extend_from_slice(transcript_digest);
    info
}

fn derive_record_subkey(
    base_key: &TrafficKey,
    label: &[u8],
    session_id: SessionId,
    request_id: RequestId,
) -> Result<TrafficKey> {
    let hkdf =
        Hkdf::<Sha256>::from_prk(&base_key.0).map_err(|_| TransportV2Error::KeyDerivation)?;
    let mut info = Vec::with_capacity(label.len() + 1 + SESSION_ID_BYTES + 16);
    info.extend_from_slice(label);
    info.push(0);
    info.extend_from_slice(session_id.as_bytes());
    info.extend_from_slice(request_id.as_bytes());

    let mut key = [0; 32];
    hkdf.expand(&info, &mut key)
        .map_err(|_| TransportV2Error::KeyDerivation)?;
    Ok(TrafficKey(key))
}

fn request_aad(session_id: SessionId, request_id: RequestId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(REQUEST_RECORD_DOMAIN.len() + 1 + SESSION_ID_BYTES + 16);
    aad.extend_from_slice(REQUEST_RECORD_DOMAIN);
    aad.push(0);
    aad.extend_from_slice(session_id.as_bytes());
    aad.extend_from_slice(request_id.as_bytes());
    aad
}

fn response_aad(session_id: SessionId, request_id: RequestId, sequence: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        RESPONSE_RECORD_DOMAIN.len() + 1 + SESSION_ID_BYTES + 16 + std::mem::size_of::<u64>(),
    );
    aad.extend_from_slice(RESPONSE_RECORD_DOMAIN);
    aad.push(0);
    aad.extend_from_slice(session_id.as_bytes());
    aad.extend_from_slice(request_id.as_bytes());
    aad.extend_from_slice(&sequence.to_be_bytes());
    aad
}

fn response_nonce(sequence: u64) -> [u8; RECORD_NONCE_BYTES] {
    let mut nonce = [0; RECORD_NONCE_BYTES];
    nonce[4..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

fn seal(
    key: &TrafficKey,
    aad: &[u8],
    nonce: [u8; RECORD_NONCE_BYTES],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new((&key.0).into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| TransportV2Error::Encryption)
}

fn open(
    key: &TrafficKey,
    aad: &[u8],
    nonce: [u8; RECORD_NONCE_BYTES],
    record: &[u8],
) -> Result<Vec<u8>> {
    if record.len() < RECORD_TAG_BYTES {
        return Err(TransportV2Error::InvalidFrame);
    }
    ChaCha20Poly1305::new((&key.0).into())
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: record, aad })
        .map_err(|_| TransportV2Error::Authentication)
}

#[cfg(test)]
pub(super) fn session_from_shared_for_test(
    shared_secret: [u8; 32],
    transcript: HandshakeTranscript,
) -> SessionSecrets {
    derive_session_secrets(&shared_secret, &transcript).expect("valid test session")
}

#[cfg(test)]
pub(super) fn seal_response_for_test(
    secrets: &SessionSecrets,
    request_id: RequestId,
    sequence: u64,
    plaintext: &[u8],
) -> Vec<u8> {
    let key = derive_record_subkey(
        &secrets.response_key,
        RESPONSE_SUBKEY_INFO,
        secrets.session_id,
        request_id,
    )
    .expect("valid response test key");
    seal(
        &key,
        &response_aad(secrets.session_id, request_id, sequence),
        response_nonce(sequence),
        plaintext,
    )
    .expect("valid response test record")
}

#[cfg(test)]
pub(super) fn open_request_for_test(
    secrets: &SessionSecrets,
    record: &[u8],
) -> (RequestId, Vec<u8>) {
    let request_id = RequestId::from_bytes(
        record
            .get(..16)
            .expect("test request record contains an id")
            .try_into()
            .expect("test request id has the fixed width"),
    );
    let key = derive_record_subkey(
        &secrets.request_key,
        REQUEST_SUBKEY_INFO,
        secrets.session_id,
        request_id,
    )
    .expect("valid request test key");
    let plaintext = open(
        &key,
        &request_aad(secrets.session_id, request_id),
        [0; RECORD_NONCE_BYTES],
        record
            .get(16..)
            .expect("test request record contains ciphertext"),
    )
    .expect("valid encrypted test request");
    (request_id, plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript() -> HandshakeTranscript {
        HandshakeTranscript::new([0x11; 32], [0x22; 32], [0x33; 32])
    }

    #[test]
    fn matches_backend_key_and_record_vectors() {
        let secrets = session_from_shared_for_test([0x44; 32], transcript());
        assert_eq!(
            secrets.session_id().to_string(),
            "f7258fb103137c612baab47ced4a5a02"
        );
        let record = secrets
            .encrypt_request(RequestId::from_bytes([0x55; 16]), b"vector plaintext")
            .unwrap();
        assert_eq!(
            hex::encode(record),
            "55555555555555555555555555555555671f5c411205cb00f769e6b2705052b795e91f44516fc6165e16a152e686b209"
        );
        assert_eq!(
            hex::encode(seal_response_for_test(
                &secrets,
                RequestId::from_bytes([0x66; 16]),
                0,
                b"vector response",
            )),
            "25a2d5ed89864bd7b5e13c83eb49b1f314a70abf8bd7e871b706bb6768c9e1"
        );
    }

    #[test]
    fn response_records_are_bound_to_session_request_and_sequence() {
        let first = session_from_shared_for_test([0x44; 32], transcript());
        let second = session_from_shared_for_test([0x45; 32], transcript());
        let request_id = RequestId::from_bytes([1; 16]);
        let record = seal_response_for_test(&first, request_id, 0, b"response");

        assert!(matches!(
            second
                .response_opener(request_id)
                .unwrap()
                .open_next(&record),
            Err(TransportV2Error::Authentication)
        ));
        assert!(matches!(
            first
                .response_opener(RequestId::from_bytes([2; 16]))
                .unwrap()
                .open_next(&record),
            Err(TransportV2Error::Authentication)
        ));
        let mut reordered = first.response_opener(request_id).unwrap();
        let sequence_one = seal_response_for_test(&first, request_id, 1, b"response");
        assert!(matches!(
            reordered.open_next(&sequence_one),
            Err(TransportV2Error::Authentication)
        ));
    }

    #[test]
    fn session_id_wire_encoding_is_canonical() {
        let session_id = SessionId::from_bytes([0xab; 16]);
        assert_eq!(session_id.to_string(), "abababababababababababababababab");
        assert_eq!(
            SessionId::from_str(&session_id.to_string()).unwrap(),
            session_id
        );
        assert!(SessionId::from_str("ABABABABABABABABABABABABABABABAB").is_err());
    }
}
