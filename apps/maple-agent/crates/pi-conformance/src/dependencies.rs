//! Dependency boundary checks over local manifests and the committed lockfile.

use crate::{CheckResult, toml};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use toml::Value;

pub const PI_CRATES: [&str; 6] = [
    "pi-ai",
    "pi-agent-core",
    "pi-coding-agent",
    "pi-mcp",
    "pi-testkit",
    "pi-conformance",
];

#[derive(Debug, Deserialize)]
struct Lockfile {
    package: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

fn dependency_tables<'a>(
    manifest: &'a Value,
    out: &mut Vec<(&'a str, &'a toml::map::Map<String, Value>)>,
) {
    for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(kind).and_then(Value::as_table) {
            out.push((kind, table));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            dependency_tables(target, out);
        }
    }
}

fn path_dependency(
    owner: &str,
    kind: &str,
    dependency: &str,
    path: &Path,
    root: &Path,
) -> CheckResult {
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("{owner} dependency {dependency}: {error}"))?;
    let allowed: BTreeMap<PathBuf, &str> = PI_CRATES
        .into_iter()
        .map(|name| (root.join("crates").join(name), name))
        .collect();
    let Some(target) = allowed.get(&resolved) else {
        return Err(format!(
            "{owner} {kind} {dependency} points outside the six Pi crates: {}",
            resolved.display()
        ));
    };
    if kind != "dev-dependencies"
        && !matches!(owner, "pi-testkit" | "pi-conformance")
        && matches!(*target, "pi-testkit" | "pi-conformance")
    {
        return Err(format!(
            "production crate {owner} depends on test support {target} through {kind}"
        ));
    }
    Ok(())
}

fn forbidden(name: &str) -> bool {
    name == "maple" || name.starts_with("maple-") || name.starts_with("opensecret")
}

fn verify_graph(packages: &[Package]) -> CheckResult {
    let mut visited = BTreeSet::new();
    let mut queue: Vec<usize> = packages
        .iter()
        .enumerate()
        .filter(|(_, package)| PI_CRATES.contains(&package.name.as_str()))
        .map(|(index, _)| index)
        .collect();
    for name in PI_CRATES {
        if !packages
            .iter()
            .any(|package| package.name == name && package.source.is_none())
        {
            return Err(format!("Cargo.lock is missing workspace crate {name}"));
        }
    }
    while let Some(index) = queue.pop() {
        if !visited.insert(index) {
            continue;
        }
        let package = &packages[index];
        if forbidden(&package.name)
            || (package.source.is_none() && !PI_CRATES.contains(&package.name.as_str()))
        {
            return Err(format!(
                "Pi dependency graph reaches forbidden package {} {}",
                package.name, package.version
            ));
        }
        for dependency in &package.dependencies {
            let mut components = dependency.split_whitespace();
            let name = components.next().ok_or("empty dependency in Cargo.lock")?;
            let version = components.next();
            let source = components
                .next()
                .map(|source| {
                    source
                        .strip_prefix('(')
                        .and_then(|source| source.strip_suffix(')'))
                        .ok_or_else(|| {
                            format!("invalid source in Cargo.lock dependency {dependency:?}")
                        })
                })
                .transpose()?;
            if components.next().is_some() {
                return Err(format!("invalid Cargo.lock dependency {dependency:?}"));
            }
            let matches: Vec<_> = packages
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.name == name
                        && version.is_none_or(|version| version == candidate.version)
                        && source.is_none_or(|source| candidate.source.as_deref() == Some(source))
                })
                .map(|(index, _)| index)
                .collect();
            if matches.len() != 1 {
                return Err(format!(
                    "ambiguous or missing lockfile dependency {dependency:?} of {}",
                    package.name
                ));
            }
            queue.extend(matches);
        }
    }
    Ok(())
}

pub fn check(root: &Path) -> CheckResult {
    let workspace: Value = toml(&root.join("Cargo.toml"))?;
    let shared = workspace
        .get("workspace")
        .and_then(|value| value.get("dependencies"))
        .and_then(Value::as_table)
        .ok_or("missing workspace.dependencies")?;
    for owner in PI_CRATES {
        let directory = root.join("crates").join(owner);
        let manifest: Value = toml(&directory.join("Cargo.toml"))?;
        if manifest
            .get("package")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            != Some(owner)
        {
            return Err(format!("unexpected package identity in {owner}/Cargo.toml"));
        }
        if matches!(owner, "pi-testkit" | "pi-conformance")
            && manifest
                .get("package")
                .and_then(|value| value.get("publish"))
                .and_then(Value::as_bool)
                != Some(false)
        {
            return Err(format!("{owner} must have publish = false"));
        }
        let mut tables = Vec::new();
        dependency_tables(&manifest, &mut tables);
        for (kind, table) in tables {
            for (alias, original) in table {
                let inherited = original.get("workspace").and_then(Value::as_bool) == Some(true);
                let dependency = if inherited {
                    shared
                        .get(alias)
                        .ok_or_else(|| format!("unresolved workspace dependency {alias}"))?
                } else {
                    original
                };
                let name = dependency
                    .get("package")
                    .and_then(Value::as_str)
                    .unwrap_or(alias);
                if forbidden(name) {
                    return Err(format!("{owner} depends on forbidden package {name}"));
                }
                if let Some(path) = dependency.get("path").and_then(Value::as_str) {
                    let base = if inherited { root } else { &directory };
                    path_dependency(owner, kind, name, &base.join(path), root)?;
                }
            }
        }
    }
    let lockfile: Lockfile = toml(&root.join("Cargo.lock"))?;
    verify_graph(&lockfile.package)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<Package> {
        PI_CRATES
            .into_iter()
            .map(|name| Package {
                name: name.into(),
                version: "0.1.0".into(),
                source: None,
                dependencies: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn ignores_unreachable_maple_but_rejects_transitive_local_dependencies() {
        let mut packages = roots();
        packages.push(Package {
            name: "maple-sdk".into(),
            version: "0.1.0".into(),
            source: None,
            dependencies: Vec::new(),
        });
        assert!(verify_graph(&packages).is_ok());
        packages[0].dependencies.push("third-party".into());
        packages.push(Package {
            name: "third-party".into(),
            version: "1.0.0".into(),
            source: Some("registry+test".into()),
            dependencies: vec!["maple-sdk".into()],
        });
        assert!(verify_graph(&packages).unwrap_err().contains("maple-sdk"));
    }

    #[test]
    fn rejects_renamed_unknown_local_package_in_reachable_graph() {
        let mut packages = roots();
        packages[0].dependencies.push("innocent-name".into());
        packages.push(Package {
            name: "innocent-name".into(),
            version: "1.0.0".into(),
            source: None,
            dependencies: Vec::new(),
        });
        assert!(
            verify_graph(&packages)
                .unwrap_err()
                .contains("innocent-name")
        );
    }
}
