//! Exercises Goose's ordered dispatch through Maple's actual native Python client.
//! The fake provider supplies one batch; Python execution and process cleanup are real.

use super::*;
use crate::agent::AgentToolContextSpec;
use crate::agent::developer_tools::MapleDeveloperClient;
use crate::agent::web_tools::WebToolState;
use futures_util::StreamExt;
use goose::agents::{
    Agent, AgentConfig, AgentEvent, ExtensionConfig, GoosePlatform, SessionConfig,
};
use goose::config::{GooseMode, PermissionManager};
use goose::conversation::message::Message;
use goose::session::{SessionManager, SessionType};
use goose_providers::base::{MessageStream, Provider, stream_from_single_message};
use goose_providers::conversation::token_usage::{ProviderUsage, Usage};
use goose_providers::errors::ProviderError;
use goose_providers::model::ModelConfig;
use rmcp::model::CallToolRequestParams;
use std::num::NonZeroUsize;
use std::time::Duration;

struct BatchProvider(Mutex<Option<Message>>);

#[async_trait::async_trait]
impl Provider for BatchProvider {
    fn get_name(&self) -> &str {
        "ordered-python-test"
    }

    async fn stream(
        &self,
        _model_config: &ModelConfig,
        _system: &str,
        _messages: &[Message],
        _tools: &[Tool],
    ) -> Result<MessageStream, ProviderError> {
        let message = self
            .0
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Message::assistant().with_text("Done."));
        Ok(stream_from_single_message(
            message,
            ProviderUsage::new("ordered-python-test".into(), Usage::default()),
        ))
    }
}

struct NativeBatch {
    _temp: tempfile::TempDir,
    root: PathBuf,
    agent: Arc<Agent>,
    session_id: String,
    runtime: Runtime,
    binding: Arc<PythonTaskBinding>,
    context: SharedAgentToolContext,
}

impl NativeBatch {
    async fn new(calls: &[(&str, &str, bool)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let sessions = Arc::new(SessionManager::new(root.join("sessions")));
        let session = sessions
            .create_session(
                root.clone(),
                "Ordered Python test".into(),
                SessionType::User,
                GooseMode::Auto,
            )
            .await
            .unwrap();
        let mut config = AgentConfig::new(
            sessions,
            Arc::new(PermissionManager::new(root.join("permissions"))),
            None,
            GooseMode::Auto,
            true,
            GoosePlatform::GooseDesktop,
        )
        .with_use_login_shell_path(false)
        .with_ordered_tool_calls(
            ["python_code".into(), "developer__python_code".into()],
            NonZeroUsize::new(32).unwrap(),
        );
        // Agent::with_config otherwise discovers and executes installed user
        // hooks. Suppress those external programs in this native test harness.
        config.is_subagent = true;
        let agent = Arc::new(Agent::with_config(config));
        let batch = calls.iter().enumerate().fold(
            Message::assistant(),
            |message, (index, (name, code, reset))| {
                message.with_tool_request(
                    format!("cell-{index}"),
                    Ok(CallToolRequestParams::new((*name).to_string())
                        .with_arguments(object!({ "code": code, "reset": reset }))),
                )
            },
        );
        agent
            .update_provider(
                Arc::new(BatchProvider(Mutex::new(Some(batch)))),
                ModelConfig::new("ordered-python-test"),
                &session.id,
            )
            .await
            .unwrap();

        let runtime = Runtime::default();
        let context = SharedAgentToolContext::new(AgentToolContextSpec::default());
        let manifest = std::env::var_os("MAPLE_CODE_MODE_RUNTIME_MANIFEST")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/runtime/python/runtime.json")
            });
        let package = PackagedPython::from_manifest(manifest)
            .expect("prepare the bundled Python fixture with `nix develop -c just python-prepare`");
        let binding = Arc::new(
            PythonTaskBinding::new(
                runtime.clone(),
                "ordered-python-task".into(),
                session.id.clone(),
                root.clone(),
                context.clone(),
            )
            .with_packaged_python(package),
        );
        // Start the real worker without an interactive login-shell probe. The
        // injected client subsequently reuses this immutable launch configuration.
        let ready = binding
            .call(
                PythonParams {
                    code: "None".into(),
                    reset: false,
                },
                &ToolCallContext::new(session.id.clone(), Some(root.clone()), None),
                std::future::ready(None),
                CancellationToken::new(),
            )
            .await;
        assert!(!ready.is_error.unwrap_or(false), "{ready:?}");
        let client = MapleDeveloperClient::new(
            agent.extension_manager.get_context().clone(),
            true,
            crate::maple_api::test_maple_api_session("ordered-python-test"),
            Arc::new(WebToolState::default()),
            context.clone(),
        )
        .unwrap()
        .with_python_binding(binding.clone());
        agent
            .extension_manager
            .add_client(
                "developer".into(),
                ExtensionConfig::Builtin {
                    name: "developer".into(),
                    description: "Native Python test".into(),
                    display_name: None,
                    timeout: None,
                    bundled: Some(true),
                    available_tools: Vec::new(),
                },
                Arc::new(client),
                None,
                None,
            )
            .await;
        Self {
            _temp: temp,
            root,
            agent,
            session_id: session.id,
            runtime,
            binding,
            context,
        }
    }

    fn start(&self, run: CancellationToken) -> tokio::task::JoinHandle<Vec<CallToolResult>> {
        let agent = self.agent.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            let mut stream = agent
                .reply(
                    Message::user().with_text("Run the supplied Python batch."),
                    SessionConfig {
                        id: session_id,
                        schedule_id: None,
                        max_turns: Some(2),
                        retry_config: None,
                    },
                    Some(run),
                )
                .await
                .unwrap();
            let mut responses = BTreeMap::new();
            while let Some(event) = stream.next().await {
                if let AgentEvent::Message(message) = event.unwrap() {
                    for response in message.content.iter().filter_map(|c| c.as_tool_response()) {
                        responses.insert(
                            response.id.clone(),
                            response
                                .tool_result
                                .clone()
                                .unwrap_or_else(|error| python_error(error.to_string())),
                        );
                    }
                }
            }
            responses.into_values().collect()
        })
    }

    async fn cleanup(&self) {
        self.binding.retire("ordered test complete").await.unwrap();
        self.runtime
            .shutdown("ordered test complete")
            .await
            .unwrap();
        assert!(self.runtime.snapshot().holders.is_empty());
    }
}

fn python_body(result: &CallToolResult) -> &serde_json::Value {
    assert!(!result.is_error.unwrap_or(false), "{result:?}");
    &result.structured_content.as_ref().unwrap()["maple_python"]
}

#[tokio::test]
async fn native_ordered_batch_shares_state_between_cells() {
    let batch = NativeBatch::new(&[
        (
            "python_code",
            "import asyncio\nawait asyncio.sleep(0.05)\nanswer = 40\nanswer",
            false,
        ),
        ("python_code", "answer += 2\nanswer", false),
    ])
    .await;
    let results = tokio::time::timeout(
        Duration::from_secs(30),
        batch.start(CancellationToken::new()),
    )
    .await
    .expect("ordered native batch must finish")
    .unwrap();
    batch.cleanup().await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(python_body(&results[0])["value"], "40");
    assert_eq!(python_body(&results[1])["value"], "42");
    assert_eq!(
        python_body(&results[0])["generation"],
        python_body(&results[1])["generation"]
    );
}

#[tokio::test]
async fn native_ordered_reset_replaces_state_before_following_cell() {
    let batch = NativeBatch::new(&[
        ("python_code", "answer = 40\nanswer", false),
        (
            "python_code",
            "assert 'answer' not in globals()\nreplacement = 6\nreplacement",
            true,
        ),
        ("python_code", "replacement += 1\nreplacement", false),
    ])
    .await;
    let results = tokio::time::timeout(
        Duration::from_secs(30),
        batch.start(CancellationToken::new()),
    )
    .await
    .expect("reset and replacement cells must finish")
    .unwrap();
    batch.cleanup().await;
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(python_body(&results[0])["value"], "40");
    assert_eq!(python_body(&results[1])["value"], "6");
    assert_eq!(python_body(&results[2])["value"], "7");
    assert_ne!(
        python_body(&results[0])["generation"],
        python_body(&results[1])["generation"]
    );
    assert_eq!(
        python_body(&results[1])["generation"],
        python_body(&results[2])["generation"]
    );
}

async fn queued_cell_cannot_outlive_its_authority(retire_owner: bool) {
    let batch = NativeBatch::new(&[
        (
            "python_code",
            "from pathlib import Path\nimport asyncio\nPath('started').touch()\nawait asyncio.Event().wait()",
            false,
        ),
        (
            "python_code",
            "from pathlib import Path\nPath('queued-effect').touch()",
            false,
        ),
    ])
    .await;
    let run = CancellationToken::new();
    let execution = batch.start(run.clone());
    tokio::time::timeout(Duration::from_secs(30), async {
        while !batch.root.join("started").exists() {
            assert!(!execution.is_finished(), "foreground cell failed to start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first native cell must start before authority is withdrawn");
    if retire_owner {
        batch.context.revoke();
        batch.binding.retire("test owner retired").await.unwrap();
    } else {
        run.cancel();
    }
    tokio::time::timeout(Duration::from_secs(30), execution)
        .await
        .expect("queued batch must settle after losing authority")
        .unwrap();
    batch.cleanup().await;
    assert!(!batch.root.join("queued-effect").exists());
}

#[tokio::test]
async fn native_ordered_queue_does_not_execute_after_run_cancellation() {
    queued_cell_cannot_outlive_its_authority(false).await;
}

#[tokio::test]
async fn native_ordered_queue_does_not_execute_after_owner_retirement() {
    queued_cell_cannot_outlive_its_authority(true).await;
}
