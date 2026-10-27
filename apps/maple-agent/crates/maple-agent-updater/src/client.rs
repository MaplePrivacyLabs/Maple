use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use semver::Version;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::{
    manifest::{self, Artifact, Target, VerifiedRelease},
    state::{self, PreparedUpdate, StateStore},
};

pub struct Config {
    pub feed_url: Url,
    pub public_keys: Vec<String>,
    pub current_version: Version,
    pub target: Target,
    pub installed_path: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum CheckOutcome {
    Current,
    Available(Box<VerifiedRelease>),
    Withdrawn,
}

#[derive(Clone, Debug)]
pub enum Revalidation {
    Ready(VerifiedRelease),
    Superseded(VerifiedRelease),
    Withdrawn,
}

pub struct Updater {
    config: Config,
    client: reqwest::Client,
    state: StateStore,
    #[cfg(test)]
    allow_loopback_http: bool,
}

impl Updater {
    pub fn new(config: Config) -> Result<Self> {
        Self::construct(config, false)
    }

    fn construct(config: Config, _test_http: bool) -> Result<Self> {
        validate_url(&config.feed_url, _test_http)?;
        ensure!(
            !config.public_keys.is_empty() && config.public_keys.len() <= 4,
            "updater requires pinned signing keys"
        );
        for key in &config.public_keys {
            if key.starts_with("untrusted comment:") {
                minisign_verify::PublicKey::decode(key)?;
            } else {
                minisign_verify::PublicKey::from_base64(key)?;
            }
        }
        manifest::stable_version(&config.current_version.to_string())?;
        let state = StateStore::new(&config.state_dir, &config.installed_path)?;
        let client = reqwest::Client::builder()
            .user_agent("Maple-Agent-Updater/1")
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30 * 60))
            .build()?;
        Ok(Self {
            config,
            client,
            state,
            #[cfg(test)]
            allow_loopback_http: _test_http,
        })
    }

    pub fn installed_path(&self) -> &Path {
        self.state.installed_path()
    }

    fn validate_url(&self, url: &Url) -> Result<()> {
        #[cfg(test)]
        let test_http = self.allow_loopback_http;
        #[cfg(not(test))]
        let test_http = false;
        validate_url(url, test_http)?;
        ensure!(
            origin_allowed(&self.config.feed_url, url),
            "update redirect is outside the compiled origin policy"
        );
        Ok(())
    }

    async fn request(&self, url: Url, range_start: Option<u64>) -> Result<reqwest::Response> {
        let mut url = url;
        for redirects in 0..=4 {
            self.validate_url(&url)?;
            let mut request = self
                .client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity");
            if let Some(offset) = range_start {
                request = request.header(header::RANGE, format!("bytes={offset}-"));
            }
            let response = request.send().await.context("update request failed")?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirects < 4, "too many update redirects");
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .context("update redirect has no destination")?
                    .to_str()?;
                ensure!(location.len() <= 2048, "update redirect is too long");
                url = url.join(location).context("invalid update redirect")?;
                continue;
            }
            ensure!(
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .is_none_or(|value| value == "identity"),
                "compressed update HTTP response is not allowed"
            );
            return Ok(response);
        }
        bail!("too many update redirects")
    }

    async fn discovery(&self) -> Result<VerifiedRelease> {
        tokio::time::timeout(Duration::from_secs(60), async {
            let response = self.request(self.config.feed_url.clone(), None).await?;
            ensure!(
                response.status() == StatusCode::OK,
                "update metadata is unavailable"
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= manifest::MAX_ENVELOPE_BYTES as u64),
                "update envelope is too large"
            );
            let origin = response.url().clone();
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("cannot read update metadata")?;
                ensure!(
                    chunk.len() <= manifest::MAX_ENVELOPE_BYTES - bytes.len(),
                    "update envelope is too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            let release = manifest::verify_envelope(&bytes, &origin, &self.config.public_keys)?;
            release.artifact(self.config.target)?;
            Ok::<_, anyhow::Error>(release)
        })
        .await
        .context("update metadata request timed out")?
    }

    fn authenticate(&self, release: &VerifiedRelease) -> Result<VerifiedRelease> {
        self.validate_url(&release.origin)?;
        let verified = manifest::verify_envelope(
            release.envelope.as_bytes(),
            &release.origin,
            &self.config.public_keys,
        )?;
        ensure!(
            verified.manifest == release.manifest,
            "release fields differ from its signed envelope"
        );
        verified.artifact(self.config.target)?;
        Ok(verified)
    }

    fn eligible(&self, release: &VerifiedRelease) -> Result<bool> {
        // Schema and target are bound before rollback memory can advance or a
        // saved preparation can become eligible for filesystem effects.
        release.artifact(self.config.target)?;
        let version = release.version();
        Ok(version > self.config.current_version
            && self
                .state
                .highest_verified()?
                .as_ref()
                .is_none_or(|highest| &version >= highest))
    }

    pub async fn check(&self) -> Result<CheckOutcome> {
        let release = self.discovery().await?;
        if !self.eligible(&release)? {
            let saved = self.state.prepared()?;
            let had_preparation = saved.is_some();
            if let Some(saved) = saved {
                self.discard_prepared(Some(&saved.envelope)).await?;
            }
            return Ok(if had_preparation {
                CheckOutcome::Withdrawn
            } else {
                CheckOutcome::Current
            });
        }
        self.state.advance(&release.version())?;
        if let Some(saved) = self
            .state
            .prepared()?
            .filter(|prepared| prepared.envelope != release.envelope)
        {
            self.discard_prepared(Some(&saved.envelope)).await?;
        }
        Ok(CheckOutcome::Available(Box::new(release)))
    }

    pub async fn download(&self, release: &VerifiedRelease) -> Result<PreparedUpdate> {
        self.download_with_progress(release, |_, _| {}).await
    }

    pub async fn download_with_progress(
        &self,
        release: &VerifiedRelease,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<PreparedUpdate> {
        let release = self.authenticate(release)?;
        ensure!(self.eligible(&release)?, "update is no longer eligible");
        self.state.advance(&release.version())?;
        let artifact = release.artifact(self.config.target)?;
        let lock_path = self
            .state
            .root
            .join("payloads")
            .join(format!("{}.lock", artifact.sha256));
        let lock = state::open_private(&lock_path, false)?;
        FileExt::try_lock_exclusive(&lock).context("another process is downloading this update")?;
        let payload_path = self.state.payload_path(&artifact.sha256, false);
        if verify_payload(&payload_path, artifact).await.is_ok() {
            self.state.save_prepared(&release)?;
            return Ok(PreparedUpdate {
                release,
                payload_path,
                installed_path: self.state.installed_path().to_owned(),
            });
        }
        let partial_path = self.state.payload_path(&artifact.sha256, true);
        let file = state::open_private(&partial_path, false)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut offset = file.metadata().await?.len();
        if offset > artifact.size {
            file.set_len(0).await?;
            offset = 0;
        }
        if offset < artifact.size {
            let response = self
                .request(
                    release.artifact_url(self.config.target)?,
                    (offset > 0).then_some(offset),
                )
                .await?;
            if response.status() == StatusCode::PARTIAL_CONTENT {
                validate_range(
                    response
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .context("missing resumed content range")?
                        .to_str()?,
                    offset,
                    artifact.size,
                )?;
            } else if response.status() == StatusCode::OK {
                // A server ignoring Range is safe only when the partial bytes are
                // discarded; never append a complete object to a partial object.
                offset = 0;
                file.set_len(0).await?;
            } else {
                bail!("update payload is unavailable");
            }
            ensure!(
                response
                    .content_length()
                    .is_none_or(|len| len == artifact.size - offset),
                "update response length differs from signed size"
            );
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            progress(offset, artifact.size);
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        // Tokio may still have its last buffered write in
                        // flight. Finish it before a caller retries the range.
                        file.flush().await?;
                        file.sync_all().await?;
                        return Err(error).context("update download interrupted");
                    }
                };
                ensure!(
                    chunk.len() as u64 <= artifact.size - offset,
                    "update exceeds signed size"
                );
                file.write_all(&chunk).await?;
                offset += chunk.len() as u64;
                progress(offset, artifact.size);
            }
            file.sync_all().await?;
        }
        drop(file);
        if let Err(error) = verify_payload(&partial_path, artifact).await {
            // A truncated transfer remains resumable. Complete corrupt bytes
            // must be discarded, otherwise retries would repeatedly trust them.
            if offset == artifact.size {
                let _ = tokio::fs::remove_file(&partial_path).await;
            }
            return Err(error);
        }
        // Refuse a symlink destination rather than following it; rename itself
        // replaces a directory entry and cannot write through a link.
        if let Ok(metadata) = tokio::fs::symlink_metadata(&payload_path).await {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unsafe prepared payload destination"
            );
        }
        state::replace_file(&partial_path, &payload_path)?;
        state::sync_directory(&self.state.root.join("payloads"))?;
        self.state.save_prepared(&release)?;
        Ok(PreparedUpdate {
            release,
            payload_path,
            installed_path: self.state.installed_path().to_owned(),
        })
    }

    pub async fn restore_prepared(&self) -> Result<Option<PreparedUpdate>> {
        let Some(saved) = self.state.prepared()? else {
            return Ok(None);
        };
        let result: Result<Option<PreparedUpdate>> = async {
            let origin: Url = saved.origin.parse()?;
            self.validate_url(&origin)?;
            let release = manifest::verify_envelope(
                saved.envelope.as_bytes(),
                &origin,
                &self.config.public_keys,
            )?;
            if !self.eligible(&release)? {
                self.discard_prepared(Some(&saved.envelope)).await?;
                return Ok(None);
            }
            let artifact = release.artifact(self.config.target)?;
            let payload_path = self.state.payload_path(&artifact.sha256, false);
            verify_payload(&payload_path, artifact).await?;
            Ok(Some(PreparedUpdate {
                release,
                payload_path,
                installed_path: self.state.installed_path().to_owned(),
            }))
        }
        .await;
        match result {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                self.discard_prepared(Some(&saved.envelope)).await?;
                Err(error)
            }
        }
    }

    pub async fn revalidate(&self, prepared: &PreparedUpdate) -> Result<Revalidation> {
        let authenticated = self.authenticate(&prepared.release)?;
        ensure!(
            prepared.installed_path == self.state.installed_path(),
            "preparation belongs to another application"
        );
        let artifact = authenticated.artifact(self.config.target)?;
        ensure!(
            prepared.payload_path == self.state.payload_path(&artifact.sha256, false),
            "unexpected prepared payload path"
        );
        verify_payload(&prepared.payload_path, artifact).await?;
        // Do not clear a good preparation on network failure; users can retry.
        let fresh = self.discovery().await?;
        if fresh.envelope == authenticated.envelope && self.eligible(&fresh)? {
            self.state.advance(&fresh.version())?;
            return Ok(Revalidation::Ready(fresh));
        }
        self.discard_prepared(Some(&authenticated.envelope)).await?;
        if self.eligible(&fresh)? && fresh.version() > authenticated.version() {
            self.state.advance(&fresh.version())?;
            Ok(Revalidation::Superseded(fresh))
        } else {
            Ok(Revalidation::Withdrawn)
        }
    }

    pub async fn clear_prepared(&self) -> Result<()> {
        self.discard_prepared(None).await
    }

    /// Call synchronously under the application-install and host-startup fences
    /// immediately around final staged verification and replacement. No other
    /// updater state method may be called by `operation` while this lock is held.
    /// This closes the cross-process gap between fresh discovery and commit.
    pub fn with_commit_guard<T>(
        &self,
        prepared: &PreparedUpdate,
        operation: impl FnOnce() -> std::result::Result<T, String>,
    ) -> Result<T> {
        let authenticated = self.authenticate(&prepared.release)?;
        ensure!(
            authenticated.version() > self.config.current_version,
            "prepared update is not an upgrade"
        );
        ensure!(
            prepared.installed_path == self.state.installed_path(),
            "preparation belongs to another application"
        );
        ensure!(
            prepared.payload_path
                == self
                    .state
                    .payload_path(&authenticated.artifact(self.config.target)?.sha256, false),
            "unexpected prepared payload path"
        );
        self.state
            .with_commit_guard(&authenticated.version(), &authenticated.envelope, || {
                operation().map_err(anyhow::Error::msg)
            })
    }

    async fn discard_prepared(&self, expected_envelope: Option<&str>) -> Result<()> {
        let saved = self.state.take_prepared(expected_envelope)?;
        if let Some(saved) = saved {
            // Saved paths and ready flags never authorize filesystem changes.
            // Only derive a filename from an authenticated artifact digest.
            let Ok(release) = manifest::verify_envelope(
                saved.envelope.as_bytes(),
                &self.config.feed_url,
                &self.config.public_keys,
            ) else {
                return Ok(());
            };
            let Ok(artifact) = release.artifact(self.config.target) else {
                return Ok(());
            };
            let lock_path = self
                .state
                .root
                .join("payloads")
                .join(format!("{}.lock", artifact.sha256));
            let lock = state::open_private(&lock_path, false)?;
            if FileExt::try_lock_exclusive(&lock).is_err() {
                return Ok(());
            }
            if self.state.prepared()?.as_ref().is_some_and(|current| {
                manifest::verify_envelope(
                    current.envelope.as_bytes(),
                    &self.config.feed_url,
                    &self.config.public_keys,
                )
                .ok()
                .and_then(|release| {
                    release
                        .artifact(self.config.target)
                        .ok()
                        .map(|artifact| artifact.sha256.clone())
                })
                .is_some_and(|digest| digest == artifact.sha256)
            }) {
                return Ok(());
            }
            let path = self.state.payload_path(&artifact.sha256, false);
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn validate_url(url: &Url, _test_http: bool) -> Result<()> {
    #[cfg(test)]
    let secure = url.scheme() == "https"
        || (_test_http && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"));
    #[cfg(not(test))]
    let secure = url.scheme() == "https";
    ensure!(
        secure && url.host_str().is_some(),
        "update URL must use HTTPS"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "update URL cannot contain credentials, query or fragment"
    );
    ensure!(url.as_str().len() <= 2048, "update URL is too long");
    Ok(())
}

/// The one planned hostname migration is known at build time. Other callers
/// and test feeds remain confined to the exact configured origin, including
/// port. No wildcard Maple subdomain or arbitrary HTTPS redirect is allowed.
pub(crate) fn origin_allowed(feed: &Url, target: &Url) -> bool {
    if feed.origin() == target.origin() {
        return true;
    }
    let approved = |url: &Url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str(),
                Some("updates.trymaple.ai" | "updates.maple.ai")
            )
    };
    approved(feed) && approved(target)
}

fn validate_range(value: &str, offset: u64, total: u64) -> Result<()> {
    let expected = format!("bytes {offset}-{}/{total}", total - 1);
    ensure!(
        value == expected,
        "resumed update range differs from signed payload"
    );
    Ok(())
}

pub(crate) async fn verify_payload(path: &Path, artifact: &Artifact) -> Result<()> {
    let file = state::open_readonly(path).context("prepared payload is missing or unsafe")?;
    ensure!(
        file.metadata()?.file_type().is_file() && file.metadata()?.len() == artifact.size,
        "update payload has incorrect size"
    );
    let mut file = tokio::fs::File::from_std(file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        read += count as u64;
        ensure!(read <= artifact.size, "update payload exceeds signed size");
        digest.update(&buffer[..count]);
    }
    ensure!(
        read == artifact.size && hex::encode(digest.finalize()) == artifact.sha256,
        "update payload SHA-256 mismatch"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_updater(config: Config) -> Result<Updater> {
    Updater::construct(config, true)
}
