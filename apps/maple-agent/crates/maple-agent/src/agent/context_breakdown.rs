//! What fills a task's model context, by category, for the composer's
//! context card. The parts are estimates: Pi's request estimator sizes
//! each as the model is sent it, and the card scales them to the meter.

use pi_ai::transcript::current_system_message;
use pi_ai::{Content, Message, Model, UserMessage, estimate_message_tokens, estimate_text_tokens};
use pi_coding_agent::SessionMessage;
use pi_coding_agent::compaction::CompactionSettings;

use super::AgentContextBreakdown;

impl AgentContextBreakdown {
    /// Every category together; `compact_at` is not one.
    pub fn total(&self) -> u64 {
        self.instructions + self.summary + self.messages + self.tool_results + self.images
    }

    /// The breakdown of a loaded task: `messages`, its model context, on
    /// `model` with `compaction`, the session's compaction settings.
    /// `None` when the context holds nothing the model reads.
    pub(super) fn of_session(
        messages: &[SessionMessage],
        model: Option<&Model>,
        compaction: &CompactionSettings,
    ) -> Option<Self> {
        let image = image_tokens();
        // The transcript's system messages declare the prompt and the
        // tools, and later ones change them. A request sends what they add
        // up to, each tool once, so they count once, as that.
        let instructions = current_system_message(messages.iter().filter_map(model_message))
            .map_or(0, |system| {
                estimate_message_tokens(&Message::System(system))
            });
        let mut breakdown = Self {
            instructions,
            compact_at: compact_at(model, compaction),
            ..Self::default()
        };
        for message in messages {
            match message {
                SessionMessage::Llm(Message::System(_)) => {}
                SessionMessage::Llm(llm @ Message::User(user)) => {
                    let (text, images) = text_and_images(llm, &user.content);
                    breakdown.messages += text;
                    breakdown.images += images * image;
                }
                SessionMessage::Llm(reply @ Message::Assistant(_)) => {
                    breakdown.messages += estimate_message_tokens(reply);
                }
                SessionMessage::Llm(llm @ Message::ToolResult(result)) => {
                    let (text, images) = text_and_images(llm, &result.content);
                    breakdown.tool_results += text;
                    breakdown.images += images * image;
                }
                SessionMessage::Custom(custom) => {
                    breakdown.messages += text_tokens(&custom.content);
                    breakdown.images += image_count(&custom.content) * image;
                }
                // A `!!` command never reaches the model.
                SessionMessage::BashExecution(_) if message.excluded_from_context() => {}
                SessionMessage::BashExecution(_) => {
                    breakdown.tool_results += estimate_message_tokens(&message.to_llm());
                }
                SessionMessage::CompactionSummary(_) | SessionMessage::BranchSummary(_) => {
                    breakdown.summary += estimate_message_tokens(&message.to_llm());
                }
            }
        }
        (breakdown.total() > 0).then_some(breakdown)
    }
}

/// Where the session compacts itself, as Pi's threshold check decides it
/// for `model`: past its window less the reserve, while compaction is on.
fn compact_at(model: Option<&Model>, compaction: &CompactionSettings) -> Option<u64> {
    let model = model?;
    let settings = compaction.for_model(Some(model));
    (settings.enabled && model.context_window > 0)
        .then(|| model.context_window.saturating_sub(settings.reserve_tokens))
}

fn model_message(message: &SessionMessage) -> Option<&Message> {
    match message {
        SessionMessage::Llm(message) => Some(message),
        _ => None,
    }
}

/// What Pi's estimator charges for an image, whatever its size.
fn image_tokens() -> u64 {
    estimate_message_tokens(&Message::User(UserMessage {
        content: vec![Content::image(String::new(), String::new())],
        timestamp: 0,
    }))
}

fn image_count(content: &[Content]) -> u64 {
    content
        .iter()
        .filter(|block| matches!(block, Content::Image(_)))
        .count() as u64
}

/// The text of `content`, a block at a time.
fn text_tokens(content: &[Content]) -> u64 {
    content
        .iter()
        .filter_map(Content::as_text)
        .map(estimate_text_tokens)
        .sum()
}

/// `message`, which holds `content`, as the estimate of its text and how
/// many images it holds.
fn text_and_images(message: &Message, content: &[Content]) -> (u64, u64) {
    match image_count(content) {
        // Most hold no image; Pi's estimate of the whole message is exact.
        0 => (estimate_message_tokens(message), 0),
        images => (text_tokens(content), images),
    }
}

#[cfg(test)]
mod tests {
    use pi_ai::faux::{faux_message, faux_tool_call};
    use pi_ai::{
        AssistantContent, Content, Message, Model, Tool, ToolResultMessage, UserMessage,
        estimate_message_tokens, estimate_text_tokens, estimate_tool_tokens,
    };
    use pi_coding_agent::SessionMessage;
    use pi_coding_agent::compaction::{
        CompactionModelOverride, CompactionSettings, should_compact,
    };
    use pi_coding_agent::messages::{
        BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage, CustomMessage,
    };
    use serde_json::json;

    use super::AgentContextBreakdown;

    /// What Pi's request estimator charges for an image, whatever its size.
    const IMAGE: u64 = 1_372;

    fn model(context_window: u64) -> Model {
        serde_json::from_value(json!({
            "id": "glm-5-3", "name": "GLM 5.3", "api": "openai-completions",
            "provider": "maple", "contextWindow": context_window, "maxTokens": 8_000,
        }))
        .unwrap()
    }

    fn system(message: serde_json::Value) -> SessionMessage {
        SessionMessage::Llm(Message::System(serde_json::from_value(message).unwrap()))
    }

    fn user(content: Vec<Content>) -> SessionMessage {
        SessionMessage::Llm(Message::User(UserMessage {
            content,
            timestamp: 3,
        }))
    }

    fn tool_result(content: Vec<Content>) -> SessionMessage {
        SessionMessage::Llm(Message::ToolResult(ToolResultMessage {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            content,
            details: None,
            usage: None,
            is_error: false,
            timestamp: 5,
        }))
    }

    /// The user's `!ls`, or `!!ls`, which stays out of the model's context.
    fn shell(exclude_from_context: bool) -> SessionMessage {
        SessionMessage::BashExecution(BashExecutionMessage {
            command: "ls".into(),
            output: "main.rs".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 7,
            exclude_from_context,
        })
    }

    /// Pi's estimate of a message as the model reads it.
    fn estimate(message: &SessionMessage) -> u64 {
        estimate_message_tokens(&message.to_llm())
    }

    fn read_tool() -> Tool {
        Tool::new(
            "read",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )
    }

    #[test]
    fn each_part_of_a_compacted_task_lands_in_its_category() {
        // A compaction projects the prompt it kept, then its summary.
        let prompt = system(json!({"content": "You are Maple.", "toolsAdded": [read_tool()]}));
        let summary = SessionMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "Read main.rs and found the bug.".into(),
            tokens_before: 90_000,
            timestamp: 2,
        });
        let screenshot = user(vec![
            Content::text("What does this show?"),
            Content::image("iVBORw0KGgo=", "image/png"),
        ]);
        let reply = SessionMessage::Llm(Message::Assistant(faux_message(vec![
            AssistantContent::thinking("Look at the file first."),
            AssistantContent::text("Reading it."),
            faux_tool_call("read", json!({"path": "shot.png"})),
        ])));
        let image_result = tool_result(vec![
            Content::text("Read image file [image/png]"),
            Content::image("iVBORw0KGgo=", "image/png"),
        ]);
        let text_result = tool_result(vec![Content::text("fn main() {}")]);
        let notice = SessionMessage::Custom(CustomMessage {
            custom_type: "maple.notice".into(),
            content: vec![Content::text("Switched to GLM 5.3.")],
            display: true,
            details: None,
            timestamp: 8,
        });
        let messages = [
            prompt.clone(),
            summary.clone(),
            screenshot,
            reply.clone(),
            image_result,
            text_result.clone(),
            shell(false),
            shell(true),
            notice,
        ];

        let breakdown = AgentContextBreakdown::of_session(
            &messages,
            Some(&model(200_000)),
            &CompactionSettings::default(),
        )
        .expect("the context holds something");
        assert_eq!(
            breakdown,
            AgentContextBreakdown {
                instructions: estimate(&prompt),
                // With the framing the model reads it in.
                summary: estimate(&summary),
                messages: estimate_text_tokens("What does this show?")
                    + estimate(&reply)
                    + estimate_text_tokens("Switched to GLM 5.3."),
                tool_results: estimate_text_tokens("Read image file [image/png]")
                    + estimate(&text_result)
                    + estimate(&shell(false)),
                images: 2 * IMAGE,
                compact_at: Some(200_000 - CompactionSettings::default().reserve_tokens),
            }
        );
        assert!(breakdown.summary > estimate_text_tokens("Read main.rs and found the bug."));
        assert_eq!(
            breakdown.total(),
            breakdown.instructions
                + breakdown.summary
                + breakdown.messages
                + breakdown.tool_results
                + breakdown.images
        );

        // A branch summary is a summary too.
        let branch = SessionMessage::BranchSummary(BranchSummaryMessage {
            summary: "Tried another fix.".into(),
            from_id: "entry".into(),
            timestamp: 9,
        });
        let branched = AgentContextBreakdown::of_session(
            std::slice::from_ref(&branch),
            None,
            &CompactionSettings::default(),
        )
        .unwrap();
        assert_eq!(
            branched,
            AgentContextBreakdown {
                summary: estimate(&branch),
                ..AgentContextBreakdown::default()
            }
        );
    }

    #[test]
    fn the_prompt_and_tools_count_once_as_the_transcript_replays_them() {
        let bash = Tool::new("bash", "Run a command", json!({"type": "object"}));
        let grep = Tool::new("grep", "Search files", json!({"type": "object"}));
        let messages = [
            system(json!({
                "content": "You are Maple.",
                "sections": {"rules": "Use read."},
                "toolsAdded": [read_tool(), bash],
            })),
            user(vec![Content::text("Find the bug.")]),
            // A later declaration patches a section and swaps a tool.
            system(json!({
                "sections": {"rules": "Use grep."},
                "toolsAdded": [grep.clone()],
                "toolsRemoved": [{"name": "bash"}],
            })),
        ];
        let breakdown =
            AgentContextBreakdown::of_session(&messages, None, &CompactionSettings::default())
                .unwrap();
        assert_eq!(
            breakdown.instructions,
            estimate_text_tokens("You are Maple.\n\nUse grep.")
                + estimate_tool_tokens(&[read_tool(), grep])
        );
        assert_eq!(breakdown.messages, estimate_text_tokens("Find the bug."));
        assert_eq!(
            breakdown.compact_at, None,
            "without a model there is no window"
        );
    }

    #[test]
    fn compaction_starts_past_the_window_less_the_sessions_reserve() {
        let messages = [user(vec![Content::text("Hello")])];
        let at = |model: Option<&Model>, settings: &CompactionSettings| {
            AgentContextBreakdown::of_session(&messages, model, settings)
                .unwrap()
                .compact_at
        };
        let defaults = CompactionSettings::default();
        assert_eq!(
            at(Some(&model(200_000)), &defaults),
            Some(200_000 - defaults.reserve_tokens)
        );
        // A model's own reserve wins, as Pi resolves the session's settings.
        let mut tuned = defaults.clone();
        tuned.model_overrides.insert(
            "maple/glm-5-3".into(),
            CompactionModelOverride {
                reserve_tokens: Some(50_000),
                keep_recent_tokens: None,
            },
        );
        assert_eq!(at(Some(&model(200_000)), &tuned), Some(150_000));
        // Exactly where Pi's own check starts compacting.
        let resolved = tuned.for_model(Some(&model(200_000)));
        assert!(!should_compact(150_000, 200_000, &resolved));
        assert!(should_compact(150_001, 200_000, &resolved));
        // A window within the reserve compacts at any size, as Pi's check does.
        assert_eq!(at(Some(&model(10_000)), &defaults), Some(0));
        // Off, or with the window unknown, the task never compacts itself.
        let off = CompactionSettings {
            enabled: false,
            ..defaults.clone()
        };
        assert_eq!(at(Some(&model(200_000)), &off), None);
        assert_eq!(at(Some(&model(0)), &defaults), None);
        assert_eq!(at(None, &defaults), None);
    }

    #[test]
    fn a_context_with_nothing_the_model_reads_has_no_breakdown() {
        let settings = CompactionSettings::default();
        let model = model(200_000);
        assert_eq!(
            AgentContextBreakdown::of_session(&[], Some(&model), &settings),
            None
        );
        assert_eq!(
            AgentContextBreakdown::of_session(&[shell(true)], Some(&model), &settings),
            None
        );
    }

    /// Hosts and viewers of different builds read each other's breakdown:
    /// a part one leaves out reads as zero, a part it does not know is
    /// ignored, and no threshold is no field.
    #[test]
    fn a_breakdown_reads_across_builds() {
        let breakdown = AgentContextBreakdown {
            instructions: 1,
            summary: 2,
            messages: 3,
            tool_results: 4,
            images: 5,
            compact_at: Some(6),
        };
        let value = serde_json::to_value(breakdown).unwrap();
        assert_eq!(
            value,
            json!({
                "instructions": 1, "summary": 2, "messages": 3,
                "toolResults": 4, "images": 5, "compactAt": 6,
            })
        );
        assert_eq!(
            serde_json::from_value::<AgentContextBreakdown>(value).unwrap(),
            breakdown
        );
        let partial: AgentContextBreakdown =
            serde_json::from_value(json!({"messages": 3, "aLaterPart": 7})).unwrap();
        assert_eq!(
            partial,
            AgentContextBreakdown {
                messages: 3,
                ..AgentContextBreakdown::default()
            }
        );
        assert!(
            serde_json::to_value(partial)
                .unwrap()
                .get("compactAt")
                .is_none()
        );
    }
}
