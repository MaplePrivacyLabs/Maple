use crate::Error;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeManifest {
    pub protocol_version: u32,
    pub implementation: String,
    pub version: String,
    pub distribution: String,
    pub executable: PathBuf,
    pub worker: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagedPython {
    pub manifest: PathBuf,
    pub executable: PathBuf,
    pub worker: PathBuf,
    pub implementation: String,
    pub version: String,
    pub distribution: String,
}
impl PackagedPython {
    pub fn from_manifest(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let missing = |error| {
            Error::Unavailable(format!(
                "Bundled Python is unavailable at {}: {error}. Run `just python-prepare` or reinstall Maple.",
                path.display()
            ))
        };
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(missing)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(missing)?;
        if bytes.len() > 64 * 1024 {
            return Err(Error::Unavailable(
                "Python runtime manifest exceeds 64 KiB".into(),
            ));
        }
        let metadata: RuntimeManifest = serde_json::from_slice(&bytes).map_err(|error| {
            Error::Unavailable(format!("Invalid Python runtime manifest: {error}"))
        })?;
        if metadata.protocol_version != 1
            || metadata.implementation != "cpython"
            || metadata.version != "3.13.15"
            || metadata.distribution.is_empty()
            || metadata.distribution.len() > 256
        {
            return Err(Error::Unavailable("Python runtime manifest does not declare the supported CPython 3.13.15 protocol 1 distribution".into()));
        }
        let manifest = fs::canonicalize(path).map_err(missing)?;
        let directory = manifest.parent().expect("absolute manifest has a parent");
        fn resolve(directory: &Path, path: &Path, absolute: bool) -> Result<PathBuf, Error> {
            if path.as_os_str().is_empty()
                || (!absolute && path.is_absolute())
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                return Err(Error::Unavailable(
                    "Invalid packaged Python resource path".into(),
                ));
            }
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                directory.join(path)
            };
            let canonical = fs::canonicalize(&path).map_err(|error| Error::Unavailable(format!("Missing Python resource {}: {error}. Run `just python-prepare` or reinstall Maple.", path.display())))?;
            if !canonical.is_file() {
                return Err(Error::Unavailable(format!(
                    "Python resource is not a file: {}",
                    path.display()
                )));
            }
            Ok(canonical)
        }
        let executable = resolve(
            directory,
            &metadata.executable,
            metadata.distribution.starts_with("nix"),
        )?;
        let worker = resolve(directory, &metadata.worker, false)?;
        Ok(Self {
            manifest,
            executable,
            worker,
            implementation: metadata.implementation,
            version: metadata.version,
            distribution: metadata.distribution,
        })
    }

    /// Select exactly one known package layout. Never search PATH or task CWD.
    pub fn for_application_executable(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(Error::Unavailable(
                "Maple executable path must be absolute".into(),
            ));
        }
        let directory = path
            .parent()
            .ok_or_else(|| Error::Unavailable("Maple executable has no parent directory".into()))?;
        let manifest = if directory.file_name().is_some_and(|name| name == "MacOS")
            && directory
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "Contents"))
        {
            directory
                .parent()
                .unwrap()
                .join("Resources/python/runtime.json")
        } else if directory.file_name().is_some_and(|name| name == "bin")
            && directory.parent().is_some_and(|parent| {
                parent.starts_with("/nix/store") || parent.join("share/maple-gpui/python").is_dir()
            })
        {
            directory
                .parent()
                .unwrap()
                .join("share/maple-gpui/python/runtime.json")
        } else {
            directory.join("runtime/python/runtime.json")
        };
        Self::from_manifest(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_resources_never_fall_back_to_installed_python() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            PackagedPython::for_application_executable(dir.path().join("maple")).unwrap_err();
        assert!(error.to_string().contains("just python-prepare"));
    }
    #[test]
    fn rejects_unsupported_manifest_before_resources() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("runtime.json");
        fs::write(&manifest, r#"{"protocol_version":1,"implementation":"cpython","version":"3.14.0","distribution":"pbs","executable":"bin/python","worker":"worker.py"}"#).unwrap();
        assert!(
            PackagedPython::from_manifest(manifest)
                .unwrap_err()
                .to_string()
                .contains("3.13.15")
        );
    }
}
