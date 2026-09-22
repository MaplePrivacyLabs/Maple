//! The hosts a client has paired with.
//!
//! One JSON file per account. A host is identified by its static public
//! key and may be reachable through several connections; adding a second
//! address to a host the client already knows merges into that host rather
//! than creating another. Loading salvages per entry: a malformed
//! connection is dropped, not the host, and a malformed host is dropped,
//! not the file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::now_ms;

/// One way to reach a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostConnection {
    /// Plain WebSocket to `host:port` on a LAN or a Tailscale network.
    Direct { address: String },
}

impl HostConnection {
    pub fn label(&self) -> &str {
        match self {
            Self::Direct { address } => address,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedHost {
    /// The host's static public key.
    pub id: String,
    pub name: String,
    pub connections: Vec<HostConnection>,
    pub paired_at_ms: u64,
    /// The version the host announced at the last successful hello, so an
    /// offline host still shows what it ran. Absent until it connected once
    /// on a build that records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_version: Option<String>,
    /// The build the host announced then, when its build knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_build: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: Vec<serde_json::Value>,
}

pub struct HostsStore {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Decode one saved host, dropping connections that do not parse.
fn salvage_host(value: serde_json::Value) -> Option<SavedHost> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?.to_string();
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("host")
        .to_string();
    let paired_at_ms = object
        .get("pairedAtMs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let connections = object
        .get("connections")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let string = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Some(SavedHost {
        id,
        name,
        connections,
        paired_at_ms,
        last_seen_version: string("lastSeenVersion"),
        last_seen_build: string("lastSeenBuild"),
    })
}

impl HostsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<Vec<SavedHost>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: HostsFile = serde_json::from_slice(&bytes).map_err(|error| {
                    format!("{} is not a hosts file: {error}", self.path.display())
                })?;
                Ok(file.hosts.into_iter().filter_map(salvage_host).collect())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!("cannot read {}: {error}", self.path.display())),
        }
    }

    fn write(&self, hosts: &[SavedHost]) -> Result<(), String> {
        let file = HostsFile {
            hosts: hosts
                .iter()
                .map(|host| serde_json::to_value(host).unwrap_or(serde_json::Value::Null))
                .collect(),
        };
        maple_agent::private_file::write_private_json(&self.path, &file)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    pub fn list(&self) -> Result<Vec<SavedHost>, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.read()
    }

    pub fn get(&self, id: &str) -> Result<Option<SavedHost>, String> {
        Ok(self.list()?.into_iter().find(|host| host.id == id))
    }

    /// Save a host. A host with the same key already saved keeps its
    /// record and gains the new connections; the name changes only when
    /// the saved one is empty, and the last seen version only when the
    /// new record names one.
    pub fn upsert(&self, host: SavedHost) -> Result<SavedHost, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hosts = self.read()?;
        let merged = match hosts.iter_mut().find(|saved| saved.id == host.id) {
            Some(saved) => {
                for connection in host.connections {
                    if !saved.connections.contains(&connection) {
                        saved.connections.push(connection);
                    }
                }
                if saved.name.trim().is_empty() {
                    saved.name = host.name;
                }
                if host.last_seen_version.is_some() {
                    saved.last_seen_version = host.last_seen_version;
                    saved.last_seen_build = host.last_seen_build;
                }
                saved.clone()
            }
            None => {
                let mut host = host;
                if host.paired_at_ms == 0 {
                    host.paired_at_ms = now_ms();
                }
                hosts.push(host.clone());
                host
            }
        };
        self.write(&hosts)?;
        Ok(merged)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hosts = self.read()?;
        let host = hosts
            .iter_mut()
            .find(|host| host.id == id)
            .ok_or_else(|| "no such host".to_string())?;
        host.name = name.trim().to_string();
        self.write(&hosts)
    }

    /// Record what a host announced at a successful hello. A host that is
    /// no longer saved is ignored; an unchanged version is not rewritten.
    pub fn record_last_seen(
        &self,
        id: &str,
        version: &str,
        build: Option<&str>,
    ) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hosts = self.read()?;
        let Some(host) = hosts.iter_mut().find(|host| host.id == id) else {
            return Ok(());
        };
        if host.last_seen_version.as_deref() == Some(version)
            && host.last_seen_build.as_deref() == build
        {
            return Ok(());
        }
        host.last_seen_version = Some(version.to_string());
        host.last_seen_build = build.map(str::to_string);
        self.write(&hosts)
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hosts = self.read()?;
        hosts.retain(|host| host.id != id);
        self.write(&hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_merge_on_identity_and_salvage_bad_entries() {
        let dir = std::env::temp_dir().join(format!("maple-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = HostsStore::new(dir.join("hosts.json"));
        store
            .upsert(SavedHost {
                id: "k1".into(),
                name: "workstation".into(),
                connections: vec![HostConnection::Direct {
                    address: "100.64.0.7:7130".into(),
                }],
                paired_at_ms: 0,
                last_seen_version: None,
                last_seen_build: None,
            })
            .unwrap();
        // Pairing again over another address merges into the same host.
        let merged = store
            .upsert(SavedHost {
                id: "k1".into(),
                name: "other name".into(),
                connections: vec![
                    HostConnection::Direct {
                        address: "192.168.1.20:7130".into(),
                    },
                    HostConnection::Direct {
                        address: "100.64.0.7:7130".into(),
                    },
                ],
                paired_at_ms: 0,
                last_seen_version: None,
                last_seen_build: None,
            })
            .unwrap();
        assert_eq!(merged.name, "workstation");
        assert_eq!(merged.connections.len(), 2);
        assert!(merged.paired_at_ms > 0);
        assert_eq!(store.list().unwrap().len(), 1);

        store.rename("k1", " box ").unwrap();
        assert_eq!(store.get("k1").unwrap().unwrap().name, "box");

        // A hand-edited file with one bad connection and one bad host.
        std::fs::write(
            store.path(),
            r#"{"hosts":[
                {"id":"k1","name":"box","pairedAtMs":5,"connections":[
                    {"kind":"direct","address":"a:1"},
                    {"kind":"teleport","where":"nowhere"}
                ]},
                {"name":"no id"},
                7
            ]}"#,
        )
        .unwrap();
        let hosts = store.list().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].connections.len(), 1);

        store.remove("k1").unwrap();
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_last_seen_version_persists_and_a_file_without_one_still_loads() {
        let dir = std::env::temp_dir().join(format!("maple-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = HostsStore::new(dir.join("hosts.json"));
        // A file written before the version was recorded.
        std::fs::write(
            store.path(),
            r#"{"hosts":[{"id":"k1","name":"box","pairedAtMs":5,"connections":[]}]}"#,
        )
        .unwrap();
        let host = store.get("k1").unwrap().unwrap();
        assert_eq!(host.last_seen_version, None);
        assert_eq!(host.last_seen_build, None);
        assert!(
            !std::fs::read_to_string(store.path())
                .unwrap()
                .contains("lastSeen"),
            "nothing is written until a hello is recorded"
        );

        // Recording an unknown host is not an error and writes nothing.
        store.record_last_seen("k9", "0.1.0", None).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);

        store
            .record_last_seen("k1", "0.1.0", Some("63bcff5c"))
            .unwrap();
        let host = store.get("k1").unwrap().unwrap();
        assert_eq!(host.last_seen_version.as_deref(), Some("0.1.0"));
        assert_eq!(host.last_seen_build.as_deref(), Some("63bcff5c"));
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains(r#""lastSeenVersion": "0.1.0""#), "{text}");
        assert!(text.contains(r#""lastSeenBuild": "63bcff5c""#), "{text}");

        // A host that lost its build keeps the version and drops the build.
        store.record_last_seen("k1", "0.2.0", None).unwrap();
        let host = store.get("k1").unwrap().unwrap();
        assert_eq!(host.last_seen_version.as_deref(), Some("0.2.0"));
        assert_eq!(host.last_seen_build, None);

        // Pairing again does not erase what was seen unless the new record
        // names a version.
        let merged = store
            .upsert(SavedHost {
                id: "k1".into(),
                name: String::new(),
                connections: Vec::new(),
                paired_at_ms: 0,
                last_seen_version: None,
                last_seen_build: None,
            })
            .unwrap();
        assert_eq!(merged.last_seen_version.as_deref(), Some("0.2.0"));
        let merged = store
            .upsert(SavedHost {
                id: "k1".into(),
                name: String::new(),
                connections: Vec::new(),
                paired_at_ms: 0,
                last_seen_version: Some("0.3.0".into()),
                last_seen_build: Some("abc1234".into()),
            })
            .unwrap();
        assert_eq!(merged.last_seen_version.as_deref(), Some("0.3.0"));
        assert_eq!(merged.last_seen_build.as_deref(), Some("abc1234"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
