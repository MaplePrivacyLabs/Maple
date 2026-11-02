//! Deterministic synthetic data adapted from code-docs' bench-engines generator.
//! Pure synthetic text, linear Pi trees, real tool calls/results and compactions.

use pi_ai::{
    AssistantContent, AssistantMessage, Content, Message, StopReason, ThinkingLevel, ToolCall,
    ToolResultMessage, Usage, UserMessage,
};
use pi_coding_agent::{
    SessionMessage,
    session::{EntryKind, SessionEntry},
};
use serde::Serialize;
use serde_json::{Map, json};

pub const SEED: u64 = 0x6d61_706c_655f_6167;
pub const BASE_MS: i64 = 1_760_000_000_000;

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Scale {
    pub tasks: usize,
    pub entries: usize,
    pub hot_entries: usize,
    pub long_entries: usize,
}
impl Scale {
    pub fn full() -> Self {
        Self {
            tasks: 2000,
            entries: 1_000_000,
            hot_entries: 100_000,
            long_entries: 20_000,
        }
    }
    pub fn ci() -> Self {
        Self {
            tasks: 40,
            entries: 8000,
            hot_entries: 2000,
            long_entries: 1000,
        }
    }
    pub fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.tasks >= 2 && self.tasks <= 100_000,
            "tasks must be 2..100000"
        );
        anyhow::ensure!(
            self.entries >= self.hot_entries + self.long_entries + (self.tasks - 2) * 2,
            "not enough entries for requested task counts"
        );
        anyhow::ensure!(
            self.hot_entries >= 2 && self.long_entries >= 2,
            "hot and long tasks require at least two entries"
        );
        anyhow::ensure!(
            self.tasks > 2 || self.entries == self.hot_entries + self.long_entries,
            "two-task scale must allocate all entries to hot and long tasks"
        );
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub index: usize,
    pub uuid: String,
    pub entries: usize,
    pub created_ms: i64,
    pub gap_ms: i64,
}
impl TaskSpec {
    pub fn timestamp(&self, seq: usize) -> i64 {
        self.created_ms + seq as i64 * self.gap_ms
    }
}

pub fn tasks(scale: Scale) -> Vec<TaskSpec> {
    let mut rng = Rng::new(SEED);
    let mut counts = vec![2; scale.tasks];
    counts[0] = scale.hot_entries;
    counts[1] = scale.long_entries;
    let remaining = scale.entries - counts.iter().sum::<usize>();
    if scale.tasks > 2 {
        let weights = (2..scale.tasks)
            .map(|_| 1 + (rng.next() % 1000) as usize)
            .collect::<Vec<_>>();
        let total = weights.iter().sum::<usize>();
        let mut allocated = 0;
        for (index, weight) in weights.iter().enumerate() {
            let count = remaining * weight / total;
            counts[index + 2] += count;
            allocated += count;
        }
        for index in 0..remaining - allocated {
            counts[2 + index % (scale.tasks - 2)] += 1;
        }
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(index, entries)| {
            let created_ms = BASE_MS + ((rng.next() % 365) as i64) * 86_400_000;
            TaskSpec {
                index,
                uuid: format!("018ddbcc-b80b-7c20-8123-{index:012x}"),
                entries,
                created_ms,
                gap_ms: (86_400_000 / entries as i64).max(10),
            }
        })
        .collect()
}

pub fn insertion_order(tasks: &[TaskSpec]) -> Vec<(usize, usize)> {
    let mut order = tasks
        .iter()
        .flat_map(|task| (0..task.entries).map(move |seq| (task.timestamp(seq), task.index, seq)))
        .collect::<Vec<_>>();
    order.sort_unstable();
    order
        .into_iter()
        .map(|(_, task, seq)| (task, seq))
        .collect()
}

pub fn text(task: usize, seq: usize, len: usize) -> String {
    const WORDS:&[u8]=b"Maple synthetic agent context function result encrypted SQLite transaction tool file workspace test code message " ;
    let mut rng = Rng::new(SEED ^ (task as u64).wrapping_mul(0x9e37_79b9) ^ seq as u64);
    let offset = (rng.next() as usize) % WORDS.len();
    (0..len)
        .map(|index| WORDS[(index + offset) % WORDS.len()] as char)
        .collect()
}

pub fn incompressible_text(len: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/";
    let mut rng = Rng::new(SEED ^ 0x20_000_000);
    (0..len)
        .map(|_| alphabet[(rng.next() % 64) as usize] as char)
        .collect()
}

pub fn entry(task: &TaskSpec, seq: usize) -> SessionEntry {
    let timestamp = task.timestamp(seq);
    let id = format!("{seq:08x}");
    let parent_id = seq.checked_sub(1).map(|seq| format!("{seq:08x}"));
    let bytes = 256 + ((task.index.wrapping_mul(997) + seq.wrapping_mul(7919)) % 3000);
    let kind = if seq % 512 == 511 {
        EntryKind::Compaction {
            summary: text(task.index, seq, 800),
            first_kept_entry_id: format!("{:08x}", seq - 32),
            tokens_before: 100_000,
            details: None,
            usage: None,
            from_extension: false,
            system_message: None,
        }
    } else if seq % 127 == 126 {
        EntryKind::ThinkingLevelChange {
            thinking_level: ThinkingLevel::Medium,
        }
    } else {
        EntryKind::Message {
            message: SessionMessage::Llm(match seq % 6 {
                0 | 4 => Message::User(UserMessage {
                    content: vec![Content::text(text(task.index, seq, bytes))],
                    timestamp,
                }),
                2 => Message::ToolResult(ToolResultMessage {
                    tool_call_id: format!("call-{}", seq - 1),
                    tool_name: "read".into(),
                    content: vec![Content::text(text(
                        task.index,
                        seq,
                        if seq % 97 == 2 { 50_000 } else { bytes },
                    ))],
                    details: None,
                    usage: None,
                    is_error: false,
                    timestamp,
                }),
                _ => {
                    let mut content = vec![AssistantContent::text(text(task.index, seq, bytes))];
                    if seq % 6 == 1 {
                        let arguments = if seq % 61 == 1 {
                            json!({"todos":[{"content":"Review synthetic store","status":"in_progress"}]})
                        } else {
                            json!({"path":format!("src/module_{}.rs",seq%40),"offset":seq})
                        };
                        content.push(AssistantContent::ToolCall(ToolCall {
                            id: format!("call-{seq}"),
                            name: (if seq % 61 == 1 { "todo_write" } else { "read" }).into(),
                            arguments: arguments.as_object().cloned().unwrap_or_else(Map::new),
                            arguments_error: None,
                            partial_json: None,
                        }));
                    }
                    Message::Assistant(AssistantMessage {
                        content,
                        api: "openai-completions".into(),
                        provider: "synthetic".into(),
                        model: "fixture-model".into(),
                        response_id: None,
                        thinking_level: Some(ThinkingLevel::Medium),
                        usage: Usage::default(),
                        stop_reason: StopReason::Stop,
                        error_message: None,
                        timestamp,
                    })
                }
            }),
        }
    };
    SessionEntry {
        id,
        parent_id,
        timestamp,
        kind,
    }
}

pub fn oversized_entry(task: &TaskSpec, seq: usize, len: usize) -> SessionEntry {
    let mut entry = entry(task, seq);
    let arguments = json!({"path":"synthetic-large.txt","content":incompressible_text(len)});
    entry.kind = EntryKind::Message {
        message: SessionMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: format!("oversized-{seq}"),
                name: "write".into(),
                arguments: arguments.as_object().unwrap().clone(),
                arguments_error: None,
                partial_json: None,
            })],
            api: "openai-completions".into(),
            provider: "synthetic".into(),
            model: "fixture-model".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: entry.timestamp,
        })),
    };
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scale_is_exact_and_order_preserves_each_linear_tree() {
        let specs = tasks(Scale::ci());
        assert_eq!(
            specs.iter().map(|task| task.entries).sum::<usize>(),
            Scale::ci().entries
        );
        let mut next = vec![0; specs.len()];
        for (task, seq) in insertion_order(&specs) {
            assert_eq!(seq, next[task]);
            next[task] += 1;
            let value = entry(&specs[task], seq);
            assert_eq!(
                serde_json::from_slice::<SessionEntry>(&serde_json::to_vec(&value).unwrap())
                    .unwrap(),
                value
            );
        }
    }
}
