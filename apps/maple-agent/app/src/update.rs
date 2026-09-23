//! Discover stable Agent releases independently of other monorepo components.
//!
//! The desktop app checks once per launch and shows a release-page banner.
//! Nothing is downloaded or installed. Only `maple-agent-vMAJOR.MINOR.PATCH`
//! tags are eligible. `MAPLE_UPDATE_REPO` overrides the GitHub `owner/repo`,
//! and `MAPLE_DISABLE_UPDATE_CHECK=1` turns the check off.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use semver::Version;

const DEFAULT_REPO: &str = "MaplePrivacyLabs/Maple";
const AGENT_TAG_PREFIX: &str = "maple-agent-v";
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 10;
const MAX_PAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Agent version without its release-tag prefix.
    pub version: String,
    /// Canonical GitHub release page for the user to open.
    pub url: String,
}

static AVAILABLE: OnceLock<UpdateInfo> = OnceLock::new();

/// The newer release found by [`check`], if any.
pub fn available() -> Option<&'static UpdateInfo> {
    AVAILABLE.get()
}

pub fn enabled() -> bool {
    !crate::env::env_flag("MAPLE_DISABLE_UPDATE_CHECK")
}

/// Admit only GitHub owner/repository names, never an arbitrary URL or path.
fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.starts_with(|ch: char| ch.is_ascii_alphanumeric())
        && owner.ends_with(|ch: char| ch.is_ascii_alphanumeric())
        && owner
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        && !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
}

fn repo() -> Option<String> {
    let repo =
        crate::env::env_string("MAPLE_UPDATE_REPO").unwrap_or_else(|| DEFAULT_REPO.to_string());
    if valid_repo(&repo) {
        Some(repo)
    } else {
        log::debug!("update check: invalid repository override");
        None
    }
}

/// Ask GitHub for stable Agent releases. Runs on the backend runtime; the
/// HTTP client and the overall deadline must be created inside Tokio.
pub async fn check() -> Option<UpdateInfo> {
    if !enabled() {
        return None;
    }
    let repo = repo()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("maple-agent/", env!("CARGO_PKG_VERSION")))
        // A moved repository must be explicitly configured, not silently
        // followed to a different release source.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let version = tokio::time::timeout(
        Duration::from_secs(30),
        find_newer_release(&current, |page| fetch_page(&client, &repo, page)),
    )
    .await
    .ok()??;
    let info = release_info(&repo, version)?;
    let _ = AVAILABLE.set(info.clone());
    Some(info)
}

async fn fetch_page(
    client: &reqwest::Client,
    repo: &str,
    page: usize,
) -> Option<Vec<serde_json::Value>> {
    let url = format!(
        "https://api.github.com/repos/{repo}/releases?per_page={RELEASES_PER_PAGE}&page={page}"
    );
    let mut response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| log::debug!("update check: release request failed"))
        .ok()?;
    if !response.status().is_success() {
        log::debug!(
            "update check: release request returned {}",
            response.status()
        );
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_PAGE_BYTES.saturating_sub(bytes.len()) {
            log::debug!("update check: release page exceeds size limit");
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

/// GitHub's list order is not version order. Finish the bounded scan before
/// selecting a version; partial results must never produce an update banner.
async fn find_newer_release<F, Fut>(current: &Version, mut fetch: F) -> Option<Version>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Option<Vec<serde_json::Value>>>,
{
    let mut newest = None;
    for page in 1..=MAX_RELEASE_PAGES {
        let releases = fetch(page).await?;
        if releases.len() > RELEASES_PER_PAGE {
            return None;
        }
        for release in &releases {
            let Some(version) = stable_agent_version(release) else {
                continue;
            };
            if version.cmp_precedence(current).is_gt()
                && newest.as_ref().is_none_or(|best| &version > best)
            {
                newest = Some(version);
            }
        }
        if releases.len() < RELEASES_PER_PAGE {
            return newest;
        }
    }
    log::debug!("update check: release pagination limit reached");
    None
}

fn stable_agent_version(release: &serde_json::Value) -> Option<Version> {
    // Missing or malformed flags also fail closed.
    if release["draft"].as_bool()? || release["prerelease"].as_bool()? {
        return None;
    }
    let tag = release["tag_name"].as_str()?;
    let version = Version::parse(tag.strip_prefix(AGENT_TAG_PREFIX)?).ok()?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return None;
    }
    Some(version)
}

fn release_info(repo: &str, version: Version) -> Option<UpdateInfo> {
    if !valid_repo(repo) || !version.pre.is_empty() || !version.build.is_empty() {
        return None;
    }
    Some(UpdateInfo {
        url: format!("https://github.com/{repo}/releases/tag/{AGENT_TAG_PREFIX}{version}"),
        version: version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            // This untrusted link must have no influence on the banner.
            "html_url": "https://example.test/untrusted/release",
            "draft": false,
            "prerelease": false,
        })
    }

    async fn find(pages: Vec<Vec<serde_json::Value>>, current: &str) -> Option<Version> {
        find_newer_release(&Version::parse(current).unwrap(), |page| {
            std::future::ready(pages.get(page - 1).cloned())
        })
        .await
    }

    #[tokio::test]
    async fn agent_feed_ignores_research_and_selects_highest_semantic_version() {
        let newest = find(
            vec![vec![
                release("v99.0.0"),
                release("maple-agent-v1.9.0"),
                release("maple-agent-v2.0.0"),
                release("maple-agent-v1.10.0"),
                release("maple-agent-v1.1.0"),
            ]],
            "1.0.0",
        )
        .await
        .expect("newer Agent release");
        let info = release_info(DEFAULT_REPO, newest).unwrap();
        assert_eq!(info.version, "2.0.0");
        assert_eq!(
            info.url,
            "https://github.com/MaplePrivacyLabs/Maple/releases/tag/maple-agent-v2.0.0"
        );
        assert!(
            find(vec![vec![release("v99.0.0")]], "0.1.0")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn scans_later_pages_and_does_not_assume_release_date_order() {
        let mut first = vec![release("v99.0.0"); RELEASES_PER_PAGE];
        first[0] = release("maple-agent-v1.5.0");
        let newest = find(vec![first, vec![release("maple-agent-v1.10.0")]], "1.0.0")
            .await
            .unwrap();
        assert_eq!(newest.to_string(), "1.10.0");
    }

    #[tokio::test]
    async fn incomplete_or_failed_scan_does_not_advertise_partial_result() {
        let full = vec![release("maple-agent-v2.0.0"); RELEASES_PER_PAGE];
        assert!(find(vec![full.clone()], "1.0.0").await.is_none());
        let mut calls = 0;
        let newest = find_newer_release(&Version::parse("1.0.0").unwrap(), |_| {
            calls += 1;
            std::future::ready(Some(full.clone()))
        })
        .await;
        assert!(newest.is_none());
        assert_eq!(calls, MAX_RELEASE_PAGES);
        assert!(
            find(
                vec![vec![release("maple-agent-v2.0.0"); RELEASES_PER_PAGE + 1]],
                "1.0.0"
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn same_older_and_build_only_versions_are_ignored() {
        let releases = vec![release("maple-agent-v1.2.0"), release("maple-agent-v1.1.9")];
        assert!(find(vec![releases.clone()], "1.2.0").await.is_none());
        assert!(find(vec![releases], "1.2.0+local.5").await.is_none());
    }

    #[tokio::test]
    async fn stable_release_updates_a_prerelease_client() {
        let newest = find(vec![vec![release("maple-agent-v1.2.0")]], "1.2.0-beta.10")
            .await
            .unwrap();
        assert_eq!(newest.to_string(), "1.2.0");
    }

    #[test]
    fn drafts_prereleases_and_invalid_flags_are_ignored() {
        for (field, value) in [
            ("draft", serde_json::json!(true)),
            ("prerelease", serde_json::json!(true)),
            ("draft", serde_json::Value::Null),
            ("prerelease", serde_json::json!("false")),
        ] {
            let mut body = release("maple-agent-v9.0.0");
            body[field] = value;
            assert!(stable_agent_version(&body).is_none());
        }
    }

    #[test]
    fn malformed_and_nonstable_tags_are_ignored_even_when_marked_stable() {
        for tag in [
            "latest",
            "1.2.3",
            "v1.2.3",
            "agent-v1.2.3",
            "maple-agent-v1",
            "maple-agent-v1.2",
            "maple-agent-v1.2.3.4",
            "maple-agent-v01.2.3",
            "maple-agent-v1.2.3-beta.1",
            "maple-agent-v1.2.3+build.5",
            "maple-agent-v1.2.3 ",
            " maple-agent-v1.2.3",
            "maple-agent-vv1.2.3",
            "maple-agent-v1.2.3/../../../other",
            "maple-agent-v18446744073709551616.0.0",
        ] {
            assert!(
                stable_agent_version(&release(tag)).is_none(),
                "accepted {tag}"
            );
        }
        assert!(stable_agent_version(&serde_json::json!({})).is_none());
    }

    #[test]
    fn repository_override_cannot_escape_github_owner_repo() {
        for repo in [DEFAULT_REPO, "some-owner/agent.test_repo"] {
            assert!(valid_repo(repo));
            let info = release_info(repo, Version::parse("1.2.3").unwrap()).unwrap();
            assert_eq!(
                info.url,
                format!("https://github.com/{repo}/releases/tag/maple-agent-v1.2.3")
            );
        }
        for repo in [
            "",
            "MaplePrivacyLabs",
            "/Maple",
            "owner/",
            "owner/../elsewhere",
            "owner/..",
            "owner/.",
            "https://github.com/owner/repo",
            "owner/repo?x=1",
            "owner/repo#fragment",
            "owner/repo%2felsewhere",
            "owner/repo\\other",
            "owner@evil.example/repo",
            "owner/repo\n",
            "-owner/repo",
            "owner-/repo",
        ] {
            assert!(!valid_repo(repo), "accepted {repo}");
            assert!(release_info(repo, Version::parse("1.2.3").unwrap()).is_none());
        }
    }
}
