//! Cheap corpus freshness checks. These never launch Node or fetch source.

use crate::{CheckResult, json};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub pin: Value,
    pub node_version: String,
    pub upstream_lockfile_sha256: String,
    pub input_hashes: BTreeMap<String, String>,
    pub output_hashes: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pin {
    tag: String,
    rev: String,
    catalog_revision: String,
    nixpkgs: String,
    package_lock_sha256: String,
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn check_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// A manifest path must remain within its declared root on every platform.
pub fn safe_relative(path: &str) -> CheckResult {
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        || Path::new(path).is_absolute()
    {
        return Err(format!("unsafe manifest path {path:?}"));
    }
    Ok(())
}

pub fn files(root: &Path) -> CheckResult<BTreeMap<String, String>> {
    fn visit(root: &Path, directory: &Path, out: &mut BTreeMap<String, String>) -> CheckResult {
        let entries =
            fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_symlink() {
                return Err(format!(
                    "reference data must not contain symlinks: {}",
                    path.display()
                ));
            }
            if kind.is_dir() {
                visit(root, &path, out)?;
            } else if kind.is_file() {
                let name = path
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_str()
                    .ok_or("reference path is not UTF-8")?
                    .replace('\\', "/");
                safe_relative(&name)?;
                let data =
                    fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                out.insert(name, sha256(&data));
            } else {
                return Err(format!(
                    "reference data must be regular files: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result)?;
    Ok(result)
}

fn compare_hashes(
    label: &str,
    recorded: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> CheckResult {
    for (path, hash) in recorded {
        safe_relative(path)?;
        if !check_hash(hash) {
            return Err(format!("{label} has invalid SHA-256 for {path}"));
        }
    }
    let missing: Vec<_> = actual
        .keys()
        .filter(|key| !recorded.contains_key(*key))
        .collect();
    let removed: Vec<_> = recorded
        .keys()
        .filter(|key| !actual.contains_key(*key))
        .collect();
    let changed: Vec<_> = actual
        .iter()
        .filter(|(key, hash)| recorded.get(*key).is_some_and(|value| value != *hash))
        .map(|(key, _)| key)
        .collect();
    if !missing.is_empty() || !removed.is_empty() || !changed.is_empty() {
        return Err(format!(
            "{label} is stale; unrecorded: {missing:?}; removed: {removed:?}; changed: {changed:?}. Re-record the TypeScript corpus."
        ));
    }
    Ok(())
}

pub fn check(root: &Path) -> CheckResult {
    let manifest: Manifest = json(&root.join("corpus/manifest.json"))?;
    if manifest.schema_version != 1 {
        return Err("unsupported corpus manifest schema".into());
    }
    let pin: Value = json(&root.join("pin.json"))?;
    let typed_pin: Pin = serde_json::from_value(pin.clone())
        .map_err(|error| format!("invalid pin.json: {error}"))?;
    let revision = |value: &str| {
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if !typed_pin.tag.starts_with('v')
        || !revision(&typed_pin.rev)
        || !revision(&typed_pin.nixpkgs)
        || !typed_pin
            .catalog_revision
            .strip_prefix("sha256-")
            .is_some_and(check_hash)
        || !check_hash(&typed_pin.package_lock_sha256)
    {
        return Err("invalid tag, revision, catalog, or package-lock hash in pin.json".into());
    }
    if pin != manifest.pin {
        return Err("corpus manifest pin differs from pin.json".into());
    }
    let lock: Value = json(&root.join("flake.lock"))?;
    if lock.pointer("/nodes/pi/locked/rev") != pin.get("rev")
        || lock.pointer("/nodes/nixpkgs/locked/rev") != pin.get("nixpkgs")
    {
        return Err("reference flake.lock differs from pin.json".into());
    }
    if !check_hash(&manifest.upstream_lockfile_sha256)
        || pin.get("packageLockSha256").and_then(Value::as_str)
            != Some(&manifest.upstream_lockfile_sha256)
    {
        return Err("upstream package-lock.json hash differs from pin.json".into());
    }
    if manifest.node_version != "v22.23.2" {
        return Err(format!(
            "unexpected reference Node version {}",
            manifest.node_version
        ));
    }
    let mut inputs = BTreeMap::new();
    for name in ["scenarios", "recorder", "nix", "fixtures", "selection"] {
        let directory = root.join(name);
        if directory.exists() {
            for (file, hash) in files(&directory)? {
                inputs.insert(format!("{name}/{file}"), hash);
            }
        }
    }
    for name in ["pin.json", "flake.lock", "flake.nix"] {
        let bytes = fs::read(root.join(name)).map_err(|error| format!("{name}: {error}"))?;
        inputs.insert(name.into(), sha256(&bytes));
    }
    compare_hashes("corpus input manifest", &manifest.input_hashes, &inputs)?;
    let mut outputs = files(&root.join("corpus"))?;
    outputs.remove("manifest.json");
    compare_hashes("corpus output manifest", &manifest.output_hashes, &outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_escape_or_change_meaning_on_windows() {
        for path in ["../x", "/x", "x/../y", "x\\y", "C:/x", "./x", "x//y", ""] {
            assert!(safe_relative(path).is_err(), "{path:?}");
        }
        assert!(safe_relative("scenarios/agent/basic-text-turn/scenario.json").is_ok());
    }

    #[test]
    fn input_removal_addition_and_mutation_are_all_stale() {
        let recorded = BTreeMap::from([("one".to_owned(), sha256(b"one"))]);
        assert!(compare_hashes("test", &recorded, &recorded).is_ok());
        assert!(compare_hashes("test", &recorded, &BTreeMap::new()).is_err());
        assert!(compare_hashes("test", &BTreeMap::new(), &recorded).is_err());
        assert!(
            compare_hashes(
                "test",
                &recorded,
                &BTreeMap::from([("one".to_owned(), sha256(b"two"))])
            )
            .is_err()
        );
    }
}
