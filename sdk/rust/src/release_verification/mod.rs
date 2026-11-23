mod bundle;
mod embedded;
mod trust;

use crate::attestation::AttestationDocument;
use crate::pcr::Pcr0Environment;
use bundle::{
    verify_release_bundle, MeasurementTuple, ReleasePolicy, ReleaseVerificationEvidence,
    VerificationFailure,
};
use futures::StreamExt;
use reqwest::{Client, StatusCode, Url};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::DerPublicKey;
use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

const MAX_BUNDLE_BYTES: usize = 1024 * 1024;
const DEFAULT_BUNDLE_FETCH_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const DEFAULT_ROOT_REFRESH_TIMEOUT: Duration = Duration::from_secs(4);
const DEFAULT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseVerificationOutcome {
    Verified,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseBundleSource {
    Embedded,
    Cache,
    Remote,
    Unavailable,
}

/// Signed source, build and transparency-log data from a verified release.
/// These values are diagnostics, never authorization policy inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseVerificationIdentity {
    pub repository: String,
    pub commit: String,
    pub build_run_id: String,
    pub build_run_attempt: u64,
    pub log_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseVerificationResult {
    pub outcome: ReleaseVerificationOutcome,
    pub category: &'static str,
    pub source: ReleaseBundleSource,
    pub tlog_verified: bool,
    pub refresh_error: Option<&'static str>,
    pub identity: Option<ReleaseVerificationIdentity>,
}

impl ReleaseVerificationResult {
    fn verified(source: ReleaseBundleSource, evidence: ReleaseVerificationEvidence) -> Self {
        let identity = ReleaseVerificationIdentity {
            repository: evidence.repository,
            commit: evidence.commit,
            build_run_id: evidence.build_run_id,
            build_run_attempt: evidence.build_run_attempt,
            log_index: evidence.log_index,
        };
        Self {
            outcome: ReleaseVerificationOutcome::Verified,
            category: "verified",
            source,
            tlog_verified: true,
            refresh_error: None,
            identity: Some(identity),
        }
    }

    fn rejected(category: &'static str, source: ReleaseBundleSource) -> Self {
        Self {
            outcome: ReleaseVerificationOutcome::Rejected,
            category,
            source,
            tlog_verified: false,
            refresh_error: None,
            identity: None,
        }
    }

    fn clone_for_cache(&self) -> Self {
        Self {
            source: ReleaseBundleSource::Cache,
            ..self.clone()
        }
    }

    #[cfg(test)]
    pub(crate) fn tlog_verified(&self) -> bool {
        self.tlog_verified
    }
}

#[derive(Clone, Copy)]
struct ObservationTimeouts {
    bundle_fetch: Duration,
    root_refresh: Duration,
    overall: Duration,
}

impl Default for ObservationTimeouts {
    fn default() -> Self {
        Self {
            bundle_fetch: DEFAULT_BUNDLE_FETCH_TIMEOUT,
            root_refresh: DEFAULT_ROOT_REFRESH_TIMEOUT,
            overall: DEFAULT_OBSERVATION_TIMEOUT,
        }
    }
}

/// Dropping the observation future prevents queued cryptographic work from
/// starting. Work that already started may finish; its result is discarded
/// with the cancelled future rather than becoming a later observation result.
struct CancelObservation(Arc<AtomicBool>);

impl Drop for CancelObservation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) struct ReleaseVerifier {
    environment: Pcr0Environment,
    approval_key: Option<DerPublicKey>,
    bundle_host: Url,
    http: Client,
    trusted_root: RwLock<TrustedRoot>,
    root_generation: AtomicU64,
    root_refresher: Box<dyn trust::RootRefresher>,
    refresh_lock: AsyncMutex<()>,
    verified_cache: Mutex<HashMap<MeasurementTuple, ReleaseVerificationResult>>,
    timeouts: ObservationTimeouts,
    #[cfg(test)]
    use_embedded_bundle: bool,
    #[cfg(test)]
    worker_start_counter: Option<Arc<AtomicUsize>>,
}

impl ReleaseVerifier {
    pub(crate) fn new(environment: Pcr0Environment) -> Result<Self, String> {
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot construct release verification HTTP client".to_owned())?;
        Ok(Self {
            environment,
            approval_key: trust::approval_key(environment)?,
            bundle_host: trust::bundle_host(environment),
            http,
            trusted_root: RwLock::new(trust::embedded_trusted_root()?),
            root_generation: AtomicU64::new(0),
            root_refresher: Box::new(trust::SigstoreTufRefresher),
            refresh_lock: AsyncMutex::new(()),
            verified_cache: Mutex::new(HashMap::new()),
            timeouts: ObservationTimeouts::default(),
            #[cfg(test)]
            use_embedded_bundle: true,
            #[cfg(test)]
            worker_start_counter: None,
        })
    }

    pub(crate) async fn observe(
        &self,
        document: &AttestationDocument,
    ) -> ReleaseVerificationResult {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelObservation(Arc::clone(&cancelled));
        match tokio::time::timeout(
            self.timeouts.overall,
            self.observe_inner(document, cancelled),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => ReleaseVerificationResult::rejected(
                "observation-timeout",
                ReleaseBundleSource::Unavailable,
            ),
        }
    }

    async fn observe_inner(
        &self,
        document: &AttestationDocument,
        cancelled: Arc<AtomicBool>,
    ) -> ReleaseVerificationResult {
        let Some(approval_key) = self.approval_key.clone() else {
            return ReleaseVerificationResult::rejected(
                "approval-key-not-configured",
                ReleaseBundleSource::Unavailable,
            );
        };
        let Some(measurements) = measurement_tuple(document) else {
            return ReleaseVerificationResult::rejected(
                "statement-measurement-mismatch",
                ReleaseBundleSource::Unavailable,
            );
        };
        let cached_result = self.cached_result(&measurements);
        if let Some(result) = cached_result {
            return result.clone_for_cache();
        }

        let candidate = self.load_bundle_candidate(&measurements).await;
        let (bytes, source) = match candidate {
            Ok(candidate) => candidate,
            Err(result) => return result,
        };
        let result = self
            .verify_candidate(bytes, source, &approval_key, &measurements, &cancelled)
            .await;
        let is_verified = result.outcome == ReleaseVerificationOutcome::Verified;
        if is_verified {
            self.cache_verified(measurements, result.clone());
        }
        result
    }

    fn cached_result(&self, measurements: &MeasurementTuple) -> Option<ReleaseVerificationResult> {
        let cache = self.verified_cache.lock().ok()?;
        let result = cache.get(measurements);
        result.cloned()
    }

    fn cache_verified(&self, measurements: MeasurementTuple, result: ReleaseVerificationResult) {
        let has_verified_tlog = result.tlog_verified;
        if !has_verified_tlog {
            return;
        }
        if let Ok(mut cache) = self.verified_cache.lock() {
            cache.insert(measurements, result);
        }
    }

    fn embedded_bundle(&self, measurements: &MeasurementTuple) -> Option<&'static [u8]> {
        #[cfg(test)]
        let use_embedded_bundle = self.use_embedded_bundle;
        #[cfg(test)]
        if !use_embedded_bundle {
            return None;
        }
        let is_development = self.environment == Pcr0Environment::Development;
        if !is_development {
            return None;
        }
        embedded::embedded_development_bundle(&measurements.pcr0_hex())
    }

    async fn load_bundle_candidate(
        &self,
        measurements: &MeasurementTuple,
    ) -> Result<(Vec<u8>, ReleaseBundleSource), ReleaseVerificationResult> {
        if let Some(bytes) = self.embedded_bundle(measurements) {
            return Ok((bytes.to_vec(), ReleaseBundleSource::Embedded));
        }
        let bytes = self.fetch_bundle(measurements).await.map_err(|error| {
            ReleaseVerificationResult::rejected(error.code(), ReleaseBundleSource::Remote)
        })?;
        Ok((bytes, ReleaseBundleSource::Remote))
    }

    async fn fetch_bundle(
        &self,
        measurements: &MeasurementTuple,
    ) -> Result<Vec<u8>, VerificationFailure> {
        let path = format!("{}.json", measurements.pcr0_hex());
        let url = self
            .bundle_host
            .join(&path)
            .map_err(|_| VerificationFailure::fetch("bundle-fetch-failed"))?;
        let response = self
            .http
            .get(url)
            .timeout(self.timeouts.bundle_fetch)
            .send()
            .await
            .map_err(|error| VerificationFailure::fetch(fetch_error_code(&error)))?;
        let is_not_found = response.status() == StatusCode::NOT_FOUND;
        if is_not_found {
            return Err(VerificationFailure::fetch("bundle-not-found"));
        }
        let is_success = response.status().is_success();
        if !is_success {
            return Err(VerificationFailure::fetch("bundle-fetch-failed"));
        }
        let declared_body_exceeds_limit = response
            .content_length()
            .is_some_and(|length| length > MAX_BUNDLE_BYTES as u64);
        if declared_body_exceeds_limit {
            return Err(VerificationFailure::fetch("bundle-too-large"));
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| VerificationFailure::fetch(fetch_error_code(&error)))?;
            let streamed_body_exceeds_limit =
                bytes.len().saturating_add(chunk.len()) > MAX_BUNDLE_BYTES;
            if streamed_body_exceeds_limit {
                return Err(VerificationFailure::fetch("bundle-too-large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn verify_candidate(
        &self,
        bytes: Vec<u8>,
        source: ReleaseBundleSource,
        approval_key: &DerPublicKey,
        measurements: &MeasurementTuple,
        cancelled: &Arc<AtomicBool>,
    ) -> ReleaseVerificationResult {
        let generation = self.root_generation.load(Ordering::Acquire);
        let first_result = self
            .verify_once(
                bytes.clone(),
                approval_key.clone(),
                *measurements,
                cancelled,
            )
            .await;
        let first_error = match first_result {
            Ok(evidence) => return ReleaseVerificationResult::verified(source, evidence),
            Err(error) => error,
        };
        let is_refreshable = first_error.refreshable();
        if !is_refreshable {
            return ReleaseVerificationResult::rejected(first_error.code(), source);
        }

        let _refresh_guard = self.refresh_lock.lock().await;
        let needs_root_refresh = self.root_generation.load(Ordering::Acquire) == generation;
        if needs_root_refresh {
            let refresh_failed = || {
                let mut result = ReleaseVerificationResult::rejected(first_error.code(), source);
                result.refresh_error = Some("trust-refresh-failed");
                result
            };
            let refreshed_root = match tokio::time::timeout(
                self.timeouts.root_refresh,
                self.root_refresher.refresh(),
            )
            .await
            {
                Ok(Ok(refreshed_root)) => refreshed_root,
                Ok(Err(())) | Err(_) => return refresh_failed(),
            };
            let mut root = match self.trusted_root.write() {
                Ok(root) => root,
                Err(_) => return refresh_failed(),
            };
            *root = refreshed_root;
            self.root_generation.fetch_add(1, Ordering::AcqRel);
        }

        // At most one retry, either with this task's refreshed root or with
        // the root another concurrent verifier just refreshed.
        match self
            .verify_once(bytes, approval_key.clone(), *measurements, cancelled)
            .await
        {
            Ok(evidence) => ReleaseVerificationResult::verified(source, evidence),
            Err(error) => ReleaseVerificationResult::rejected(error.code(), source),
        }
    }

    async fn verify_once(
        &self,
        bytes: Vec<u8>,
        approval_key: DerPublicKey,
        measurements: MeasurementTuple,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<ReleaseVerificationEvidence, VerificationFailure> {
        let trusted_root = {
            let trusted_root = self
                .trusted_root
                .read()
                .map_err(|_| VerificationFailure::policy("verifier-failed"))?;
            trusted_root.clone()
        };
        let environment = self.environment;
        let cancelled = Arc::clone(cancelled);
        #[cfg(test)]
        let worker_start_counter = self.worker_start_counter.clone();
        tokio::task::spawn_blocking(move || {
            // Do not begin queued cryptographic work after cancellation. If this
            // check passes, already-running work may finish without interruption.
            let is_cancelled = cancelled.load(Ordering::Acquire);
            if is_cancelled {
                return Err(VerificationFailure::policy("verifier-failed"));
            }
            #[cfg(test)]
            if let Some(counter) = worker_start_counter {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            verify_at(
                environment,
                &bytes,
                &approval_key,
                &trusted_root,
                measurements,
            )
            .inspect(|evidence| {
                debug_assert!(evidence.tlog_verified);
            })
        })
        .await
        .map_err(|_| VerificationFailure::policy("verifier-failed"))?
    }

    #[cfg(test)]
    fn for_test(
        environment: Pcr0Environment,
        approval_key: Option<DerPublicKey>,
        bundle_host: Url,
        trusted_root: TrustedRoot,
        root_refresher: Box<dyn trust::RootRefresher>,
    ) -> Self {
        let mut verifier =
            Self::new(Pcr0Environment::Development).expect("embedded test dependencies are valid");
        verifier.environment = environment;
        verifier.approval_key = approval_key;
        verifier.bundle_host = bundle_host;
        verifier.trusted_root = RwLock::new(trusted_root);
        verifier.root_refresher = root_refresher;
        verifier
    }

    #[cfg(test)]
    fn verify_bundle(
        &self,
        bytes: &[u8],
        measurements: MeasurementTuple,
    ) -> Result<ReleaseVerificationEvidence, VerificationFailure> {
        let approval_key = self
            .approval_key
            .as_ref()
            .ok_or_else(|| VerificationFailure::policy("approval-key-not-configured"))?;
        let trusted_root = self
            .trusted_root
            .read()
            .map_err(|_| VerificationFailure::policy("verifier-failed"))?;
        verify_at(
            self.environment,
            bytes,
            approval_key,
            &trusted_root,
            measurements,
        )
    }

    #[cfg(test)]
    pub(crate) fn fetching_for_test(bundle_host: Url) -> Self {
        let mut verifier =
            Self::new(Pcr0Environment::Development).expect("embedded test dependencies are valid");
        verifier.bundle_host = bundle_host;
        verifier.use_embedded_bundle = false;
        verifier
    }

    #[cfg(test)]
    fn with_timeouts(mut self, timeouts: ObservationTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    #[cfg(test)]
    fn without_embedded_bundle(mut self) -> Self {
        self.use_embedded_bundle = false;
        self
    }

    #[cfg(test)]
    fn with_worker_start_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.worker_start_counter = Some(counter);
        self
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.verified_cache.lock().map_or(0, |cache| cache.len())
    }
}

fn verify_at(
    environment: Pcr0Environment,
    bytes: &[u8],
    approval_key: &DerPublicKey,
    trusted_root: &TrustedRoot,
    measurements: MeasurementTuple,
) -> Result<ReleaseVerificationEvidence, VerificationFailure> {
    verify_release_bundle(
        bytes,
        approval_key,
        trusted_root,
        ReleasePolicy::new(environment, measurements),
    )
}

fn fetch_error_code(error: &reqwest::Error) -> &'static str {
    let is_timeout = error.is_timeout();
    if is_timeout {
        "bundle-timeout"
    } else {
        "bundle-fetch-failed"
    }
}

impl VerificationFailure {
    fn fetch(code: &'static str) -> Self {
        Self::policy(code)
    }
}

fn measurement_tuple(document: &AttestationDocument) -> Option<MeasurementTuple> {
    Some(MeasurementTuple {
        pcr0: authenticated_pcr(document, 0)?,
        pcr1: authenticated_pcr(document, 1)?,
        pcr2: authenticated_pcr(document, 2)?,
    })
}

fn authenticated_pcr(document: &AttestationDocument, index: usize) -> Option<[u8; 48]> {
    document.pcrs.get(&index)?.as_slice().try_into().ok()
}

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod tests;
