use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

pub const MAX_ENVELOPE_BYTES: usize = 128 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    DarwinAarch64,
    LinuxX86_64,
    WindowsX86_64,
    WindowsAarch64,
}

impl Target {
    pub fn key(self) -> &'static str {
        match self {
            Self::DarwinAarch64 => "darwin-aarch64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::WindowsX86_64 => "windows-x86_64",
            Self::WindowsAarch64 => "windows-aarch64",
        }
    }

    pub fn feed_path(self) -> &'static str {
        match self {
            Self::DarwinAarch64 => "/agent/stable/latest.json",
            Self::LinuxX86_64 => "/agent/stable/linux/latest.json",
            Self::WindowsX86_64 => "/agent/stable/windows/latest.json",
            Self::WindowsAarch64 => "/agent/stable/windows-arm64/latest.json",
        }
    }

    pub fn manifest_filename(self) -> &'static str {
        match self {
            Self::DarwinAarch64 => "manifest.json",
            Self::LinuxX86_64 => "linux-manifest.json",
            Self::WindowsX86_64 => "windows-manifest.json",
            Self::WindowsAarch64 => "windows-arm64-manifest.json",
        }
    }

    pub fn manifest_path(self, version: &str) -> String {
        format!(
            "/agent/stable/releases/{version}/{}",
            self.manifest_filename()
        )
    }

    fn manifest_format(self) -> &'static str {
        match self {
            Self::DarwinAarch64 => "maple-agent-update-v1",
            Self::LinuxX86_64 => "maple-agent-update-v2",
            Self::WindowsX86_64 => "maple-agent-update-v3",
            Self::WindowsAarch64 => "maple-agent-update-v4",
        }
    }

    pub fn for_platform(os: &str, architecture: &str) -> Option<Self> {
        match (os, architecture) {
            ("macos", "aarch64") => Some(Self::DarwinAarch64),
            ("linux", "x86_64") => Some(Self::LinuxX86_64),
            ("windows", "x86_64") => Some(Self::WindowsX86_64),
            ("windows", "aarch64") => Some(Self::WindowsAarch64),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Downloads {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macos: Option<Artifact>,
    #[serde(rename = "linux-appimage", skip_serializing_if = "Option::is_none")]
    pub linux_appimage: Option<Artifact>,
    #[serde(rename = "linux-bundle", skip_serializing_if = "Option::is_none")]
    pub linux_bundle: Option<Artifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows: Option<Artifact>,
    #[serde(rename = "windows-arm64", skip_serializing_if = "Option::is_none")]
    pub windows_arm64: Option<Artifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub product: String,
    pub channel: String,
    pub version: String,
    pub build: String,
    pub source_sha: String,
    pub pub_date: String,
    pub notes: String,
    pub artifacts: BTreeMap<String, Artifact>,
    pub downloads: Downloads,
}

/// An authenticated manifest and its exact envelope bytes. Public fields allow
/// presentation, but every authority-bearing operation authenticates them again.
#[derive(Clone, Debug)]
pub struct VerifiedRelease {
    pub manifest: Manifest,
    pub envelope: String,
    pub origin: Url,
}

impl VerifiedRelease {
    pub fn version(&self) -> Version {
        // Only constructed after schema validation; mutation is rechecked at use.
        Version::parse(&self.manifest.version).expect("validated release version")
    }

    pub fn artifact(&self, target: Target) -> Result<&Artifact> {
        ensure!(
            self.manifest.format == target.manifest_format(),
            "release format does not support this target"
        );
        self.manifest
            .artifacts
            .get(target.key())
            .context("release does not support this target")
    }

    pub fn artifact_url(&self, target: Target) -> Result<Url> {
        Ok(self.origin.join(&self.artifact(target)?.path)?)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    manifest: String,
    signature: String,
}

pub fn verify_envelope(bytes: &[u8], origin: &Url, keys: &[String]) -> Result<VerifiedRelease> {
    ensure!(
        bytes.len() <= MAX_ENVELOPE_BYTES,
        "update envelope is too large"
    );
    let envelope: Envelope = serde_json::from_slice(bytes).context("invalid update envelope")?;
    ensure!(
        envelope.manifest.len() <= MAX_MANIFEST_BYTES,
        "update manifest is too large"
    );
    ensure!(
        envelope.signature.len() <= 4096,
        "update signature is too large"
    );
    // Require exactly the four minisign lines. The parser otherwise ignores
    // additional lines, which makes malformed publisher output ambiguous.
    ensure!(
        envelope.signature.lines().count() == 4,
        "invalid signature text"
    );
    let signature = Signature::decode(&envelope.signature).context("invalid update signature")?;
    ensure!(
        keys.iter().any(|key| {
            let key = if key.starts_with("untrusted comment:") {
                PublicKey::decode(key)
            } else {
                PublicKey::from_base64(key)
            };
            key.is_ok_and(|key| {
                key.verify(envelope.manifest.as_bytes(), &signature, false)
                    .is_ok()
            })
        }),
        "update signature was not made by a pinned key"
    );
    // Signature and its trusted comment are authenticated before interpreting
    // product, paths, sizes, version or any other manifest field.
    let manifest: Manifest =
        serde_json::from_str(&envelope.manifest).context("invalid signed manifest")?;
    validate_manifest(&manifest)?;
    validate_download_fields(&envelope.manifest, &manifest.format)?;
    ensure!(
        signature.trusted_comment()
            == format!(
                "product=maple-agent channel=stable version={}",
                manifest.version
            ),
        "signed comment does not match update identity"
    );
    Ok(VerifiedRelease {
        manifest,
        envelope: std::str::from_utf8(bytes)?.to_owned(),
        origin: origin.origin().ascii_serialization().parse()?,
    })
}

pub fn stable_version(value: &str) -> Result<Version> {
    ensure!(value.len() <= 64, "version is too long");
    let version = Version::parse(value).context("invalid update version")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty() && version.to_string() == value,
        "update version must be canonical stable SemVer"
    );
    Ok(version)
}

fn lowercase_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_download_fields(text: &str, format: &str) -> Result<()> {
    // Optional fields cannot distinguish omission from an explicit null. Keep
    // legacy v1's exact field contract, and require platform-only inventories
    // even when a forbidden download field has a null value.
    let value: serde_json::Value = serde_json::from_str(text)?;
    let downloads = value["downloads"]
        .as_object()
        .context("invalid download inventory")?;
    match format {
        "maple-agent-update-v1" => ensure!(
            !downloads.contains_key("linux-bundle")
                && !downloads.contains_key("windows")
                && !downloads.contains_key("windows-arm64"),
            "platform downloads require their own update format"
        ),
        "maple-agent-update-v2" => ensure!(
            downloads.len() == 1 && downloads.contains_key("linux-bundle"),
            "update format v2 requires only the Linux bundle download"
        ),
        "maple-agent-update-v3" => ensure!(
            downloads.len() == 1 && downloads.contains_key("windows"),
            "update format v3 requires only the Windows installer download"
        ),
        "maple-agent-update-v4" => ensure!(
            downloads.len() == 1 && downloads.contains_key("windows-arm64"),
            "update format v4 requires only the Windows ARM64 installer download"
        ),
        _ => bail!("unsupported update format"),
    }
    Ok(())
}

pub fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(
        matches!(
            manifest.format.as_str(),
            "maple-agent-update-v1"
                | "maple-agent-update-v2"
                | "maple-agent-update-v3"
                | "maple-agent-update-v4"
        ),
        "unsupported update format"
    );
    ensure!(manifest.product == "maple-agent", "wrong update product");
    ensure!(manifest.channel == "stable", "wrong update channel");
    stable_version(&manifest.version)?;
    ensure!(
        !manifest.build.is_empty()
            && manifest.build.len() <= 18
            && !manifest.build.starts_with('0')
            && manifest.build.bytes().all(|b| b.is_ascii_digit()),
        "invalid update build number"
    );
    ensure!(
        lowercase_hex(&manifest.source_sha, 40),
        "invalid update source SHA"
    );
    let published =
        OffsetDateTime::parse(&manifest.pub_date, &Rfc3339).context("invalid publication time")?;
    ensure!(
        manifest.pub_date.ends_with('Z') && published.offset().is_utc(),
        "publication time must be UTC"
    );
    ensure!(
        manifest.notes.len() <= 16 * 1024
            && !manifest
                .notes
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "invalid release notes"
    );
    match manifest.format.as_str() {
        "maple-agent-update-v1" => validate_v1_inventory(manifest),
        "maple-agent-update-v2" => validate_v2_inventory(manifest),
        "maple-agent-update-v3" => validate_v3_inventory(manifest),
        "maple-agent-update-v4" => validate_v4_inventory(manifest),
        _ => unreachable!("update format was validated above"),
    }
}

fn validate_v1_inventory(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.downloads.linux_bundle.is_none()
            && manifest.downloads.windows.is_none()
            && manifest.downloads.windows_arm64.is_none(),
        "platform downloads require their own update format"
    );
    ensure!(
        manifest.artifacts.contains_key("darwin-aarch64"),
        "macOS artifact is required"
    );
    for (target, artifact) in &manifest.artifacts {
        let suffix = match target.as_str() {
            "darwin-aarch64" => ".app.tar.gz",
            "linux-x86_64" => ".AppImage",
            _ => bail!("unsupported update target"),
        };
        validate_artifact(artifact, &manifest.version, suffix)?;
    }
    validate_artifact(
        manifest
            .downloads
            .macos
            .as_ref()
            .context("macOS download is required")?,
        &manifest.version,
        ".dmg",
    )?;
    ensure!(
        manifest.artifacts.contains_key("linux-x86_64")
            == manifest.downloads.linux_appimage.is_some(),
        "Linux updater and download inventory differ"
    );
    if let Some(artifact) = &manifest.downloads.linux_appimage {
        validate_artifact(artifact, &manifest.version, ".AppImage")?;
        ensure!(
            Some(artifact) == manifest.artifacts.get("linux-x86_64"),
            "Linux download differs from updater payload"
        );
    }
    Ok(())
}

fn validate_v2_inventory(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.artifacts.len() == 1 && manifest.artifacts.contains_key("linux-x86_64"),
        "update format v2 requires only the Linux artifact"
    );
    ensure!(
        manifest.downloads.macos.is_none()
            && manifest.downloads.linux_appimage.is_none()
            && manifest.downloads.windows.is_none()
            && manifest.downloads.windows_arm64.is_none(),
        "update format v2 requires only the Linux bundle download"
    );
    let artifact = &manifest.artifacts["linux-x86_64"];
    validate_artifact(artifact, &manifest.version, ".tar.gz")?;
    ensure!(
        manifest.downloads.linux_bundle.as_ref() == Some(artifact),
        "Linux bundle download differs from updater payload"
    );
    Ok(())
}

fn validate_v3_inventory(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.artifacts.len() == 1 && manifest.artifacts.contains_key("windows-x86_64"),
        "update format v3 requires only the Windows artifact"
    );
    ensure!(
        manifest.downloads.macos.is_none()
            && manifest.downloads.linux_appimage.is_none()
            && manifest.downloads.linux_bundle.is_none()
            && manifest.downloads.windows_arm64.is_none(),
        "update format v3 requires only the Windows installer download"
    );
    validate_artifact(
        &manifest.artifacts["windows-x86_64"],
        &manifest.version,
        ".tar.gz",
    )?;
    validate_artifact(
        manifest
            .downloads
            .windows
            .as_ref()
            .context("Windows installer download is required")?,
        &manifest.version,
        ".exe",
    )?;
    Ok(())
}

fn validate_v4_inventory(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.artifacts.len() == 1 && manifest.artifacts.contains_key("windows-aarch64"),
        "update format v4 requires only the Windows ARM64 artifact"
    );
    ensure!(
        manifest.downloads.macos.is_none()
            && manifest.downloads.linux_appimage.is_none()
            && manifest.downloads.linux_bundle.is_none()
            && manifest.downloads.windows.is_none(),
        "update format v4 requires only the Windows ARM64 installer download"
    );
    validate_artifact(
        &manifest.artifacts["windows-aarch64"],
        &manifest.version,
        ".tar.gz",
    )?;
    validate_artifact(
        manifest
            .downloads
            .windows_arm64
            .as_ref()
            .context("Windows ARM64 installer download is required")?,
        &manifest.version,
        ".exe",
    )?;
    Ok(())
}

fn validate_artifact(artifact: &Artifact, version: &str, suffix: &str) -> Result<()> {
    ensure!(
        artifact.size > 0 && artifact.size <= MAX_ARTIFACT_BYTES,
        "invalid artifact size"
    );
    ensure!(
        lowercase_hex(&artifact.sha256, 64),
        "invalid artifact SHA-256"
    );
    let prefix = format!("/agent/stable/releases/{version}/");
    let filename = artifact
        .path
        .strip_prefix(&prefix)
        .context("artifact path has wrong release prefix")?;
    ensure!(
        !filename.is_empty()
            && filename.len() <= 200
            && filename.ends_with(suffix)
            && !filename.starts_with('.')
            && filename
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid artifact filename"
    );
    Ok(())
}

#[cfg(test)]
mod windows_tests {
    use super::*;

    fn fixture() -> VerifiedRelease {
        verify_envelope(
            include_bytes!("../tests/fixtures/signed-windows-v3-envelope.json"),
            &"https://updates.example.test".parse().unwrap(),
            &[include_str!("../tests/fixtures/public-test-key.pub").to_owned()],
        )
        .unwrap()
    }

    #[test]
    fn signed_windows_v3_fixture_keeps_its_exact_bytes_and_platform() {
        let release = fixture();
        assert_eq!(
            serde_json::to_vec(&release.manifest).unwrap(),
            include_bytes!("../tests/fixtures/windows-v3-manifest.json")
        );
        assert!(release.artifact(Target::WindowsX86_64).is_ok());
        assert!(release.artifact(Target::DarwinAarch64).is_err());
        assert!(release.artifact(Target::LinuxX86_64).is_err());
        assert_ne!(
            release.artifact(Target::WindowsX86_64).unwrap().path,
            release.manifest.downloads.windows.as_ref().unwrap().path
        );
    }

    #[test]
    fn windows_v3_requires_exact_independent_bundle_and_installer_inventory() {
        let mutations: Vec<fn(&mut Manifest)> = vec![
            |m| m.format = "maple-agent-update-v2".into(),
            |m| m.product = "maple-research".into(),
            |m| m.channel = "dev".into(),
            |m| m.artifacts.clear(),
            |m| {
                m.artifacts
                    .insert("linux-x86_64".into(), m.artifacts["windows-x86_64"].clone());
            },
            |m| m.downloads.windows = None,
            |m| m.downloads.macos = m.downloads.windows.clone(),
            |m| m.downloads.linux_appimage = m.downloads.windows.clone(),
            |m| m.downloads.linux_bundle = m.downloads.windows.clone(),
            |m| m.artifacts.get_mut("windows-x86_64").unwrap().size = 0,
            |m| {
                m.artifacts.get_mut("windows-x86_64").unwrap().size = MAX_ARTIFACT_BYTES + 1;
            },
            |m| m.artifacts.get_mut("windows-x86_64").unwrap().sha256 = "A".repeat(64),
            |m| m.downloads.windows.as_mut().unwrap().sha256 = "bad".into(),
            |m| m.downloads.windows.as_mut().unwrap().size = 0,
            |m| {
                m.downloads.windows.as_mut().unwrap().path =
                    "/agent/stable/releases/0.0.1/Maple-Agent-setup.exe".into();
            },
            |m| {
                m.downloads.windows.as_mut().unwrap().path =
                    "/agent/stable/releases/0.0.2/Maple-Agent.tar.gz".into();
            },
            |m| {
                m.artifacts.get_mut("windows-x86_64").unwrap().path =
                    "/agent/stable/releases/0.0.2/Maple-Agent-setup.exe".into();
            },
        ];
        for mutation in mutations {
            let mut manifest = fixture().manifest;
            mutation(&mut manifest);
            assert!(
                validate_manifest(&manifest).is_err(),
                "accepted {manifest:?}"
            );
        }
    }

    #[test]
    fn windows_arm64_v4_requires_its_own_complete_bounded_inventory() {
        let release = verify_envelope(
            include_bytes!("../tests/fixtures/signed-windows-arm64-v4-envelope.json"),
            &"https://updates.example.test".parse().unwrap(),
            &[include_str!("../tests/fixtures/public-test-key.pub").to_owned()],
        )
        .unwrap();
        let mutations: Vec<fn(&mut Manifest)> = vec![
            |m| m.format = "maple-agent-update-v3".into(),
            |m| m.artifacts.clear(),
            |m| {
                let artifact = m.artifacts.remove("windows-aarch64").unwrap();
                m.artifacts.insert("windows-x86_64".into(), artifact);
            },
            |m| {
                m.artifacts.insert(
                    "windows-x86_64".into(),
                    m.artifacts["windows-aarch64"].clone(),
                );
            },
            |m| m.downloads.windows_arm64 = None,
            |m| m.downloads.windows = m.downloads.windows_arm64.clone(),
            |m| m.downloads.macos = m.downloads.windows_arm64.clone(),
            |m| m.downloads.linux_bundle = m.downloads.windows_arm64.clone(),
            |m| m.downloads.linux_appimage = m.downloads.windows_arm64.clone(),
            |m| m.artifacts.get_mut("windows-aarch64").unwrap().size = 0,
            |m| m.artifacts.get_mut("windows-aarch64").unwrap().size = MAX_ARTIFACT_BYTES + 1,
            |m| m.artifacts.get_mut("windows-aarch64").unwrap().sha256 = "A".repeat(64),
            |m| m.downloads.windows_arm64.as_mut().unwrap().size = 0,
            |m| m.downloads.windows_arm64.as_mut().unwrap().sha256 = "bad".into(),
            |m| {
                m.downloads.windows_arm64.as_mut().unwrap().path =
                    "/agent/stable/releases/0.0.1/wrong.exe".into()
            },
            |m| {
                m.downloads.windows_arm64.as_mut().unwrap().path =
                    "/agent/stable/releases/0.0.2/wrong.tar.gz".into()
            },
            |m| {
                m.artifacts.get_mut("windows-aarch64").unwrap().path =
                    "/agent/stable/releases/0.0.2/wrong.exe".into()
            },
        ];
        for mutate in mutations {
            let mut manifest = release.manifest.clone();
            mutate(&mut manifest);
            assert!(
                validate_manifest(&manifest).is_err(),
                "accepted {manifest:?}"
            );
        }
        let mut legacy = fixture().manifest;
        legacy.downloads.windows_arm64 = release.manifest.downloads.windows_arm64;
        assert!(validate_manifest(&legacy).is_err());
    }

    #[test]
    fn platform_download_fields_are_rejected_even_when_explicitly_null() {
        let windows = serde_json::to_string(&fixture().manifest).unwrap();
        for field in ["macos", "linux-appimage", "linux-bundle", "unknown"] {
            let mut value: serde_json::Value = serde_json::from_str(&windows).unwrap();
            value["downloads"][field] = serde_json::Value::Null;
            assert!(validate_download_fields(&value.to_string(), "maple-agent-update-v3").is_err());
        }
        for (bytes, format) in [
            (
                include_str!("../tests/fixtures/manifest.json"),
                "maple-agent-update-v1",
            ),
            (
                include_str!("../tests/fixtures/linux-v2-manifest.json"),
                "maple-agent-update-v2",
            ),
        ] {
            let mut value: serde_json::Value = serde_json::from_str(bytes).unwrap();
            value["downloads"]["windows"] = serde_json::Value::Null;
            assert!(validate_download_fields(&value.to_string(), format).is_err());
            value["downloads"]["windows"] =
                serde_json::to_value(fixture().manifest.downloads.windows.unwrap()).unwrap();
            assert!(validate_download_fields(&value.to_string(), format).is_err());
        }
    }
}
