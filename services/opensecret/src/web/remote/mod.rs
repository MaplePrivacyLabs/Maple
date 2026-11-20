//! Same-account discovery and admission. The service never sees a pairing code
//! or PAKE message and never creates a device's local control grant.
pub(crate) mod relay;
pub(crate) mod types;
use types::*;

use axum::{
    extract::State,
    middleware::from_fn_with_state,
    response::Response,
    routing::{get, post},
    Extension, Router,
};
use chrono::Utc;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, sync::Arc};
use uuid::Uuid;

use crate::{
    db::DBError,
    models::{remote_enrollment::Record, users::User},
    remote_enrollment::Binding,
    web::encryption_middleware::{decrypt_request, encrypt_response, Decrypted, TransportSession},
    ApiError, AppState,
};

const DEVICE_LEASE: i64 = 900;
const ADMISSION_LEASE: i64 = 120;
// Keep recent tombstones across short interruptions. Local device windows still
// enforce their original deadline and one attempt after service cleanup.
const WINDOW_RETENTION: i64 = 24 * 60 * 60;

pub(crate) fn router(state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route(
            "/v1/remote/devices",
            get(list)
                .layer(from_fn_with_state(state.clone(), decrypt_request::<()>))
                .merge(post(register).layer(from_fn_with_state(
                    state.clone(),
                    decrypt_request::<RegisterRemoteDevice>,
                ))),
        )
        .route(
            "/v1/remote/admissions",
            post(admit).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<RemoteAdmissionRequest>,
            )),
        )
        .route(
            "/v1/remote/admissions/check",
            post(check).layer(from_fn_with_state(
                state.clone(),
                decrypt_request::<CheckRemoteAdmission>,
            )),
        )
        .with_state(state)
}

fn db_error(error: DBError) -> ApiError {
    match error {
        DBError::RemoteEnrollmentCapacity => ApiError::TooManyRequests,
        _ => ApiError::InternalServerError,
    }
}

fn binding<'a>(user: &User, deployment: &'a str, kind: &'a str, id: &'a str) -> Binding<'a> {
    Binding {
        account: user.uuid,
        project: user.project_id,
        deployment,
        kind,
        id,
    }
}

pub(crate) fn read_record<T: serde::de::DeserializeOwned>(
    state: &AppState,
    user: &User,
    kind: &str,
    id: &str,
) -> Result<Option<T>, ApiError> {
    let row = state
        .db
        .get_remote_enrollment(user.uuid, kind, id)
        .map_err(db_error)?;
    row.map(|row| {
        binding(user, &state.app_mode.to_string(), kind, id)
            .open(&state.enclave_key, &row.ciphertext)
            .map_err(|_| ApiError::BadRequest)
    })
    .transpose()
}

fn save_record<T: Serialize + serde::de::DeserializeOwned>(
    state: &AppState,
    user: &User,
    kind: &str,
    id: &str,
    nonce: Uuid,
    expires_at: i64,
    value: &T,
) -> Result<T, ApiError> {
    let deployment = state.app_mode.to_string();
    let expected = binding(user, &deployment, kind, id);
    let ciphertext = expected
        .seal(&state.enclave_key, value)
        .map_err(|_| ApiError::InternalServerError)?;
    let row = state
        .db
        .put_remote_enrollment(
            Record {
                user_id: user.uuid,
                project_id: user.project_id,
                kind: kind.into(),
                record_id: id.into(),
                nonce,
                expires_at,
                ciphertext,
            },
            Utc::now().timestamp(),
        )
        .map_err(db_error)?;
    expected
        .open(&state.enclave_key, &row.ciphertext)
        .map_err(|_| ApiError::BadRequest)
}

fn public_key(text: &str) -> Result<VerifyingKey, ApiError> {
    if text.len() != 64
        || text
            .bytes()
            .any(|c| !c.is_ascii_digit() && !(b'a'..=b'f').contains(&c))
    {
        return Err(ApiError::BadRequest);
    }
    let bytes: [u8; 32] = hex::decode(text)
        .map_err(|_| ApiError::BadRequest)?
        .try_into()
        .map_err(|_| ApiError::BadRequest)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| ApiError::BadRequest)
}

fn validate_registration(
    state: &AppState,
    user: &User,
    request: &RegisterRemoteDevice,
) -> Result<(), ApiError> {
    let registration = &request.registration;
    let project = state
        .db
        .get_org_project_by_id(user.project_id)
        .map_err(db_error)?;
    if registration.account != user.uuid
        || registration.project != project.client_id
        || registration.deployment != state.app_mode.to_string()
        || !matches!(registration.channel.as_str(), "unpackaged" | "dev" | "prod")
        || registration.name.is_empty()
        || registration.name.len() > 128
        || registration.name.chars().any(char::is_control)
        || registration.addresses.relays.len() > 4
        || registration.addresses.direct.len() > 16
    {
        return Err(ApiError::BadRequest);
    }
    for address in &registration.addresses.direct {
        address
            .parse::<SocketAddr>()
            .map_err(|_| ApiError::BadRequest)?;
    }
    for address in &registration.addresses.relays {
        let url = url::Url::parse(address).map_err(|_| ApiError::BadRequest)?;
        if address.len() > 512
            || url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ApiError::BadRequest);
        }
    }
    if let Some(window) = &registration.window {
        if !registration.accepting_connections || !(1..=300).contains(&window.remaining_seconds) {
            return Err(ApiError::BadRequest);
        }
    }
    let key = public_key(&registration.endpoint)?;
    let signature = hex::decode(&request.signature).map_err(|_| ApiError::BadRequest)?;
    let signature = Signature::from_slice(&signature).map_err(|_| ApiError::BadRequest)?;
    key.verify_strict(
        &registration
            .proof_message()
            .map_err(|_| ApiError::BadRequest)?,
        &signature,
    )
    .map_err(|_| ApiError::BadRequest)
}

fn prune_old_windows(
    state: &AppState,
    user: &User,
    current_endpoint: &str,
    now: i64,
) -> Result<(), ApiError> {
    let deployment = state.app_mode.to_string();
    for row in state
        .db
        .list_remote_enrollment(user.uuid, "window")
        .map_err(db_error)?
    {
        // Never remove the current endpoint's W before save_record compares it.
        if row.record_id == current_endpoint {
            continue;
        }
        let Ok(window) = binding(user, &deployment, "window", &row.record_id)
            .open::<RemoteWindow>(&state.enclave_key, &row.ciphertext)
        else {
            continue;
        };
        if window.expires_at < now.saturating_sub(WINDOW_RETENTION) {
            state
                .db
                .delete_remote_enrollment_unchanged(&row)
                .map_err(db_error)?;
        }
    }
    Ok(())
}

pub(crate) fn register_inner(
    state: &AppState,
    user: &User,
    request: RegisterRemoteDevice,
) -> Result<RemoteDevice, ApiError> {
    validate_registration(state, user, &request)?;
    let registration = request.registration;
    let now = Utc::now().timestamp();
    if registration.window.is_some() {
        prune_old_windows(state, user, &registration.endpoint, now)?;
    }
    let window = registration
        .window
        .map(|window| {
            let saved = RemoteWindow {
                id: window.id,
                expires_at: now + i64::from(window.remaining_seconds),
            };
            save_record(
                state,
                user,
                "window",
                &registration.endpoint,
                window.id,
                saved.expires_at,
                &saved,
            )
        })
        .transpose()?;
    let device = RemoteDevice {
        channel: registration.channel,
        endpoint: registration.endpoint.clone(),
        name: registration.name,
        addresses: registration.addresses,
        accepting_connections: registration.accepting_connections,
        window,
        expires_at: now + DEVICE_LEASE,
    };
    save_record(
        state,
        user,
        "device",
        &registration.endpoint,
        Uuid::new_v4(),
        device.expires_at,
        &device,
    )
}

async fn register(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Decrypted(request): Decrypted<RegisterRemoteDevice>,
) -> Result<Response, ApiError> {
    let worker = state.clone();
    let value = tokio::task::spawn_blocking(move || register_inner(&worker, &user, request))
        .await
        .map_err(|_| ApiError::InternalServerError)??;
    encrypt_response(&state, &session, &value).await
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(user): Extension<User>,
) -> Result<Response, ApiError> {
    let worker = state.clone();
    let devices = tokio::task::spawn_blocking(move || {
        let now = Utc::now().timestamp();
        let deployment = worker.app_mode.to_string();
        let rows = worker
            .db
            .list_remote_enrollment(user.uuid, "device")
            .map_err(db_error)?;
        let mut devices = Vec::new();
        for row in rows {
            let expected = binding(&user, &deployment, "device", &row.record_id);
            // A corrupt/foreign row is not an available device. Never use its
            // identity columns as the account authority for decryption.
            if let Ok(mut device) =
                expected.open::<RemoteDevice>(&worker.enclave_key, &row.ciphertext)
            {
                if device.expires_at > now && device.endpoint == row.record_id {
                    if device.window.as_ref().is_some_and(|w| w.expires_at <= now) {
                        device.window = None;
                    }
                    devices.push(device);
                }
            }
        }
        Ok::<_, ApiError>(devices)
    })
    .await
    .map_err(|_| ApiError::InternalServerError)??;
    encrypt_response(&state, &session, &RemoteDeviceList { devices }).await
}

#[derive(Serialize, Deserialize)]
struct AdmissionRecord {
    controller: String,
    execution: String,
    window: Uuid,
    expires_at: i64,
}

fn admission_id(handle: &str) -> Result<String, ApiError> {
    if handle.len() != 64 || !handle.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest);
    }
    Ok(hex::encode(Sha256::digest(handle.as_bytes())))
}

pub(crate) fn admit_inner(
    state: &AppState,
    user: &User,
    request: RemoteAdmissionRequest,
) -> Result<RemoteAdmission, ApiError> {
    public_key(&request.controller)?;
    public_key(&request.execution)?;
    if request.controller == request.execution {
        return Err(ApiError::BadRequest);
    }
    let now = Utc::now().timestamp();
    let controller: RemoteDevice =
        read_record(state, user, "device", &request.controller)?.ok_or(ApiError::BadRequest)?;
    let execution: RemoteDevice =
        read_record(state, user, "device", &request.execution)?.ok_or(ApiError::BadRequest)?;
    let window = execution.window.ok_or(ApiError::BadRequest)?;
    if controller.channel != execution.channel
        || controller.expires_at <= now
        || execution.expires_at <= now
        || !execution.accepting_connections
        || window.id != request.window
        || window.expires_at <= now
    {
        return Err(ApiError::BadRequest);
    }
    let expires_at = window.expires_at.min(now + ADMISSION_LEASE);
    let mut random = [0; 32];
    getrandom::getrandom(&mut random).map_err(|_| ApiError::InternalServerError)?;
    let handle = hex::encode(random);
    let id = admission_id(&handle)?;
    let record = AdmissionRecord {
        controller: request.controller,
        execution: request.execution,
        window: request.window,
        expires_at,
    };
    let _: AdmissionRecord = save_record(
        state,
        user,
        "admission",
        &id,
        Uuid::new_v4(),
        expires_at,
        &record,
    )?;
    Ok(RemoteAdmission { handle, expires_at })
}

async fn admit(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Decrypted(request): Decrypted<RemoteAdmissionRequest>,
) -> Result<Response, ApiError> {
    let worker = state.clone();
    let value = tokio::task::spawn_blocking(move || admit_inner(&worker, &user, request))
        .await
        .map_err(|_| ApiError::InternalServerError)??;
    encrypt_response(&state, &session, &value).await
}

async fn check(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<TransportSession>,
    Extension(user): Extension<User>,
    Decrypted(request): Decrypted<CheckRemoteAdmission>,
) -> Result<Response, ApiError> {
    let worker = state.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let id = admission_id(&request.handle)?;
        let record: Option<AdmissionRecord> = read_record(&worker, &user, "admission", &id)?;
        Ok::<_, ApiError>(record.is_some_and(|record| {
            record.expires_at > Utc::now().timestamp()
                && record.controller == request.controller
                && record.execution == request.execution
                && record.window == request.window
        }))
    })
    .await
    .map_err(|_| ApiError::InternalServerError)??;
    encrypt_response(&state, &session, &RemoteAdmissionCheck { valid }).await
}
