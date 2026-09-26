//! The devices a host has paired with.
//!
//! One JSON file at mode 0600. A device is its static public key; the name
//! and account are what the device claimed in its last handshake, kept for
//! display. Revoking a device removes it here; the host's listener notices
//! and drops the device's live connections.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::now_ms;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub public_key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub paired_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default)]
    devices: Vec<PairedDevice>,
}

pub struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Longest device name kept. A client claims its own name; it is display
/// text, not identity.
pub const MAX_DEVICE_NAME_CHARS: usize = 64;

/// A device name as the host keeps and shows it: control characters
/// removed, cut to [`MAX_DEVICE_NAME_CHARS`], and trimmed.
pub fn clean_device_name(name: &str) -> String {
    name.chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

impl DeviceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<DeviceFile, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("{} is not a device file: {error}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DeviceFile::default()),
            Err(error) => Err(format!("cannot read {}: {error}", self.path.display())),
        }
    }

    fn write(&self, file: &DeviceFile) -> Result<(), String> {
        maple_agent::private_file::write_private_json(&self.path, file)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    pub fn list(&self) -> Result<Vec<PairedDevice>, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self.read()?.devices)
    }

    pub fn is_paired(&self, public_key: &str) -> bool {
        self.list()
            .map(|devices| devices.iter().any(|device| device.public_key == public_key))
            .unwrap_or(false)
    }

    /// Record a device that just paired. Pairing again with the same key
    /// keeps the record and refreshes its name.
    pub fn insert(&self, public_key: &str, name: &str) -> Result<PairedDevice, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.read()?;
        let now = now_ms();
        let name = clean_device_name(name);
        let device = match file
            .devices
            .iter_mut()
            .find(|device| device.public_key == public_key)
        {
            Some(existing) => {
                existing.name = name;
                existing.last_seen_ms = Some(now);
                existing.clone()
            }
            None => {
                let device = PairedDevice {
                    public_key: public_key.to_string(),
                    name,
                    user_id: None,
                    paired_at_ms: now,
                    last_seen_ms: Some(now),
                };
                file.devices.push(device.clone());
                device
            }
        };
        self.write(&file)?;
        Ok(device)
    }

    /// A paired device connected: keep what it claims about itself.
    pub fn touch(&self, public_key: &str, name: &str, user_id: Option<&str>) -> Result<(), String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.read()?;
        let Some(device) = file
            .devices
            .iter_mut()
            .find(|device| device.public_key == public_key)
        else {
            return Ok(());
        };
        let name = clean_device_name(name);
        if !name.is_empty() {
            device.name = name;
        }
        device.user_id = user_id.map(str::to_string);
        device.last_seen_ms = Some(now_ms());
        self.write(&file)
    }

    /// Remove a device by public key, or by name when no key matches. A
    /// name that matches several devices is refused; use the key.
    pub fn revoke(&self, key_or_name: &str) -> Result<PairedDevice, String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.read()?;
        if let Some(index) = file
            .devices
            .iter()
            .position(|device| device.public_key == key_or_name)
        {
            let removed = file.devices.remove(index);
            self.write(&file)?;
            return Ok(removed);
        }
        let matches: Vec<usize> = file
            .devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.name == key_or_name)
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [] => Err(format!("no paired device matches {key_or_name:?}")),
            [index] => {
                let removed = file.devices.remove(*index);
                self.write(&file)?;
                Ok(removed)
            }
            _ => Err(format!(
                "{key_or_name:?} names several devices; revoke by public key"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_insert_touch_and_revoke() {
        let dir = std::env::temp_dir().join(format!("maple-devices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = DeviceStore::new(dir.join("devices.json"));
        assert!(store.list().unwrap().is_empty());
        assert!(!store.is_paired("k1"));
        store.insert("k1", "laptop").unwrap();
        store.insert("k2", "laptop").unwrap();
        assert!(store.is_paired("k1"));
        store.touch("k1", "bens-laptop", Some("user-1")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].name, "bens-laptop");
        assert_eq!(listed[0].user_id.as_deref(), Some("user-1"));
        assert!(
            store.revoke("laptop").is_ok(),
            "only k2 is still named laptop"
        );
        assert!(store.revoke("nobody").is_err());
        store.insert("k1", "laptop").unwrap();
        assert_eq!(store.revoke("k1").unwrap().public_key, "k1");
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn revoke_prefers_the_key_and_names_are_kept_short_and_printable() {
        let dir = std::env::temp_dir().join(format!("maple-devices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = DeviceStore::new(dir.join("devices.json"));
        store.insert("k1", "laptop").unwrap();
        // A device that names itself after another device's key.
        store.insert("k2", "k1").unwrap();
        assert_eq!(
            store.revoke("k1").unwrap().public_key,
            "k1",
            "the key match wins over the name match"
        );
        assert_eq!(
            store.revoke("k1").unwrap().public_key,
            "k2",
            "with no key left to match, the name is used"
        );

        let long = format!("a\u{0}b\tc\u{7f}{}", "x".repeat(100));
        store.insert("k3", &long).unwrap();
        let name = store.list().unwrap()[0].name.clone();
        assert_eq!(name.chars().count(), MAX_DEVICE_NAME_CHARS);
        assert!(name.starts_with("abcxxx"), "{name}");
        assert!(name.chars().all(|ch| !ch.is_control()));
        store.touch("k3", "\u{1b}[31m", None).unwrap();
        assert_eq!(
            store.list().unwrap()[0].name,
            "[31m",
            "escape codes are stripped on touch"
        );
        store.touch("k3", "\u{0}\u{1}", None).unwrap();
        assert_eq!(
            store.list().unwrap()[0].name,
            "[31m",
            "a name that cleans to nothing keeps the old one"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
