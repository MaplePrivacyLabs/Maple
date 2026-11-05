//! The well-known apps Integrations offers to connect. Each is data in
//! `catalog.json`: a subset of the MCP registry's `server.json`, plus how Maple
//! signs in and what Integrations shows, so adding an app needs no code. The
//! list ships with Maple, so browsing it tells no server anything.

use std::sync::OnceLock;

use serde::Deserialize;

use super::oauth::McpSignIn;

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
    pub remote: McpCatalogRemote,
    pub auth: McpCatalogAuth,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogRemote {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<McpCatalogHeader>,
}

/// A header the person fills in. `{token}` in `value` marks where the secret goes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCatalogHeader {
    pub name: String,
    pub value: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum McpCatalogAuth {
    /// Browser sign-in, which [`McpCatalogEntry::sign_in`] starts.
    Oauth,
    /// A token the person gets from the app and pastes in.
    Token { get_key_url: String },
}

impl McpCatalogEntry {
    pub fn sign_in(&self) -> McpSignIn {
        McpSignIn {
            name: self.name.clone(),
            description: self.summary.clone(),
            url: self.remote.url.clone(),
            scopes: Vec::new(),
        }
    }
}

/// The catalog, in the order Integrations shows it.
pub fn mcp_catalog() -> &'static [McpCatalogEntry] {
    static CATALOG: OnceLock<Vec<McpCatalogEntry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("catalog.json")).expect("the bundled catalog is valid")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::oauth::sign_in_endpoint;
    use super::super::{edit_to_validation_server, name_to_key, normalize_mcp_servers};
    use super::*;
    use crate::store::records::{McpServerEdit, McpTarget};

    #[test]
    fn every_entry_is_a_server_maple_could_save() {
        let catalog = mcp_catalog();
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
            assert_eq!(entry.remote.kind, "streamable-http", "{}", entry.name);
            let edit = McpServerEdit {
                key: name_to_key(&entry.name),
                name: entry.name.clone(),
                description: entry.summary.clone(),
                enabled: false,
                timeout_seconds: 300,
                target: McpTarget::StreamableHttp {
                    url: entry.remote.url.clone(),
                },
                environment: Vec::new(),
                headers: Vec::new(),
            };
            normalize_mcp_servers(vec![edit_to_validation_server(&edit)]).unwrap();
        }
    }

    #[test]
    fn every_address_is_https_and_keeps_secrets_out_of_the_url() {
        for entry in mcp_catalog() {
            let url = sign_in_endpoint(&entry.remote.url).unwrap();
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
    fn a_token_entry_says_where_to_get_it_and_where_it_goes() {
        for entry in mcp_catalog() {
            if let McpCatalogAuth::Token { get_key_url } = &entry.auth {
                assert!(get_key_url.starts_with("https://"), "{}", entry.name);
                assert!(
                    entry
                        .remote
                        .headers
                        .iter()
                        .any(|header| header.value.contains("{token}")),
                    "{} has nowhere to put its token",
                    entry.name
                );
            } else {
                assert!(entry.remote.headers.is_empty(), "{}", entry.name);
            }
        }
    }
}
