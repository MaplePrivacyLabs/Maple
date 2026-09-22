//! The rows of the Hosts pane: each saved host with its connection state
//! and the version it announced, compared with this app's own. Rows are
//! computed when the list or a host's state changes, never in render.

use gpui::SharedString;
use maple_remote::hosts::SavedHost;
use maple_remote::manager::HostVersion;
use semver::Version;

/// How a host's build relates to this app's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The same version and, where both are known, the same build.
    Same,
    /// The same version built from another revision.
    DifferentBuild,
    /// The host's version is lower.
    Behind,
    /// The host's version is higher.
    Newer,
    /// One of the versions is not semver; nothing can be said.
    Unknown,
}

pub fn relation(host: &HostVersion, app: &HostVersion) -> Relation {
    let (Ok(host_version), Ok(app_version)) =
        (Version::parse(&host.version), Version::parse(&app.version))
    else {
        return Relation::Unknown;
    };
    match host_version.cmp(&app_version) {
        std::cmp::Ordering::Less => Relation::Behind,
        std::cmp::Ordering::Greater => Relation::Newer,
        std::cmp::Ordering::Equal => match (&host.build, &app.build) {
            (Some(host_build), Some(app_build)) if host_build != app_build => {
                Relation::DifferentBuild
            }
            _ => Relation::Same,
        },
    }
}

/// The lines shown under a host's version: how it relates to this app,
/// and the newest release the update check found when that is newer than
/// the host. `latest` is that release's version, if any.
pub fn notes(host: &HostVersion, app: &HostVersion, latest: Option<&str>) -> Vec<String> {
    let mut notes = Vec::new();
    match relation(host, app) {
        Relation::Behind => notes.push("Behind this app; update the host".to_string()),
        Relation::Newer => notes.push("Newer than this app; update this app".to_string()),
        Relation::DifferentBuild => notes.push("Different build from this app".to_string()),
        Relation::Same | Relation::Unknown => {}
    }
    if let Some(latest) = latest
        && let (Ok(latest_version), Ok(host_version)) =
            (Version::parse(latest), Version::parse(&host.version))
        && latest_version > host_version
    {
        notes.push(format!("Update available: {latest}"));
    }
    notes
}

/// One saved host as the pane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub id: String,
    pub name: SharedString,
    pub online: bool,
    /// `0.1.0 (63bcff5c)` while online, `last seen 0.1.0 (63bcff5c)`
    /// while offline; absent when the host never announced a version.
    pub version: Option<SharedString>,
    pub notes: Vec<SharedString>,
    /// The addresses and the start of the key.
    pub detail: SharedString,
}

/// The rows for `saved`. `live` answers what a host's current connection
/// announced, or `None` while it is offline; an offline host shows what it
/// announced last.
pub fn rows(
    saved: &[SavedHost],
    app: &HostVersion,
    latest: Option<&str>,
    live: impl Fn(&str) -> Option<HostVersion>,
) -> Vec<HostRow> {
    saved
        .iter()
        .map(|host| {
            let connected = live(&host.id);
            let online = connected.is_some();
            let announced = connected.or_else(|| {
                host.last_seen_version.clone().map(|version| HostVersion {
                    version,
                    build: host.last_seen_build.clone(),
                })
            });
            let version = announced.as_ref().map(|announced| {
                if online {
                    announced.label()
                } else {
                    format!("last seen {}", announced.label())
                }
            });
            let notes = announced
                .as_ref()
                .map(|announced| notes(announced, app, latest))
                .unwrap_or_default();
            let short_id: String = host.id.chars().take(10).collect();
            let connections = host
                .connections
                .iter()
                .map(|connection| connection.label().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            HostRow {
                id: host.id.clone(),
                name: host.name.clone().into(),
                online,
                version: version.map(Into::into),
                notes: notes.into_iter().map(Into::into).collect(),
                detail: format!("{connections} \u{b7} key {short_id}\u{2026}").into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_remote::hosts::HostConnection;

    fn version(version: &str, build: Option<&str>) -> HostVersion {
        HostVersion {
            version: version.to_string(),
            build: build.map(str::to_string),
        }
    }

    #[test]
    fn a_host_is_compared_by_version_then_by_build() {
        let app = version("0.2.0", Some("63bcff5c"));
        assert_eq!(
            relation(&version("0.1.9", Some("63bcff5c")), &app),
            Relation::Behind
        );
        assert_eq!(
            relation(&version("0.2.0", Some("63bcff5c")), &app),
            Relation::Same
        );
        assert_eq!(
            relation(&version("0.2.0", Some("abc1234")), &app),
            Relation::DifferentBuild
        );
        assert_eq!(
            relation(&version("0.2.1", Some("abc1234")), &app),
            Relation::Newer
        );
        // A side without a build cannot be told apart by it.
        assert_eq!(relation(&version("0.2.0", None), &app), Relation::Same);
        assert_eq!(
            relation(&version("0.2.0", Some("abc1234")), &version("0.2.0", None)),
            Relation::Same
        );
        assert_eq!(relation(&version("v0.2.0", None), &app), Relation::Unknown);
    }

    #[test]
    fn notes_name_the_relation_and_an_available_update() {
        let app = version("0.2.0", Some("63bcff5c"));
        assert_eq!(
            notes(&version("0.1.9", Some("63bcff5c")), &app, None),
            vec!["Behind this app; update the host"]
        );
        assert!(notes(&version("0.2.0", Some("63bcff5c")), &app, None).is_empty());
        assert_eq!(
            notes(&version("0.2.0", Some("abc1234")), &app, None),
            vec!["Different build from this app"]
        );
        assert_eq!(
            notes(&version("0.3.0", None), &app, None),
            vec!["Newer than this app; update this app"]
        );
        // The update note joins the relation and stands alone when the
        // host matches this app but a newer release exists.
        assert_eq!(
            notes(&version("0.1.9", None), &app, Some("0.3.0")),
            vec![
                "Behind this app; update the host",
                "Update available: 0.3.0"
            ]
        );
        assert_eq!(
            notes(&version("0.2.0", Some("63bcff5c")), &app, Some("0.3.0")),
            vec!["Update available: 0.3.0"]
        );
        // A release the host already runs, or older, is not offered.
        assert!(
            notes(
                &version("0.3.0", None),
                &version("0.3.0", None),
                Some("0.3.0")
            )
            .is_empty()
        );
        assert!(
            notes(
                &version("0.3.0", None),
                &version("0.3.0", None),
                Some("0.2.0")
            )
            .is_empty()
        );
        assert!(notes(&version("nope", None), &app, Some("also nope")).is_empty());
    }

    #[test]
    fn rows_show_the_live_version_online_and_the_last_seen_one_offline() {
        let saved = |id: &str, last_seen: Option<(&str, Option<&str>)>| SavedHost {
            id: id.to_string(),
            name: format!("host {id}"),
            connections: vec![HostConnection::Direct {
                address: "100.64.0.7:7130".to_string(),
            }],
            paired_at_ms: 0,
            last_seen_version: last_seen.map(|(version, _)| version.to_string()),
            last_seen_build: last_seen.and_then(|(_, build)| build.map(str::to_string)),
        };
        let app = version("0.2.0", Some("63bcff5c"));
        let hosts = vec![
            saved("online-host", Some(("0.1.0", None))),
            saved("offline-host", Some(("0.1.5", Some("abc1234")))),
            saved("never-seen", None),
        ];
        let rows = rows(&hosts, &app, Some("0.2.0"), |id| {
            (id == "online-host").then(|| version("0.2.0", Some("abc1234")))
        });
        assert_eq!(rows.len(), 3);

        // Online: the connection's hello wins over what was saved.
        assert!(rows[0].online);
        assert_eq!(rows[0].version.as_deref(), Some("0.2.0 (abc1234)"));
        assert_eq!(rows[0].notes, vec!["Different build from this app"]);
        assert_eq!(rows[0].name.as_ref(), "host online-host");
        assert_eq!(
            rows[0].detail.as_ref(),
            "100.64.0.7:7130 \u{b7} key online-hos\u{2026}"
        );

        // Offline: the last seen version, still compared.
        assert!(!rows[1].online);
        assert_eq!(
            rows[1].version.as_deref(),
            Some("last seen 0.1.5 (abc1234)")
        );
        assert_eq!(
            rows[1].notes,
            vec![
                "Behind this app; update the host",
                "Update available: 0.2.0"
            ]
        );

        // Never connected on a build that records versions: nothing to say.
        assert!(!rows[2].online);
        assert_eq!(rows[2].version, None);
        assert!(rows[2].notes.is_empty());
    }
}
