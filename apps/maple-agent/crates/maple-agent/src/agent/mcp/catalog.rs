//! The well-known apps Integrations offers to connect. Each is data in
//! `catalog.json`: a subset of the MCP registry's `server.json`, plus how Maple
//! signs in and what Integrations shows, so adding an app needs no code. An
//! entry says how to reach the app, what must be on this computer first and
//! how to sign in; every app is one mix of those. The list ships with Maple,
//! so browsing it tells no server anything.

use std::sync::OnceLock;

use serde::Deserialize;

use super::oauth::{McpSignIn, sign_in_endpoint};
use crate::store::records::McpTarget;

/// How long a new app gets to start and list its tools, unless it says.
const DEFAULT_CONNECT_SECONDS: u64 = 120;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogEntry {
    pub id: String,
    /// The server's name in the MCP registry, for those who curate this list.
    #[serde(default)]
    pub registry: Option<String>,
    pub name: String,
    pub summary: String,
    pub category: McpCatalogCategory,
    pub connect: McpCatalogConnection,
    pub auth: McpCatalogAuth,
    /// What must be on this computer first, checked in this order.
    #[serde(default)]
    pub requires: Vec<McpCatalogRequirement>,
    /// Where the app runs. Empty means everywhere Maple does.
    #[serde(default)]
    pub platforms: Vec<McpCatalogPlatform>,
    /// Seconds a new app gets to start and list its tools.
    #[serde(default = "default_connect_seconds")]
    pub connect_seconds: u64,
    /// What to tell the person when everything is here but it will not start.
    #[serde(default)]
    pub help: Option<String>,
}

fn default_connect_seconds() -> u64 {
    DEFAULT_CONNECT_SECONDS
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpCatalogCategory {
    Productivity,
    Teams,
    Business,
    Developer,
}

impl McpCatalogCategory {
    pub const ALL: [Self; 4] = [
        Self::Productivity,
        Self::Teams,
        Self::Business,
        Self::Developer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Productivity => "Productivity",
            Self::Teams => "Teams",
            Self::Business => "Business",
            Self::Developer => "Developer",
        }
    }
}

/// How Maple reaches an app.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpCatalogConnection {
    /// A server on the internet.
    #[serde(rename = "streamable-http")]
    Remote(McpCatalogRemote),
    /// A package Maple runs at an exact version.
    Package(McpCatalogPackage),
    /// A command that some other app installs.
    Command(McpCatalogCommand),
    /// A server in an app the person runs, at an address only they know.
    Address(McpCatalogAddress),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogRemote {
    pub url: String,
}

/// What the pop-up asks for before an app at the person's own address can connect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogAddress {
    pub label: String,
    /// Where the person finds it.
    pub help: String,
    /// Where it is on most setups, tried before asking.
    pub default: String,
    /// The server's path, which the person may leave off.
    pub path: String,
    pub app_name: String,
    pub get_url: String,
}

impl McpCatalogAddress {
    /// The server's address from what the person gave.
    fn server_url(&self, given: &str) -> Result<String, String> {
        let given = given.trim();
        if given.is_empty() {
            return Err(format!("Enter the {} first.", self.label));
        }
        let given = if given.contains("://") {
            given.to_string()
        } else {
            format!("http://{given}")
        };
        let mut url = sign_in_endpoint(&given)?;
        if url.path() == "/" {
            url.set_path(&self.path);
        }
        Ok(url.to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogPackage {
    pub registry_type: McpCatalogRegistry,
    pub identifier: String,
    pub version: String,
    #[serde(default)]
    pub arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpCatalogRegistry {
    Npm,
    Pypi,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogCommand {
    pub command: String,
    #[serde(default)]
    pub arguments: Vec<String>,
}

/// One thing an app needs, and how to tell it is here: the command must
/// succeed, and print at least `min_version` if there is one. It never runs
/// through a shell, and comes only from the bundled catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogRequirement {
    /// Named as the person knows it: "Needs {name}" and "Get {name}".
    pub name: String,
    pub check: Vec<String>,
    #[serde(default)]
    pub min_version: Option<String>,
    pub get_url: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum McpCatalogPlatform {
    Macos,
    Windows,
    Linux,
}

impl McpCatalogPlatform {
    fn current() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => Some(Self::Macos),
            "windows" => Some(Self::Windows),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum McpCatalogAuth {
    /// Browser sign-in, which [`McpCatalogEntry::sign_in`] starts.
    Oauth,
    /// Nothing to sign in to.
    None,
}

impl McpCatalogEntry {
    /// The browser sign-in for an app on the internet.
    pub fn sign_in(&self) -> Option<McpSignIn> {
        Some(McpSignIn {
            name: self.name.clone(),
            description: self.summary.clone(),
            url: self.remote()?.url.clone(),
            scopes: Vec::new(),
        })
    }

    /// The browser sign-in for an app at the address the person gave.
    pub fn sign_in_at(&self, given: &str) -> Result<McpSignIn, String> {
        let address = self
            .address()
            .ok_or_else(|| "This app has no address to give.".to_string())?;
        Ok(McpSignIn {
            name: self.name.clone(),
            description: self.summary.clone(),
            url: address.server_url(given)?,
            scopes: Vec::new(),
        })
    }

    pub fn remote(&self) -> Option<&McpCatalogRemote> {
        match &self.connect {
            McpCatalogConnection::Remote(remote) => Some(remote),
            _ => None,
        }
    }

    /// What the person has to give before this app can connect.
    pub fn address(&self) -> Option<&McpCatalogAddress> {
        match &self.connect {
            McpCatalogConnection::Address(address) => Some(address),
            _ => None,
        }
    }

    /// What a saved server of this app runs or reaches, where the catalog
    /// alone can say.
    pub fn target(&self) -> Option<McpTarget> {
        let line = |words: Vec<&str>| McpTarget::Stdio {
            command: words.join(" "),
        };
        Some(match &self.connect {
            McpCatalogConnection::Remote(remote) => McpTarget::StreamableHttp {
                url: remote.url.clone(),
            },
            McpCatalogConnection::Address(_) => return None,
            McpCatalogConnection::Package(package) => {
                let (runner, spec) = match package.registry_type {
                    McpCatalogRegistry::Npm => (
                        vec!["npx", "-y"],
                        format!("{}@{}", package.identifier, package.version),
                    ),
                    McpCatalogRegistry::Pypi => (
                        vec!["uvx"],
                        format!("{}=={}", package.identifier, package.version),
                    ),
                };
                line(
                    runner
                        .into_iter()
                        .chain([spec.as_str()])
                        .chain(package.arguments.iter().map(String::as_str))
                        .collect(),
                )
            }
            McpCatalogConnection::Command(command) => line(
                std::iter::once(command.command.as_str())
                    .chain(command.arguments.iter().map(String::as_str))
                    .collect(),
            ),
        })
    }

    fn runs_on(&self, platform: Option<McpCatalogPlatform>) -> bool {
        self.platforms.is_empty() || platform.is_some_and(|here| self.platforms.contains(&here))
    }
}

fn every_entry() -> &'static [McpCatalogEntry] {
    static CATALOG: OnceLock<Vec<McpCatalogEntry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("catalog.json")).expect("the bundled catalog is valid")
    })
}

/// The apps for this computer, in the order Integrations shows them.
pub fn mcp_catalog() -> &'static [McpCatalogEntry] {
    static HERE: OnceLock<Vec<McpCatalogEntry>> = OnceLock::new();
    HERE.get_or_init(|| {
        let platform = McpCatalogPlatform::current();
        every_entry()
            .iter()
            .filter(|entry| entry.runs_on(platform))
            .cloned()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::{
        edit_to_validation_server, name_to_key, normalize_mcp_servers, split_command,
    };
    use super::*;
    use crate::store::records::McpServerEdit;

    #[test]
    fn every_entry_is_a_server_maple_could_save() {
        let catalog = every_entry();
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        for entry in catalog {
            assert!(ids.insert(&entry.id), "{} repeats an id", entry.id);
            assert!(
                keys.insert(name_to_key(&entry.name)),
                "{} repeats a name",
                entry.name
            );
            assert!(!entry.summary.is_empty(), "{} has no summary", entry.name);
            let target = match entry.address() {
                Some(address) => McpTarget::StreamableHttp {
                    url: entry.sign_in_at(&address.default).unwrap().url,
                },
                None => entry.target().unwrap(),
            };
            let edit = McpServerEdit {
                key: name_to_key(&entry.name),
                name: entry.name.clone(),
                description: entry.summary.clone(),
                enabled: false,
                timeout_seconds: 300,
                target,
                environment: Vec::new(),
                headers: Vec::new(),
            };
            normalize_mcp_servers(vec![edit_to_validation_server(&edit)]).unwrap();
        }
    }

    #[test]
    fn every_address_is_https_and_keeps_secrets_out_of_the_url() {
        for entry in every_entry()
            .iter()
            .filter(|entry| entry.remote().is_some())
        {
            let url = sign_in_endpoint(&entry.remote().unwrap().url).unwrap();
            assert_eq!(url.scheme(), "https", "{}", entry.name);
            assert_eq!(url.query(), None, "{} has a query", entry.name);
            assert!(
                !url.as_str().contains('{'),
                "{} puts a variable in its URL",
                entry.name
            );
        }
    }

    #[test]
    fn a_local_app_is_pinned_and_says_what_it_needs() {
        for entry in every_entry()
            .iter()
            .filter(|entry| entry.remote().is_none() && entry.address().is_none())
        {
            assert_eq!(entry.auth, McpCatalogAuth::None, "{}", entry.name);
            assert!(!entry.requires.is_empty(), "{}", entry.name);
            for requirement in &entry.requires {
                assert!(!requirement.check.is_empty(), "{}", entry.name);
                assert!(
                    requirement.get_url.starts_with("https://"),
                    "{}",
                    entry.name
                );
            }
            assert!(entry.help.is_some(), "{} has no help", entry.name);
            if let McpCatalogConnection::Package(package) = &entry.connect {
                assert!(
                    package
                        .version
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.'),
                    "{} is not an exact version",
                    entry.name
                );
            }
            // What is saved reads back as the same words.
            let Some(McpTarget::Stdio { command }) = entry.target() else {
                panic!("{} is not a local command", entry.name);
            };
            assert_eq!(
                split_command(&command).unwrap().join(" "),
                command,
                "{}",
                entry.name
            );
        }
    }

    #[test]
    fn an_address_is_taken_as_the_person_gave_it() {
        let entry = every_entry()
            .iter()
            .find(|entry| entry.id == "home-assistant")
            .unwrap();
        let url = |given: &str| entry.sign_in_at(given).map(|sign_in| sign_in.url);
        for given in [
            "http://homeassistant.local:8123",
            "http://homeassistant.local:8123/",
            " homeassistant.local:8123 ",
        ] {
            assert_eq!(
                url(given).unwrap(),
                "http://homeassistant.local:8123/api/mcp"
            );
        }
        assert_eq!(
            url("https://home.example.com").unwrap(),
            "https://home.example.com/api/mcp"
        );
        assert_eq!(
            url("http://192.168.1.20:8123/api/mcp/assist").unwrap(),
            "http://192.168.1.20:8123/api/mcp/assist"
        );
        assert_eq!(
            url("  ").unwrap_err(),
            "Enter the Home Assistant address first."
        );
        assert_eq!(
            url("100.64.0.1:8123").unwrap(),
            "http://100.64.0.1:8123/api/mcp"
        );
        assert_eq!(
            url("http://home.example.com:8123").unwrap(),
            "http://home.example.com:8123/api/mcp"
        );
        assert!(url("ftp://home.example.com").is_err());
    }

    #[test]
    fn an_app_at_a_persons_own_address_says_how_to_find_it() {
        let apps: Vec<_> = every_entry()
            .iter()
            .filter(|entry| entry.address().is_some())
            .collect();
        assert_eq!(apps.len(), 2);
        for entry in apps {
            let address = entry.address().unwrap();
            assert_eq!(entry.auth, McpCatalogAuth::Oauth, "{}", entry.name);
            assert_eq!(entry.target(), None, "{}", entry.name);
            assert!(address.path.starts_with('/'), "{}", entry.name);
            assert!(address.get_url.starts_with("https://"), "{}", entry.name);
            assert!(entry.sign_in_at(&address.default).is_ok(), "{}", entry.name);
        }
    }

    #[test]
    fn a_package_runs_at_its_exact_version() {
        let entry = |id: &str| every_entry().iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(
            entry("playwright").target(),
            Some(McpTarget::Stdio {
                command: "npx -y @playwright/mcp@0.0.83 --isolated --output-dir ~/.cache/maple/playwright"
                    .into()
            })
        );
        assert_eq!(
            entry("xcode").target(),
            Some(McpTarget::Stdio {
                command: "xcrun mcpbridge".into()
            })
        );
    }

    #[test]
    fn an_app_only_shows_where_it_runs() {
        let entry = |id: &str| every_entry().iter().find(|entry| entry.id == id).unwrap();
        for platform in [
            McpCatalogPlatform::Macos,
            McpCatalogPlatform::Windows,
            McpCatalogPlatform::Linux,
        ] {
            assert!(entry("playwright").runs_on(Some(platform)));
            assert_eq!(
                entry("xcode").runs_on(Some(platform)),
                platform == McpCatalogPlatform::Macos
            );
        }
        assert_eq!(
            mcp_catalog().iter().any(|entry| entry.id == "xcode"),
            cfg!(target_os = "macos")
        );
    }
}
