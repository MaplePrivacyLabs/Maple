//! Desktop remote access is owned by the profile's local daemon.
mod network;
mod pairing;
mod store;
pub(crate) mod streams;
pub(crate) mod update;
pub(crate) mod wire;

pub mod control;
pub mod manager;

pub(crate) use store::validate_name;

/// Seed only durable local remote policy for host lifecycle fixtures. No
/// endpoint or authenticated transport is created, and Store stays private.
#[cfg(test)]
pub(crate) fn enable_saved_remote_for_test(
    backend: &crate::backend::LocalBackend,
    user: &str,
) -> Result<String, String> {
    let mut saved = store::Store::open(
        backend.store().ok_or("Test store is unavailable")?.clone(),
        backend.remote_api_url(),
        &crate::backend::local::configured_client_id().to_string(),
        crate::profile::PROFILE,
        user,
    )?;
    saved.set_enabled(true)?;
    Ok(saved.key().public().to_string())
}
