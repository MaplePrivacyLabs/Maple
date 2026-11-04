//! Request-only serialization for newly entered MCP values. The storage input
//! types remain Deserialize-only and cannot accidentally become response DTOs.

use maple_agent::store::records::{
    McpDefinition, McpSecretEdit, McpSecretUpdate, McpServerEdit, McpTarget,
};
use serde::{Serialize, Serializer};

impl super::AgentBackend {
    #[cfg(test)]
    pub async fn save_mcp_servers(
        &self,
        user_id: &str,
        servers: Vec<maple_agent::agent::AgentMcpServer>,
    ) -> Result<Vec<maple_agent::agent::AgentMcpServer>, String> {
        self.call("save_mcp_servers", (user_id, servers)).await
    }

    pub async fn save_mcp_definition_edits(
        &self,
        user_id: &str,
        edits: Vec<McpServerEdit>,
    ) -> Result<Vec<McpDefinition>, String> {
        // Borrow entered SecretStrings through encoding, then their owning
        // zeroizing inputs are dropped when this request completes or cancels.
        // No saved values are ever fetched to build this request.
        self.call("save_mcp_definition_edits", (user_id, EditsInput(&edits)))
            .await
    }

    /// Sign in to an HTTP server through this computer's browser. A paired
    /// computer's own browser and callback are not reachable from here yet.
    pub async fn sign_in_mcp_server(
        &self,
        user_id: &str,
        request: maple_agent::agent::McpSignIn,
    ) -> Result<Vec<McpDefinition>, String> {
        if self.remote_host.is_some() {
            return Err("Sign in from This Computer. Paired computers cannot do this yet.".into());
        }
        self.call("sign_in_mcp_server", (user_id, request)).await
    }

    pub async fn cancel_mcp_sign_in(&self, user_id: &str, name: &str) -> Result<(), String> {
        self.call("cancel_mcp_sign_in", (user_id, name)).await
    }
}

struct EditsInput<'a>(&'a [McpServerEdit]);

impl Serialize for EditsInput<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(ServerInput::from))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerInput<'a> {
    key: &'a str,
    name: &'a str,
    description: &'a str,
    enabled: bool,
    timeout_seconds: u64,
    target: &'a McpTarget,
    environment: Vec<SecretEditInput<'a>>,
    headers: Vec<SecretEditInput<'a>>,
}

impl<'a> From<&'a McpServerEdit> for ServerInput<'a> {
    fn from(edit: &'a McpServerEdit) -> Self {
        Self {
            key: &edit.key,
            name: &edit.name,
            description: &edit.description,
            enabled: edit.enabled,
            timeout_seconds: edit.timeout_seconds,
            target: &edit.target,
            environment: edit.environment.iter().map(SecretEditInput::from).collect(),
            headers: edit.headers.iter().map(SecretEditInput::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct SecretEditInput<'a> {
    name: &'a str,
    update: SecretUpdateInput<'a>,
}

impl<'a> From<&'a McpSecretEdit> for SecretEditInput<'a> {
    fn from(edit: &'a McpSecretEdit) -> Self {
        Self {
            name: &edit.name,
            update: match &edit.update {
                McpSecretUpdate::Keep => SecretUpdateInput::Keep,
                McpSecretUpdate::Clear => SecretUpdateInput::Clear,
                McpSecretUpdate::Set(value) => SecretUpdateInput::Set(value.expose()),
            },
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "operation", content = "value", rename_all = "snake_case")]
enum SecretUpdateInput<'a> {
    Keep,
    Clear,
    Set(&'a str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use maple_agent::store::records::SecretString;

    #[test]
    fn explicit_request_adapter_round_trips_operations_without_response_serialization() {
        let edit = McpServerEdit {
            key: "example".into(),
            name: "Example".into(),
            description: String::new(),
            enabled: true,
            timeout_seconds: 30,
            target: McpTarget::Stdio {
                command: "example --stdio".into(),
            },
            environment: vec![
                McpSecretEdit {
                    name: "NEW".into(),
                    update: McpSecretUpdate::Set(SecretString::new(
                        "synthetic-entered-value".into(),
                    )),
                },
                McpSecretEdit {
                    name: "SAVED".into(),
                    update: McpSecretUpdate::Keep,
                },
                McpSecretEdit {
                    name: "REMOVED".into(),
                    update: McpSecretUpdate::Clear,
                },
            ],
            headers: vec![],
        };
        let input = serde_json::to_value(EditsInput(std::slice::from_ref(&edit))).unwrap();
        let decoded: Vec<McpServerEdit> = serde_json::from_value(input).unwrap();
        assert_eq!(decoded, vec![edit]);
        assert!(!format!("{decoded:?}").contains("synthetic-entered-value"));
    }
}
