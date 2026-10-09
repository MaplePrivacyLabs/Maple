//! Remote development: this machine as a host ([`host`]) and as a client
//! of other hosts ([`client`]), plus the files both roles keep under
//! `<local data>/remote/`.
//!
//! The machine owns its two static keys and the hosting lock; what belongs
//! to one account (the devices paired into it, the code it accepts, the
//! hosts it saved) lives under that account's scope, so accounts on one
//! machine never see each other's peers.

#[cfg(feature = "desktop")]
pub mod client;
#[cfg(feature = "serve")]
pub mod host;

#[cfg(feature = "serve")]
use std::path::{Path, PathBuf};

#[cfg(feature = "serve")]
use maple_remote::devices::DeviceStore;
#[cfg(feature = "serve")]
use maple_remote::pairing::PendingPairingStore;

/// Default listen address for a host. Not 8080, which the proxy mode uses.
/// Read by the `serve` command line in every build, so it lives outside
/// the feature gate.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:7130";

/// Where this machine keeps its keys and the hosting lock. Created
/// owner-only on first use.
#[cfg(feature = "serve")]
pub fn remote_dir() -> Result<PathBuf, String> {
    private_dir(crate::backend::local_data_root().join("remote"))
}

/// Where the host keeps what belongs to one account: the devices paired
/// into it and the pairing code it currently accepts. Created owner-only
/// on first use.
#[cfg(feature = "serve")]
pub fn account_remote_dir(user_id: &str) -> Result<PathBuf, String> {
    private_dir(account_remote_dir_under(&remote_dir()?, user_id)?)
}

/// Pure path arithmetic behind [`account_remote_dir`]: the account's
/// directory under the machine's remote directory.
#[cfg(feature = "serve")]
fn account_remote_dir_under(remote_dir: &Path, user_id: &str) -> Result<PathBuf, String> {
    let scope = maple_agent::maple_api::account_scope(user_id)?;
    Ok(remote_dir.join("accounts").join(scope))
}

#[cfg(feature = "serve")]
fn private_dir(dir: PathBuf) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// The devices paired into the account whose directory is `account_dir`.
#[cfg(feature = "serve")]
pub fn device_store(account_dir: &Path) -> DeviceStore {
    DeviceStore::new(account_dir.join("devices.json"))
}

/// The pairing code the account whose directory is `account_dir` accepts.
#[cfg(feature = "serve")]
pub fn pending_pairing_store(account_dir: &Path) -> PendingPairingStore {
    PendingPairingStore::new(account_dir.join("pending_pairing.json"))
}

#[cfg(all(test, feature = "serve"))]
mod tests {
    use super::*;

    #[test]
    fn devices_and_codes_live_under_the_account() {
        let root = Path::new("/data/remote");
        let a = account_remote_dir_under(root, "user-a").unwrap();
        let b = account_remote_dir_under(root, "user-b").unwrap();
        assert_ne!(a, b, "two accounts never share a device list");
        assert_eq!(
            a,
            account_remote_dir_under(root, " USER-A ").unwrap(),
            "the scope follows the normalized account id"
        );
        assert_eq!(a.parent().unwrap().parent().unwrap(), root);
        assert_eq!(a.parent().unwrap().file_name().unwrap(), "accounts");
        assert_eq!(
            device_store(&a).path(),
            a.join("devices.json"),
            "the device file sits in the account directory"
        );
        assert_eq!(
            pending_pairing_store(&a).path(),
            a.join("pending_pairing.json")
        );
        assert!(account_remote_dir_under(root, "  ").is_err());
    }
}
