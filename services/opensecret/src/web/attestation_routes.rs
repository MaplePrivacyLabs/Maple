use crate::{encrypt::generate_random, ApiError, AppMode, AppState};
use aws_nitro_enclaves_nsm_api::{
    api::{Request, Response},
    driver::{nsm_exit, nsm_init, nsm_process_request},
};
use axum::routing::post;
use axum::{extract::State, routing::get, Json};
use axum::{http::StatusCode, Router};
use base64::{engine::general_purpose, Engine as _};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, Key, KeyInit, Nonce};
use chrono::{Duration, Utc};
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use serde_cbor::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use tokio::{sync::Semaphore, task};
use tracing::error;
use uuid::Uuid;
use yasna::models::ObjectIdentifier;
use yasna::{construct_der, Tag};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionState {
    session_key: [u8; 32],
}

impl SessionState {
    pub fn new(session_key: [u8; 32]) -> Self {
        Self { session_key }
    }

    pub fn get_session_key(&self) -> &[u8; 32] {
        &self.session_key
    }

    pub fn decrypt(&self, encrypted_data: &[u8], nonce: &[u8; 12]) -> Result<Vec<u8>, ApiError> {
        let key = Key::from_slice(self.session_key.as_ref());
        let cipher = ChaCha20Poly1305::new(key);
        let nonce = Nonce::from_slice(nonce);

        cipher.decrypt(nonce, encrypted_data).map_err(|e| {
            tracing::error!("could not decrypt data: {e}");
            ApiError::InternalServerError
        })
    }
}

#[derive(Deserialize)]
struct KeyExchangeRequest {
    nonce: String,
    client_public_key: String,
}

#[derive(Serialize)]
struct KeyExchangeResponse {
    session_id: Uuid,
    encrypted_session_key: String,
}

#[derive(Serialize)]
struct AttestationResponse {
    attestation_document: String,
}

pub fn router(app_state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route("/attestation/:nonce", get(get_attestation))
        .route("/key_exchange", post(key_exchange))
        .with_state(app_state)
}

async fn get_attestation(
    State(data): State<Arc<AppState>>,
    axum::extract::Path(nonce): axum::extract::Path<String>,
) -> Result<(StatusCode, Json<AttestationResponse>), ApiError> {
    // Create an ephemeral key pair for this request
    let enclave_public_key = data.create_ephemeral_key(&nonce).await?;

    // Create a request for the attestation document
    let request = Request::Attestation {
        user_data: None,
        public_key: Some(ByteBuf::from(enclave_public_key.as_bytes().to_vec())),
        nonce: Some(ByteBuf::from(nonce.into_bytes())),
    };

    let document = generate_attestation_document(data, request).await?;
    Ok(attestation_response(document))
}

/// Upper bound on NSM attestation requests in flight at once. The kernel NSM
/// driver serializes device requests under one mutex, so additional
/// concurrency only queues inside the kernel; this bound keeps a burst of
/// unauthenticated handshakes from occupying blocking-pool threads while
/// they wait for that lock.
const MAX_CONCURRENT_NSM_REQUESTS: usize = 4;
static NSM_REQUEST_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_NSM_REQUESTS)));

pub(crate) async fn generate_attestation_document(
    data: Arc<AppState>,
    request: Request,
) -> Result<Vec<u8>, ApiError> {
    match data.app_mode {
        AppMode::Local => generate_mock_attestation_document(data, request).await,
        _ => run_nsm_blocking(move || generate_real_attestation_document(request)).await?,
    }
}

/// Runs one synchronous NSM operation on Tokio's blocking pool under the
/// process-wide permit bound, so the device ioctl never stalls an async
/// worker thread that is serving other connections.
async fn run_nsm_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    run_blocking_with_permit(Arc::clone(&NSM_REQUEST_PERMITS), operation).await
}

/// The permit is owned by the blocking closure, not by this future. A
/// cancelled caller (for example a client that disconnects while waiting)
/// therefore cannot release capacity while its NSM operation is still
/// running on the blocking pool.
async fn run_blocking_with_permit<T, F>(
    permits: Arc<Semaphore>,
    operation: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = permits.acquire_owned().await.map_err(|_| {
        error!("NSM request permit semaphore is closed");
        ApiError::InternalServerError
    })?;
    task::spawn_blocking(move || {
        let result = operation();
        drop(permit);
        result
    })
    .await
    .map_err(|join_error| {
        error!("NSM blocking task did not complete: {join_error}");
        ApiError::InternalServerError
    })
}

fn attestation_response(document: Vec<u8>) -> (StatusCode, Json<AttestationResponse>) {
    let attestation_document = general_purpose::STANDARD.encode(document);
    (
        StatusCode::OK,
        Json(AttestationResponse {
            attestation_document,
        }),
    )
}

async fn generate_mock_attestation_document(
    data: Arc<AppState>,
    request: Request,
) -> Result<Vec<u8>, ApiError> {
    let (user_data, nonce, public_key) = match request {
        Request::Attestation {
            user_data,
            nonce,
            public_key,
        } => (user_data, nonce, public_key),
        _ => unreachable!(),
    };

    // Create a mock attestation document
    let mock_document =
        create_mock_attestation_document(data.clone(), user_data, nonce, public_key).await;

    // Encode the mock document
    let encoded_document = serde_cbor::to_vec(&mock_document).map_err(|e| {
        error!("Failed to encode mock document: {}", e);
        ApiError::InternalServerError
    })?;

    // Sign the mock document
    let (signature, _) = sign_mock_document(&encoded_document).map_err(|e| {
        error!("Failed to sign mock document: {}", e);
        ApiError::InternalServerError
    })?;

    // Create the COSE_Sign1 structure
    let cose_sign1 = create_cose_sign1(encoded_document, signature);

    // Encode the COSE_Sign1 structure
    let final_document = serde_cbor::to_vec(&cose_sign1).map_err(|e| {
        error!("Failed to encode COSE_Sign1 structure: {}", e);
        ApiError::InternalServerError
    })?;

    Ok(final_document)
}

async fn create_mock_attestation_document(
    data: Arc<AppState>,
    user_data: Option<ByteBuf>,
    nonce: Option<ByteBuf>,
    public_key: Option<ByteBuf>,
) -> Value {
    let mut pcrs = BTreeMap::new();
    for i in 0..3 {
        let random_bytes = generate_random::<48>();
        pcrs.insert(
            Value::Integer(i.into()),
            Value::Bytes(random_bytes.to_vec()),
        );
    }

    let module_id = format!("i-{}", hex::encode(generate_random::<8>()));

    // Create a mock certificate
    let mock_cert = create_mock_certificate(data.clone()).await;
    let cabundle = vec![
        create_mock_certificate(data.clone()).await,
        create_mock_certificate(data.clone()).await,
    ];

    let mut document = BTreeMap::new();
    document.insert(Value::Text("module_id".into()), Value::Text(module_id));
    document.insert(Value::Text("digest".into()), Value::Text("SHA384".into()));
    document.insert(
        Value::Text("timestamp".into()),
        Value::Integer(chrono::Utc::now().timestamp().into()),
    );
    document.insert(Value::Text("pcrs".into()), Value::Map(pcrs));
    document.insert(Value::Text("certificate".into()), Value::Bytes(mock_cert));
    document.insert(
        Value::Text("cabundle".into()),
        Value::Array(cabundle.into_iter().map(Value::Bytes).collect()),
    );

    // Always include public_key, even if None
    document.insert(
        Value::Text("public_key".into()),
        public_key.map_or(Value::Null, |p| Value::Bytes(p.to_vec())),
    );

    // Always include user_data, even if None
    document.insert(
        Value::Text("user_data".into()),
        user_data.map_or(Value::Null, |u| Value::Bytes(u.into_vec())),
    );

    // Always include nonce, even if None
    document.insert(
        Value::Text("nonce".into()),
        nonce.map_or(Value::Null, |n| Value::Bytes(n.into_vec())),
    );
    Value::Map(document)
}

async fn create_mock_certificate(_data: Arc<AppState>) -> Vec<u8> {
    let random_8_bytes = generate_random::<8>();
    let random_32_bytes = generate_random::<32>();

    let result = construct_der(|writer| {
        writer.write_sequence(|writer| {
            // TBSCertificate
            writer.next().write_sequence(|writer| {
                // Version
                writer.next().write_tagged(Tag::context(0), |writer| {
                    writer.write_i32(2) // v3
                });
                // SerialNumber
                writer.next().write_u64(u64::from_be_bytes(random_8_bytes));
                // Signature Algorithm
                writer.next().write_sequence(|writer| {
                    writer
                        .next()
                        .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 10045, 4, 3, 2]));
                    // ecdsa-with-SHA256
                });
                // Issuer
                writer.next().write_sequence(|writer| {
                    writer.next().write_set(|writer| {
                        writer.next().write_sequence(|writer| {
                            writer
                                .next()
                                .write_oid(&ObjectIdentifier::from_slice(&[2, 5, 4, 3])); // commonName
                            writer.next().write_utf8_string("Mock CA");
                        });
                    });
                });
                // Validity
                writer.next().write_sequence(|writer| {
                    let now = Utc::now();
                    let not_after = now + Duration::days(365);

                    // Write dates as bytes
                    writer
                        .next()
                        .write_bytes(&now.format("%y%m%d%H%M%SZ").to_string().into_bytes());
                    writer
                        .next()
                        .write_bytes(&not_after.format("%y%m%d%H%M%SZ").to_string().into_bytes());
                });
                // Subject
                writer.next().write_sequence(|writer| {
                    writer.next().write_set(|writer| {
                        writer.next().write_sequence(|writer| {
                            writer
                                .next()
                                .write_oid(&ObjectIdentifier::from_slice(&[2, 5, 4, 3])); // commonName
                            writer.next().write_utf8_string("Mock Enclave");
                        });
                    });
                });
                // SubjectPublicKeyInfo
                writer.next().write_sequence(|writer| {
                    writer.next().write_sequence(|writer| {
                        writer
                            .next()
                            .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 10045, 2, 1])); // ecPublicKey
                        writer
                            .next()
                            .write_oid(&ObjectIdentifier::from_slice(&[1, 3, 132, 0, 34]));
                        // secp384r1
                    });
                    writer
                        .next()
                        .write_bitvec_bytes(&random_32_bytes, random_32_bytes.len() * 8);
                });
            });
            // SignatureAlgorithm
            writer.next().write_sequence(|writer| {
                writer
                    .next()
                    .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 10045, 4, 3, 2]));
                // ecdsa-with-SHA256
            });
            // SignatureValue
            writer
                .next()
                .write_bitvec_bytes(&random_32_bytes, random_32_bytes.len() * 8);
        })
    });
    result
}

fn sign_mock_document(document: &[u8]) -> Result<(Vec<u8>, PublicKey), String> {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[0x42; 32])
        .map_err(|e| format!("Failed to create secret key: {}", e))?;
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);

    let mut hasher = Sha256::new();
    hasher.update(document);
    let message_hash = hasher.finalize();

    let message = secp256k1::Message::from_digest_slice(&message_hash)
        .map_err(|e| format!("Failed to create message from digest: {}", e))?;

    let signature = secp.sign_ecdsa(&message, &secret_key);

    Ok((signature.serialize_compact().to_vec(), public_key))
}

fn create_cose_sign1(payload: Vec<u8>, signature: Vec<u8>) -> Value {
    Value::Array(vec![
        Value::Bytes(vec![]),        // Protected header (empty)
        Value::Map(BTreeMap::new()), // Unprotected header (empty)
        Value::Bytes(payload),
        Value::Bytes(signature),
    ])
}

fn generate_real_attestation_document(request: Request) -> Result<Vec<u8>, ApiError> {
    // Initialize the Nitro Secure Module (NSM) driver
    let nsm_fd = nsm_init();
    if nsm_fd < 0 {
        return Err(ApiError::InternalServerError);
    }

    // Process the request and get the response
    let response = nsm_process_request(nsm_fd, request);

    // Close the NSM file descriptor
    nsm_exit(nsm_fd);

    // Handle the response
    match response {
        Response::Attestation { document } => Ok(document),
        Response::Error(_) => {
            error!("NSM returned an error response");
            Err(ApiError::InternalServerError)
        }
        _ => {
            error!("Unexpected response from NSM");
            Err(ApiError::InternalServerError)
        }
    }
}

async fn key_exchange(
    State(data): State<Arc<AppState>>,
    Json(payload): Json<KeyExchangeRequest>,
) -> Result<Json<KeyExchangeResponse>, ApiError> {
    let client_public_key_bytes = general_purpose::STANDARD
        .decode(&payload.client_public_key)
        .map_err(|_| ApiError::BadRequest)?;

    let client_public_key = x25519_dalek::PublicKey::from(
        <[u8; 32]>::try_from(client_public_key_bytes.as_slice())
            .map_err(|_| ApiError::BadRequest)?,
    );

    let ephemeral_secret = data
        .get_and_remove_ephemeral_secret(&payload.nonce)
        .await?
        .ok_or(ApiError::BadRequest)?;

    let shared_secret = derive_contributory_shared_secret(ephemeral_secret, &client_public_key)?;

    // Generate a random session key using your secure random function
    let session_state = SessionState::new(crate::encrypt::generate_random());

    // Encrypt the session key using the shared secret
    let nonce_bytes: [u8; 12] = crate::encrypt::generate_random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let cipher = ChaCha20Poly1305::new(shared_secret.as_bytes().into());
    let mut encrypted_session_key = nonce_bytes.to_vec();
    encrypted_session_key.extend_from_slice(
        &cipher
            .encrypt(nonce, session_state.get_session_key().as_ref())
            .map_err(|_| ApiError::InternalServerError)?,
    );

    // Generate a new UUID for the session
    let session_id = Uuid::new_v4();

    // Store the session state, evicting the least-recently-used unleased
    // session if the cache is full.
    data.store_session_state(session_id, session_state).await?;
    Ok(Json(KeyExchangeResponse {
        session_id,
        encrypted_session_key: general_purpose::STANDARD.encode(&encrypted_session_key),
    }))
}

fn derive_contributory_shared_secret(
    ephemeral_secret: x25519_dalek::EphemeralSecret,
    client_public_key: &x25519_dalek::PublicKey,
) -> Result<x25519_dalek::SharedSecret, ApiError> {
    let shared_secret = ephemeral_secret.diffie_hellman(client_public_key);
    if !shared_secret.was_contributory() {
        return Err(ApiError::BadRequest);
    }

    Ok(shared_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_zeroizes_key_material() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}

        assert_zeroize_on_drop::<SessionState>();

        let mut state = SessionState::new([0xA5; 32]);
        state.zeroize();
        assert_eq!(state.get_session_key(), &[0; 32]);
    }

    #[test]
    fn key_exchange_rejects_non_contributory_x25519_public_key() {
        let ephemeral_secret = x25519_dalek::EphemeralSecret::random_from_rng(rand_core::OsRng);
        let low_order_public_key = x25519_dalek::PublicKey::from([0u8; 32]);

        assert!(matches!(
            derive_contributory_shared_secret(ephemeral_secret, &low_order_public_key),
            Err(ApiError::BadRequest)
        ));
    }

    #[test]
    fn key_exchange_accepts_honest_x25519_public_key() {
        let ephemeral_secret = x25519_dalek::EphemeralSecret::random_from_rng(rand_core::OsRng);
        let client_secret = x25519_dalek::EphemeralSecret::random_from_rng(rand_core::OsRng);
        let client_public_key = x25519_dalek::PublicKey::from(&client_secret);

        assert!(derive_contributory_shared_secret(ephemeral_secret, &client_public_key).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nsm_blocking_operations_respect_the_permit_bound_and_all_complete() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        const BOUND: usize = 2;
        const OPERATIONS: usize = BOUND * 6;
        let permits = Arc::new(Semaphore::new(BOUND));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for index in 0..OPERATIONS {
            let permits = Arc::clone(&permits);
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                run_blocking_with_permit(permits, move || {
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    index
                })
                .await
            }));
        }

        let mut completed = Vec::new();
        for task in tasks {
            completed.push(task.await.unwrap().unwrap());
        }
        completed.sort_unstable();
        assert_eq!(completed, (0..OPERATIONS).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) >= 1);
        assert!(peak.load(Ordering::SeqCst) <= BOUND);
        assert_eq!(permits.available_permits(), BOUND);
    }

    #[tokio::test]
    async fn nsm_blocking_panic_is_an_internal_error_and_releases_its_permit() {
        let permits = Arc::new(Semaphore::new(1));
        let failed: Result<(), ApiError> =
            run_blocking_with_permit(Arc::clone(&permits), || panic!("simulated NSM failure"))
                .await;
        assert!(matches!(failed, Err(ApiError::InternalServerError)));
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(
            run_blocking_with_permit(Arc::clone(&permits), || 7)
                .await
                .unwrap(),
            7
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_keeps_its_permit_until_the_operation_finishes() {
        use std::sync::mpsc;
        use std::time::Duration;

        let permits = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        // Start one operation and abandon its caller once the blocking
        // operation is known to be running.
        let waiter = tokio::spawn({
            let permits = Arc::clone(&permits);
            async move {
                run_blocking_with_permit(permits, move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .await
            }
        });
        task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        // The abandoned operation still holds the only permit, so a second
        // operation must not start.
        assert_eq!(permits.available_permits(), 0);
        let blocked = tokio::time::timeout(
            Duration::from_millis(200),
            run_blocking_with_permit(Arc::clone(&permits), || 1),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second operation started while the abandoned operation held the permit"
        );

        // Releasing the running operation returns the permit.
        release_tx.send(()).unwrap();
        let value = tokio::time::timeout(
            Duration::from_secs(5),
            run_blocking_with_permit(Arc::clone(&permits), || 2),
        )
        .await
        .expect("permit was not returned after the abandoned operation finished")
        .unwrap();
        assert_eq!(value, 2);
        assert_eq!(permits.available_permits(), 1);
    }
}
