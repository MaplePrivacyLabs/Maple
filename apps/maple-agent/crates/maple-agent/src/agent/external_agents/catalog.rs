//! The external agents Maple can hand work to, as data (`catalog.json`): who
//! each is, how to find, probe and start it, and how to speak to it. Adding
//! an agent is adding an entry. A difference between agents is a key here,
//! or a named behaviour that a key selects; nothing tests an agent's id.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;

use super::acp_client;
use super::app_server::{AgentClient, RpcClient};
use super::claude;
use super::codex;
use super::driver::DriverMessage;

static ENTRIES: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("catalog.json"))
        .expect("the external agent catalog is valid; a test checks it")
});

/// Environment variables no agent inherits: they would tell an agent that
/// it runs inside another agent's session when Maple itself was started
/// from one.
const STRIPPED_ENVIRONMENT: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// One external agent.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CatalogEntry {
    /// What tools, settings and saved choices call it.
    pub(crate) id: String,
    pub(crate) name: String,
    /// The Settings card's description.
    pub(crate) description: String,
    /// What `list_agent_providers` says about running it.
    pub(crate) summary: String,
    /// The app asset the Settings card shows.
    pub(crate) icon: String,
    pub(crate) transport: Transport,
    /// The command found on the login PATH.
    pub(crate) executable: String,
    /// The oldest version Maple works with, when there is one.
    #[serde(default)]
    pub(crate) minimum_version: Option<String>,
    pub(crate) sign_in_probe: SignInProbe,
    /// Set for every start of the agent. Only switches that keep it from
    /// sending telemetry or updating itself belong here, never its
    /// configuration or sign-in.
    #[serde(default)]
    pub(crate) environment: BTreeMap<String, String>,
    /// How an ACP agent is started and spoken to.
    #[serde(default)]
    pub(crate) acp: Option<AcpSettings>,
    pub(crate) text: EntryText,
}

/// What starting and speaking to one ACP agent takes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AcpSettings {
    /// The arguments that start its ACP server on stdio.
    pub(crate) args: Vec<String>,
    /// What `initialize` carries in `_meta`.
    #[serde(default)]
    pub(crate) initialize_meta: Option<Value>,
    /// What `session/cancel` carries in `_meta`.
    #[serde(default)]
    pub(crate) cancel_meta: Option<Value>,
    /// How long start-up and opening the session may take.
    pub(crate) startup_seconds: u64,
}

/// What Maple says about an agent, word for word.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EntryText {
    /// The Settings card, when the command is not found.
    pub(crate) not_installed: String,
    /// A tool result, when the command is not found at launch.
    pub(crate) launch_missing: String,
    /// When `--version` fails; `{error}` stands for why.
    pub(crate) version_failed: String,
    pub(crate) sign_in_hint: String,
    /// The Settings card, when sign-in could not be checked; nothing when
    /// absent.
    #[serde(default)]
    pub(crate) sign_in_unknown: Option<String>,
}

/// How Maple tells whether an agent is signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SignInProbe {
    /// Whether `auth.json` exists under Codex's home. The file is never read.
    CodexAuthFile,
    /// `claude auth status --json`.
    ClaudeAuthStatus,
    /// Maple cannot tell without starting the agent.
    None,
}

/// How Maple speaks to an agent's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Transport {
    /// `codex app-server`: JSON-RPC, one process per thread.
    CodexAppServer,
    /// Claude Code's stream-json control protocol, one process per turn.
    ClaudeStreamJson,
    /// The Agent Client Protocol over stdio, one process per session.
    Acp,
}

/// One process start: its arguments, and what the connection needs.
pub(super) struct Launch {
    pub(super) args: Vec<String>,
    /// The session Claude Code is told to start or resume.
    session: Option<String>,
}

impl CatalogEntry {
    /// The arguments to start a process for `existing_thread`, or for a new
    /// thread.
    pub(super) fn launch(
        &self,
        existing_thread: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Launch {
        match self.transport {
            Transport::CodexAppServer => Launch {
                args: codex::app_server_args()
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect(),
                session: None,
            },
            Transport::ClaudeStreamJson => {
                let session = existing_thread
                    .map(str::to_string)
                    .unwrap_or_else(claude::new_session_id);
                Launch {
                    args: claude::command_args(&session, existing_thread.is_some(), model, effort),
                    session: Some(session),
                }
            }
            Transport::Acp => Launch {
                args: self
                    .acp
                    .as_ref()
                    .map(|acp| acp.args.clone())
                    .unwrap_or_default(),
                session: None,
            },
        }
    }

    /// Speak to a started process over its pipes.
    pub(super) fn connect(
        &'static self,
        stdin: ChildStdin,
        stdout: ChildStdout,
        launch: Launch,
    ) -> (
        Arc<AgentClient>,
        mpsc::Receiver<DriverMessage>,
        tokio::task::JoinHandle<()>,
    ) {
        match self.transport {
            Transport::CodexAppServer => {
                let (client, receiver, reader) = RpcClient::codex(stdin, stdout);
                (Arc::new(AgentClient::Codex(client)), receiver, reader)
            }
            Transport::ClaudeStreamJson => {
                let session = launch.session.unwrap_or_else(claude::new_session_id);
                let (client, receiver, reader) = claude::Client::new(stdin, stdout, session);
                (Arc::new(AgentClient::Claude(client)), receiver, reader)
            }
            Transport::Acp => {
                let (client, receiver, reader) = acp_client::Client::new(self, stdin, stdout);
                (Arc::new(AgentClient::Acp(client)), receiver, reader)
            }
        }
    }

    /// How long starting the agent and opening its thread may take, when
    /// the entry says.
    pub(super) fn startup_timeout(&self) -> Option<Duration> {
        self.acp
            .as_ref()
            .map(|acp| Duration::from_secs(acp.startup_seconds))
    }
}

impl Transport {
    /// Whether a process serves one turn and is reclaimed when it ends.
    pub(super) fn one_turn_per_process(self) -> bool {
        matches!(self, Self::ClaudeStreamJson)
    }
}

/// Every external agent, in the order Settings and tools list them.
pub(crate) fn entries() -> &'static [CatalogEntry] {
    &ENTRIES
}

impl CatalogEntry {
    /// Give a command that runs this agent, to work or only to answer a
    /// probe, the agent's environment: without the variables of another
    /// agent's session Maple may itself run inside, and with the entry's
    /// own, which switch off telemetry and self-updates.
    pub(in crate::agent) fn apply_environment(&self, command: &mut tokio::process::Command) {
        for name in STRIPPED_ENVIRONMENT {
            command.env_remove(name);
        }
        command.envs(&self.environment);
    }
}

/// The entry with exactly this id. Entry points that take an id from a
/// person or a model trim it first; stored ids are compared as they are.
pub(crate) fn entry(id: &str) -> Option<&'static CatalogEntry> {
    entries().iter().find(|entry| entry.id == id)
}

/// The agents' names for text that gives examples: "Codex, Claude Code or
/// Grok Build".
pub(super) fn example_names() -> String {
    either(entries().iter().map(|entry| entry.name.clone()))
}

/// The agents' ids for text that gives examples: `"codex", "claude" or
/// "grok"`.
pub(super) fn example_ids() -> String {
    either(entries().iter().map(|entry| format!("\"{}\"", entry.id)))
}

/// `a, b or c`.
fn either(items: impl Iterator<Item = String>) -> String {
    let mut items = items.collect::<Vec<_>>();
    let Some(last) = items.pop() else {
        return String::new();
    };
    if items.is_empty() {
        return last;
    }
    format!("{} or {last}", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_reads_and_its_ids_are_unique() {
        let ids: Vec<&str> = entries().iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["codex", "claude", "grok", "antigravity"]);
        for entry in entries() {
            assert!(!entry.name.is_empty() && !entry.executable.is_empty());
            assert!(entry.icon.starts_with("icons/"), "{}", entry.icon);
            // An ACP agent says how to start it; no other agent does.
            assert_eq!(
                entry.acp.is_some(),
                entry.transport == Transport::Acp,
                "{}",
                entry.id
            );
            for name in entry.environment.keys() {
                assert!(
                    name.contains("TELEMETRY") || name.contains("UPDATE"),
                    "{name} is not an off switch"
                );
            }
            if let Some(minimum) = &entry.minimum_version {
                assert!(
                    crate::agent::integrations::parse_version(minimum).is_some(),
                    "{minimum}"
                );
            }
        }
        assert_eq!(
            entry("claude").map(|entry| entry.name.as_str()),
            Some("Claude Code")
        );
        // Ids match exactly: a padded stored id is not an agent.
        assert!(entry(" claude ").is_none());
        assert!(entry("cua-driver").is_none());
        assert_eq!(
            example_names(),
            "Codex, Claude Code, Grok Build or Antigravity"
        );
        assert_eq!(
            example_ids(),
            "\"codex\", \"claude\", \"grok\" or \"antigravity\""
        );
    }

    #[test]
    fn an_agents_command_drops_another_agents_session_variables() {
        let mut command = tokio::process::Command::new("agent");
        command.env("CLAUDECODE", "1").env("KEEP", "1");
        entry("codex").unwrap().apply_environment(&mut command);
        let envs: Vec<(String, Option<String>)> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for name in STRIPPED_ENVIRONMENT {
            assert!(envs.contains(&(name.to_string(), None)), "{envs:?}");
        }
        assert!(envs.contains(&("KEEP".to_string(), Some("1".to_string()))));
    }

    #[test]
    fn transports_launch_as_before() {
        let codex = entry("codex")
            .unwrap()
            .launch(None, Some("m"), Some("high"));
        assert_eq!(codex.args, ["app-server"]);
        let claude = entry("claude").unwrap();
        let fresh = claude.launch(None, Some("m"), Some("high"));
        let session = fresh.session.clone().unwrap();
        assert!(
            fresh
                .args
                .windows(2)
                .any(|pair| pair == ["--session-id", session.as_str()])
        );
        assert!(fresh.args.windows(2).any(|pair| pair == ["--model", "m"]));
        let resumed = claude.launch(Some("s1"), None, None);
        assert!(
            resumed
                .args
                .windows(2)
                .any(|pair| pair == ["--resume", "s1"])
        );
        assert!(Transport::ClaudeStreamJson.one_turn_per_process());
        assert!(!Transport::CodexAppServer.one_turn_per_process());
        assert!(!Transport::Acp.one_turn_per_process());
        // ACP agents start as their entries say, whatever the thread.
        let grok = entry("grok").unwrap();
        assert_eq!(
            grok.launch(Some("s1"), Some("m"), None).args,
            ["agent", "stdio"]
        );
        assert_eq!(grok.startup_timeout(), Some(Duration::from_secs(20)));
        assert_eq!(
            entry("antigravity").unwrap().launch(None, None, None).args,
            ["acp", "--disable-telemetry"]
        );
        assert_eq!(claude.startup_timeout(), None);
    }
}
