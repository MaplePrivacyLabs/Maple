//! Device authority is local and separate from the refresh-token cache. Cloud
//! discovery never creates a grant, and a missing grant is never reconstructed.
use iroh::{EndpointId, SecretKey};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn default_name() -> &'static str {
    static NAME: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        default_name_from(&hostname::get().unwrap_or_default().to_string_lossy())
    });
    &NAME
}

fn default_name_from(hostname: &str) -> String {
    let mut name = String::new();
    for ch in hostname
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(64)
    {
        if name.len() + ch.len_utf8() > 128 {
            break;
        }
        name.push(ch);
    }
    if name.trim().is_empty() {
        if cfg!(target_os = "macos") {
            "Mac".into()
        } else if cfg!(target_os = "windows") {
            "Windows PC".into()
        } else {
            "Linux PC".into()
        }
    } else {
        name.trim().to_string()
    }
}

pub(crate) fn validate_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err("Enter a computer name without control characters.".into());
    }
    if name.len() > 128 || name.chars().count() > 64 {
        return Err("Use a shorter computer name.".into());
    }
    Ok(name)
}

pub use maple_agent::store::records::RemotePeer as Peer;
use maple_agent::store::records::{RemoteIdentity, RemoteNamespace};
use maple_agent::store::{AccountId, Store as Database, StoreResult};

pub struct Store {
    database: Arc<Database>,
    account: AccountId,
    namespace: RemoteNamespace,
    record: RemoteIdentity,
}

/// A durable legacy record, captured before asking the authenticated host to
/// confirm its outgoing grant. It is compared again inside the storage edit.
pub(super) struct HostGrantRecovery {
    pub peer: Peer,
    pub attempts: Vec<[u8; 16]>,
}

impl Store {
    fn scope(
        database: &Database,
        account: &str,
        api: &str,
        project: &str,
        channel: &str,
    ) -> Result<(AccountId, RemoteNamespace), String> {
        let account = database
            .account_id(account)
            .map_err(storage_error)?
            .ok_or("No local account is selected")?;
        Ok((
            account,
            RemoteNamespace {
                api_url: api.to_owned(),
                client_id: project.to_owned(),
                profile: channel.to_owned(),
            },
        ))
    }

    pub fn exists(
        database: &Database,
        api: &str,
        project: &str,
        channel: &str,
        account: &str,
    ) -> Result<bool, String> {
        let (account, namespace) = Self::scope(database, account, api, project, channel)?;
        Ok(database
            .saved_remote_identity(account, &namespace)
            .map_err(storage_error)?
            .is_some())
    }

    pub fn open(
        database: Arc<Database>,
        api: &str,
        project: &str,
        channel: &str,
        account: &str,
    ) -> Result<Self, String> {
        let (account, namespace) = Self::scope(&database, account, api, project, channel)?;
        // This creates only a durable local key, never a network Endpoint.
        let record = database
            .remote_identity(account, &namespace)
            .map_err(storage_error)?;
        validate_record(&record)?;
        Ok(Self {
            database,
            account,
            namespace,
            record,
        })
    }

    fn refresh(&mut self, result: StoreResult<()>) -> Result<(), String> {
        result.map_err(storage_error)?;
        let record = self
            .database
            .saved_remote_identity(self.account, &self.namespace)
            .map_err(storage_error)?
            .ok_or("Remote identity is no longer available")?;
        validate_record(&record)?;
        self.record = record;
        Ok(())
    }

    pub fn key(&self) -> SecretKey {
        SecretKey::from_bytes(self.record.secret.expose())
    }
    pub fn enabled(&self) -> bool {
        self.record.enabled
    }
    pub fn name(&self) -> &str {
        self.record
            .name
            .as_deref()
            .unwrap_or_else(|| default_name())
    }
    pub fn peers(&self) -> &BTreeMap<String, Peer> {
        &self.record.peers
    }
    pub fn permits(&self, peer: EndpointId) -> bool {
        self.record.enabled
            && self
                .record
                .peers
                .get(&peer.to_string())
                .is_some_and(|grant| grant.controls_this_computer)
    }
    pub fn set_name(&mut self, name: &str) -> Result<(), String> {
        let name = validate_name(name)?;
        self.refresh(
            self.database
                .set_remote_name(self.account, &self.namespace, name),
        )
    }
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), String> {
        self.refresh(
            self.database
                .set_remote_enabled(self.account, &self.namespace, enabled),
        )
    }
    pub fn grant(&mut self, key: EndpointId, grant: Peer) -> Result<(), String> {
        self.refresh(self.database.grant_remote_peer(
            self.account,
            &self.namespace,
            &key.to_string(),
            grant,
        ))
    }
    pub fn remember_host_proof(
        &mut self,
        key: EndpointId,
        window: [u8; 16],
        name: String,
    ) -> Result<(), String> {
        self.refresh(self.database.remember_remote_host_proof(
            self.account,
            &self.namespace,
            &key.to_string(),
            window,
            &name,
        ))
    }
    pub fn confirm_host(&mut self, key: EndpointId, window: [u8; 16]) -> Result<(), String> {
        self.refresh(self.database.confirm_remote_host(
            self.account,
            &self.namespace,
            &key.to_string(),
            window,
        ))
    }
    pub(super) fn host_grant_recovery(&self, key: EndpointId) -> Result<HostGrantRecovery, String> {
        let key = key.to_string();
        let peer = self
            .record
            .peers
            .get(&key)
            .filter(|peer| {
                peer.can_connect && peer.host_grant.is_none() && peer.pending_confirmation.is_none()
            })
            .ok_or("Pair this computer again before updating it remotely")?;
        Ok(HostGrantRecovery {
            peer: peer.clone(),
            attempts: self
                .record
                .attempted_windows
                .get(&key)
                .cloned()
                .unwrap_or_default(),
        })
    }
    pub(super) fn recover_host_grant(
        &mut self,
        key: EndpointId,
        expected: &HostGrantRecovery,
        window: [u8; 16],
    ) -> Result<(), String> {
        self.refresh(self.database.recover_remote_host_grant(
            self.account,
            &self.namespace,
            &key.to_string(),
            &expected.peer,
            &expected.attempts,
            window,
        ))
    }
    /// The transaction commits before any code-derived proof is sent.
    pub fn reserve_attempt(&mut self, key: EndpointId, window: [u8; 16]) -> Result<(), String> {
        self.refresh(self.database.reserve_remote_pairing_attempt(
            self.account,
            &self.namespace,
            &key.to_string(),
            window,
        ))
    }
    pub fn revoke(&mut self, key: EndpointId) -> Result<(), String> {
        self.refresh(self.database.revoke_remote_peer(
            self.account,
            &self.namespace,
            &key.to_string(),
        ))
    }
    /// Discovery updates labels of already known peers; authority stays local.
    pub fn refresh_peer_names<'a>(
        &mut self,
        names: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<(), String> {
        let names = names
            .into_iter()
            .filter(|(key, name)| {
                validate_name(name).is_ok()
                    && self
                        .record
                        .peers
                        .get(*key)
                        .is_some_and(|peer| peer.name != *name)
            })
            .map(|(key, name)| (key.to_owned(), name.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if names.is_empty() {
            return Ok(());
        }
        self.refresh(
            self.database
                .refresh_remote_peer_names(self.account, &self.namespace, &names),
        )
    }
}

fn storage_error(error: maple_agent::store::StoreError) -> String {
    format!("Could not access remote device settings: {error}")
}

fn validate_record(record: &RemoteIdentity) -> Result<(), String> {
    for key in record.peers.keys().chain(record.attempted_windows.keys()) {
        if key
            .parse::<EndpointId>()
            .map_err(|_| "Invalid saved remote device identity")?
            .to_string()
            != *key
        {
            return Err("Invalid saved remote device identity".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Database>) {
        let root = tempfile::tempdir().unwrap();
        let database =
            Arc::new(Database::open(maple_agent::store::StorePaths::new(root.path())).unwrap());
        database.upsert_account("a", 1).unwrap();
        database.upsert_account("b", 1).unwrap();
        (root, database)
    }

    #[test]
    fn computer_names_survive_restart_without_changing_identity() {
        let (_root, database) = fixture();
        let open = || Store::open(database.clone(), "local", "p", "c", "a").unwrap();
        let mut store = open();
        let key = store.key().public();
        assert!(store.record.name.is_none());
        assert!(validate_name(open().name()).is_ok());
        store.set_name("  Anthony’s Mac  ").unwrap();
        for invalid in ["", "bad\nname", &"x".repeat(65), &"🦀".repeat(33)] {
            assert!(store.set_name(invalid).is_err());
        }
        drop(store);
        let store = open();
        assert_eq!(store.name(), "Anthony’s Mac");
        assert_eq!(store.key().public(), key);
        assert!(!store.enabled());
        assert!(store.peers().is_empty());
        let other = Store::open(database.clone(), "local", "p", "c", "b").unwrap();
        assert!(other.record.name.is_none());
        assert_eq!(
            default_name_from("  Anthony-Mac.local  "),
            "Anthony-Mac.local"
        );
        for hostname in ["", "\n\t", &"🦀".repeat(70)] {
            assert!(validate_name(&default_name_from(hostname)).is_ok());
        }
    }

    #[test]
    fn discovered_names_never_change_or_restore_pairing_authority() {
        let (_root, database) = fixture();
        let open = || Store::open(database.clone(), "local", "p", "c", "a").unwrap();
        let mut store = open();
        let peer = SecretKey::generate().public();
        let unknown = SecretKey::generate().public().to_string();
        let key = peer.to_string();
        let mut expected = Peer {
            name: "Maple computer".into(),
            grant: [1; 16],
            host_grant: None,
            controls_this_computer: true,
            can_connect: false,
            pending_confirmation: None,
        };
        store.grant(peer, expected.clone()).unwrap();
        store
            .remember_host_proof(peer, [2; 16], "Maple computer".into())
            .unwrap();
        expected.pending_confirmation = Some([2; 16]);
        store
            .refresh_peer_names([(key.as_str(), "Office Mac"), (&unknown, "Unknown")])
            .unwrap();
        expected.name = "Office Mac".into();
        assert!(open().peers()[&key] == expected);
        assert_eq!(store.peers().len(), 1);
        assert!(!store.enabled());
        store
            .refresh_peer_names([(key.as_str(), "bad\nname")])
            .unwrap();
        assert!(store.peers()[&key] == expected);
        store.revoke(peer).unwrap();
        store
            .refresh_peer_names([(key.as_str(), "Office Mac")])
            .unwrap();
        assert!(open().peers().is_empty());
    }

    #[test]
    fn identities_and_grants_survive_restart_but_not_scope_changes() {
        let (_root, database) = fixture();
        let open = |account| {
            Store::open(database.clone(), "local", "project", "unpackaged", account).unwrap()
        };
        let mut a = open("a");
        let key = a.key().public();
        let peer = SecretKey::generate().public();
        a.set_enabled(true).unwrap();
        a.grant(
            peer,
            Peer {
                name: "Test".into(),
                grant: [1; 16],
                host_grant: None,
                controls_this_computer: true,
                can_connect: false,
                pending_confirmation: None,
            },
        )
        .unwrap();
        drop(a);
        let mut a = open("a");
        assert_eq!(a.key().public(), key);
        assert!(a.permits(peer));
        assert_ne!(open("b").key().public(), key);
        assert!(!open("b").permits(peer));
        assert_ne!(
            Store::open(database.clone(), "other", "project", "unpackaged", "a")
                .unwrap()
                .key()
                .public(),
            key
        );
        a.revoke(peer).unwrap();
        drop(a);
        assert!(!open("a").permits(peer));
    }

    #[test]
    fn lost_ack_and_reverse_pairing_preserve_roles_but_revocation_wins() {
        let (_root, database) = fixture();
        let open = || Store::open(database.clone(), "local", "p", "c", "a").unwrap();
        let peer = SecretKey::generate().public();
        let mut store = open();
        store.set_enabled(true).unwrap();
        store.reserve_attempt(peer, [1; 16]).unwrap();
        store
            .remember_host_proof(peer, [1; 16], "B".into())
            .unwrap();
        assert!(!store.peers()[&peer.to_string()].can_connect);
        assert_eq!(store.peers()[&peer.to_string()].host_grant, None);
        drop(store);
        let mut store = open();
        assert!(store.reserve_attempt(peer, [1; 16]).is_err());
        assert_eq!(store.peers()[&peer.to_string()].host_grant, None);
        store.confirm_host(peer, [1; 16]).unwrap();
        drop(store);
        let mut store = open();
        assert_eq!(store.peers()[&peer.to_string()].host_grant, Some([1; 16]));
        store
            .grant(
                peer,
                Peer {
                    name: "B".into(),
                    grant: [2; 16],
                    host_grant: None,
                    controls_this_computer: true,
                    can_connect: false,
                    pending_confirmation: None,
                },
            )
            .unwrap();
        assert!(store.permits(peer));
        assert!(store.peers()[&peer.to_string()].can_connect);
        assert_eq!(store.peers()[&peer.to_string()].host_grant, Some([1; 16]));
        store
            .remember_host_proof(peer, [3; 16], "B".into())
            .unwrap();
        assert!(store.permits(peer));
        assert_eq!(store.peers()[&peer.to_string()].grant, [2; 16]);
        store.revoke(peer).unwrap();
        assert!(store.confirm_host(peer, [3; 16]).is_err());
        assert!(store.reserve_attempt(peer, [1; 16]).is_err());
        assert!(!open().permits(peer));
    }

    #[test]
    fn previous_identity_files_are_untouched_and_never_adopted() {
        let (root, database) = fixture();
        let previous = root.path().join("remote").join("a.json");
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::write(&previous, b"previous identity sentinel").unwrap();
        assert!(!Store::exists(&database, "local", "p", "c", "a").unwrap());
        let store = Store::open(database.clone(), "local", "p", "c", "a").unwrap();
        let key = store.key().public();
        drop(store);
        assert_eq!(
            std::fs::read(&previous).unwrap(),
            b"previous identity sentinel"
        );
        database.close().unwrap();
        assert!(Store::open(database.clone(), "local", "p", "c", "a").is_err());
        drop(database);
        let reopened =
            Arc::new(Database::open(maple_agent::store::StorePaths::new(root.path())).unwrap());
        assert_eq!(
            Store::open(reopened.clone(), "local", "p", "c", "a")
                .unwrap()
                .key()
                .public(),
            key
        );
        reopened.close().unwrap();
    }
}
