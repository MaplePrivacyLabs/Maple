//! Presentation of host-owned timeline semantics. Unknown and legacy values
//! deliberately return None so callers retain the original raw content.
use gpui::SharedString;
use maple_agent::agent::AgentTimelineItem;

use super::i18n::{MessageCache, tr};

pub(crate) fn title(item: &AgentTimelineItem, cache: &MessageCache) -> Option<SharedString> {
    let label = item.label.as_ref()?;
    Some(match label.kind.as_str() {
        "error-authentication" => tr("runtime-timeline-error-authentication"),
        "error-credits" => tr("runtime-timeline-error-credits"),
        "error-context" => tr("runtime-timeline-error-context"),
        "error-agent" => tr("runtime-timeline-error-agent"),
        "error-usage" => tr("runtime-timeline-error-usage"),
        "thinking" => tr("runtime-timeline-thinking"),
        "notice" | "stopped" | "model-switched" => tr("runtime-timeline-notice"),
        "compacted" => tr("runtime-timeline-compacted"),
        "branch-summary" => tr("runtime-timeline-branch-summary"),
        "tool" => {
            if label.name.as_deref() == Some("load_skill") {
                let name = label.detail.as_ref()?;
                let key = match item.status.as_deref() {
                    Some("completed") => "runtime-timeline-skill-loaded",
                    Some("failed") => "runtime-timeline-skill-failed",
                    _ => "runtime-timeline-skill-loading",
                };
                return Some(cache.get(key, [("name", name.clone().into())]));
            }
            let name = tool_label(item, cache).or_else(|| {
                Some(match label.name.as_deref()? {
                    "read" => tr("runtime-timeline-builtin-read"),
                    "edit" => tr("runtime-timeline-builtin-edit"),
                    "write" => tr("runtime-timeline-builtin-write"),
                    "agent_start" => tr("runtime-timeline-agent-start"),
                    "agent_send" => tr("runtime-timeline-agent-send"),
                    "agent_status" => tr("runtime-timeline-agent-status"),
                    "agent_cancel" => tr("runtime-timeline-agent-cancel"),
                    "list_agent_providers" => tr("runtime-timeline-agent-providers"),
                    _ => return None,
                })
            })?;
            match &label.detail {
                Some(detail) => cache.get(
                    "runtime-timeline-tool-detail",
                    [
                        ("name", name.to_string().into()),
                        ("detail", detail.clone().into()),
                    ],
                ),
                None => name,
            }
        }
        _ => return None,
    })
}

pub(crate) fn body(item: &AgentTimelineItem, cache: &MessageCache) -> Option<SharedString> {
    let label = item.label.as_ref()?;
    if matches!(
        label.kind.as_str(),
        "error-agent" | "error-authentication" | "error-credits" | "error-context"
    ) {
        let key = match label.name.as_deref() {
            Some("clock") => Some("runtime-provider-clock"),
            Some("authentication") => Some("runtime-provider-authentication"),
            Some("credits") => Some("runtime-provider-credits"),
            Some("context") => Some("runtime-provider-context"),
            Some("rate-limit") => Some("runtime-provider-rate-limit"),
            Some("endpoint") => Some("runtime-provider-endpoint"),
            Some("timeout") => Some("runtime-provider-timeout"),
            Some("connect") => Some("runtime-provider-connect"),
            Some("network") => Some("runtime-provider-network"),
            Some("stream-timeout") => Some("runtime-provider-stream-timeout"),
            Some("stream") => Some("runtime-provider-stream"),
            Some("attestation") => Some("runtime-provider-attestation"),
            Some("secure-connection") => Some("runtime-provider-secure-connection"),
            Some("prepare") => Some("runtime-provider-prepare"),
            Some("cancelled") => Some("runtime-provider-cancelled"),
            _ => None,
        };
        if let Some(key) = key {
            return Some(tr(key));
        }
    }
    Some(match item.label.as_ref()?.kind.as_str() {
        "external-undelivered" => tr("runtime-timeline-external-undelivered"),
        "external-completed" | "external-failed" | "external-cancelled" => {
            let label = item.label.as_ref()?;
            let key = match label.kind.as_str() {
                "external-completed" => "runtime-timeline-external-completed",
                "external-failed" => "runtime-timeline-external-failed",
                _ => "runtime-timeline-external-cancelled",
            };
            cache.get(
                key,
                [
                    ("name", label.name.clone()?.into()),
                    ("provider", label.detail.clone()?.into()),
                ],
            )
        }
        "error-usage" => tr("runtime-timeline-error-usage-body"),
        "stopped" => tr("runtime-timeline-stopped"),
        "model-switched" => cache.get(
            "runtime-timeline-model-switched",
            [("name", item.label.as_ref()?.name.clone()?.into())],
        ),
        "compacted" => tr("runtime-timeline-compacted-body"),
        _ => return None,
    })
}

pub(crate) fn tool_label(item: &AgentTimelineItem, _cache: &MessageCache) -> Option<SharedString> {
    let label = item.label.as_ref()?;
    if label.kind != "tool" {
        return None;
    }
    // Exact first-party identifiers only: an MCP suffix cannot impersonate one.
    Some(match label.name.as_deref()? {
        "bash" | "powershell" | "shell" => tr("runtime-timeline-terminal"),
        "text_editor" | "str_replace_editor" | "str_replace_based_edit_tool" => {
            tr("runtime-timeline-editor")
        }
        "web_search" => tr("runtime-timeline-web-search"),
        "read_file" => tr("runtime-timeline-read-file"),
        "write_file" => tr("runtime-timeline-write-file"),
        "list_files" => tr("runtime-timeline-list-files"),
        "glob" => tr("runtime-timeline-find-files"),
        "grep" => tr("runtime-timeline-search"),
        _ => return None,
    })
}
