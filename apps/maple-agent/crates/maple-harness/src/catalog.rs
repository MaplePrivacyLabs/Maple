//! The initial semantic action catalog, as data.
//!
//! The prototype's `app/src/harness/catalog.rs` (about 3,300 lines) declared
//! every descriptor next to its typed GPUI adapter and executor. That file
//! cannot survive without GPUI, but the *shape* of the catalog can: stable
//! dot-separated IDs, one family per application area, explicit effect and
//! authority per action, argument schemas that name stable identifiers (task
//! IDs, canonical roots, item IDs, setting keys) and never indices or entity
//! handles, and default Standard/Vim bindings declared on the descriptor.
//!
//! This module rebuilds a representative slice of section 11.1 of the design
//! document as a validated [`ActionRegistry`]. It is deliberately smaller
//! than the original (about forty actions instead of a few hundred) but it
//! covers every family and every authority class, so tests can prove the
//! contract rules: Human Only identity operations, the `app.quit` terminal
//! contract, the `permission.respond` Full Access contract, bindable actions
//! carrying adapters, and secret-bearing audit redaction.

use serde_json::{Value, json};

use crate::{
    ActionDescriptor, ActionEffect, ActionId, ActionRegistry, AuditSpec, DefaultBinding,
    InvocationPolicy, PreconditionDomainSelector, Recoverability, RegistryBuilder,
    RegistryValidationError, SCHEMA_VERSION, SemanticContextPattern, ShortcutProfile,
};

/// One row of the catalog table before it becomes a descriptor.
struct Row {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    category: &'static str,
    arguments: Value,
    effect: ActionEffect,
    policy: InvocationPolicy,
    recoverability: Recoverability,
    audit: AuditSpec,
    precondition: Option<PreconditionDomainSelector>,
    contexts: &'static [&'static str],
    bindings: Vec<(ShortcutProfile, &'static str, &'static str)>,
    terminal: bool,
}

const NO_ARGS: fn() -> Value = || json!({"type": "object", "additionalProperties": false});

fn args(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn optional_task_arg() -> Value {
    args(json!({"task_id": {"type": "string", "minLength": 1}}), &[])
}

fn task_arg() -> Value {
    args(
        json!({"task_id": {"type": "string", "minLength": 1}}),
        &["task_id"],
    )
}

fn rows() -> Vec<Row> {
    use ActionEffect::*;
    use InvocationPolicy::*;
    use Recoverability::*;
    let std = ShortcutProfile::Standard;
    let vim = ShortcutProfile::Vim;
    vec![
        // App / screens
        Row {
            id: "app.quit",
            label: "Quit Maple",
            description: "Commit an orderly shutdown after the accepted response is delivered.",
            category: "App",
            arguments: NO_ARGS(),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![(std, "MapleApp", "cmd-q")],
            terminal: true,
        },
        Row {
            id: "settings.open",
            label: "Open Settings",
            description: "Show the settings screen, parking the current chat.",
            category: "App",
            arguments: args(
                json!({"section": {"type": "string", "enum": ["account", "appearance", "shortcuts", "programmability", "mcp"]}}),
                &[],
            ),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["section"]).unwrap(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![
                (std, "MapleApp", "cmd-,"),
                (vim, "MapleApp && app_vim_mode == normal", "space s"),
            ],
            terminal: false,
        },
        Row {
            id: "settings.close",
            label: "Close Settings",
            description: "Return to the parked chat screen.",
            category: "App",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["MapleApp && screen == settings"],
            bindings: vec![(std, "MapleApp && screen == settings", "escape")],
            terminal: false,
        },
        Row {
            id: "shortcuts.open",
            label: "Open Shortcut Settings",
            description: "Open the shortcut profile editor.",
            category: "App",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![(std, "MapleApp", "cmd-k cmd-s")],
            terminal: false,
        },
        Row {
            id: "palette.open",
            label: "Command Palette",
            description: "Open the root command palette.",
            category: "App",
            arguments: args(json!({"query": {"type": "string", "maxLength": 256}}), &[]),
            effect: Navigate,
            policy: HumanOnly,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["query"]).unwrap(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![
                (std, "MapleApp", "cmd-shift-p"),
                (vim, "MapleApp && app_vim_mode == normal", ":"),
            ],
            terminal: false,
        },
        // Account / auth: Human Only
        Row {
            id: "account.sign_out",
            label: "Sign Out",
            description: "End the current Maple session on this device.",
            category: "Account",
            arguments: NO_ARGS(),
            effect: ExternalEffect,
            policy: HumanOnly,
            recoverability: Irreversible,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "account.delete",
            label: "Delete Account",
            description: "Begin the two-step account deletion flow.",
            category: "Account",
            arguments: NO_ARGS(),
            effect: ExternalEffect,
            policy: HumanOnly,
            recoverability: Irreversible,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["MapleApp && screen == settings"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "auth.submit_password",
            label: "Sign In",
            description: "Submit email and password on the login screen.",
            category: "Account",
            arguments: args(
                json!({"email": {"type": "string"}, "password": {"type": "string"}}),
                &["email", "password"],
            ),
            effect: ExternalEffect,
            policy: HumanOnly,
            recoverability: Irreversible,
            audit: AuditSpec::secret_bearing(),
            precondition: None,
            contexts: &["MapleApp && screen == login"],
            bindings: vec![],
            terminal: false,
        },
        // Tasks
        Row {
            id: "task.new",
            label: "New Task",
            description: "Create a task in the current project.",
            category: "Tasks",
            arguments: args(json!({"project_root": {"type": "string"}}), &[]),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["project_root"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Project),
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-n"),
                (vim, "Chat && app_vim_mode == normal", "space n"),
            ],
            terminal: false,
        },
        Row {
            id: "task.open",
            label: "Open Task",
            description: "Make a task the active conversation.",
            category: "Tasks",
            arguments: task_arg(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "task.rename",
            label: "Rename Task",
            description: "Set a task title.",
            category: "Tasks",
            arguments: args(
                json!({"task_id": {"type": "string"}, "title": {"type": "string", "maxLength": 200}}),
                &["task_id", "title"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "task.set_archived",
            label: "Archive Task",
            description: "Archive or unarchive a task; a setter, not a toggle.",
            category: "Tasks",
            arguments: args(
                json!({"task_id": {"type": "string"}, "archived": {"type": "boolean"}}),
                &["task_id", "archived"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id", "archived"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "task.focus_next",
            label: "Next Task",
            description: "Move the sidebar selection to the next task.",
            category: "Tasks",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Sidebar"],
            bindings: vec![
                (std, "Sidebar", "down"),
                (vim, "Sidebar && app_vim_mode == normal", "j"),
            ],
            terminal: false,
        },
        Row {
            id: "task.focus_previous",
            label: "Previous Task",
            description: "Move the sidebar selection to the previous task.",
            category: "Tasks",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Sidebar"],
            bindings: vec![
                (std, "Sidebar", "up"),
                (vim, "Sidebar && app_vim_mode == normal", "k"),
            ],
            terminal: false,
        },
        // Projects
        Row {
            id: "project.choose",
            label: "Open Project…",
            description: "Pick a project directory; with no path the executor opens the native picker.",
            category: "Projects",
            arguments: args(json!({"path": {"type": "string"}}), &[]),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["path"]).unwrap(),
            precondition: None,
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-o"),
                (vim, "Chat && app_vim_mode == normal", "space p"),
            ],
            terminal: false,
        },
        Row {
            id: "project.set_trusted",
            label: "Trust Project",
            description: "Grant or revoke tool trust for a project root.",
            category: "Projects",
            arguments: args(
                json!({"canonical_root": {"type": "string"}, "trusted": {"type": "boolean"}}),
                &["canonical_root", "trusted"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["canonical_root", "trusted"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Project),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "project.set_collapsed",
            label: "Collapse Project",
            description: "Collapse or expand a sidebar project group.",
            category: "Projects",
            arguments: args(
                json!({"canonical_root": {"type": "string"}, "collapsed": {"type": "boolean"}}),
                &["canonical_root", "collapsed"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["canonical_root", "collapsed"]).unwrap(),
            precondition: None,
            contexts: &["Sidebar"],
            bindings: vec![],
            terminal: false,
        },
        // Sidebar / transcript / regions
        Row {
            id: "sidebar.focus",
            label: "Focus Sidebar",
            description: "Move keyboard focus to the sidebar region.",
            category: "Navigation",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-1"),
                (vim, "Chat && app_vim_mode == normal", "g s"),
            ],
            terminal: false,
        },
        Row {
            id: "sidebar.set_collapsed",
            label: "Toggle Sidebar",
            description: "Show or hide the sidebar; a setter so callers state intent.",
            category: "Navigation",
            arguments: args(json!({"collapsed": {"type": "boolean"}}), &["collapsed"]),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["collapsed"]).unwrap(),
            precondition: None,
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "transcript.focus",
            label: "Focus Transcript",
            description: "Move keyboard focus to the transcript region.",
            category: "Navigation",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-2"),
                (vim, "Chat && app_vim_mode == normal", "g t"),
            ],
            terminal: false,
        },
        Row {
            id: "transcript.focus_next",
            label: "Next Message",
            description: "Select the next transcript item.",
            category: "Navigation",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Transcript"],
            bindings: vec![
                (std, "Transcript", "down"),
                (vim, "Transcript && app_vim_mode == normal", "j"),
            ],
            terminal: false,
        },
        Row {
            id: "transcript.focus_previous",
            label: "Previous Message",
            description: "Select the previous transcript item.",
            category: "Navigation",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Transcript"],
            bindings: vec![
                (std, "Transcript", "up"),
                (vim, "Transcript && app_vim_mode == normal", "k"),
            ],
            terminal: false,
        },
        Row {
            id: "timeline.copy_item",
            label: "Copy Message",
            description: "Copy one timeline item by stable ID.",
            category: "Transcript",
            arguments: args(
                json!({"task_id": {"type": "string"}, "item_id": {"type": "string"}}),
                &["task_id", "item_id"],
            ),
            effect: Observe,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["task_id", "item_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::TimelineItem),
            contexts: &["Transcript"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "timeline.copy_selected",
            label: "Copy Selected Message",
            description: "Copy the selected transcript item; a Human Only contextual alias that resolves to timeline.copy_item.",
            category: "Transcript",
            arguments: NO_ARGS(),
            effect: Observe,
            policy: HumanOnly,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Transcript"],
            bindings: vec![
                (std, "Transcript", "cmd-c"),
                (vim, "Transcript && app_vim_mode == normal", "y y"),
            ],
            terminal: false,
        },
        Row {
            id: "timeline.set_tool_expanded",
            label: "Expand Tool Call",
            description: "Expand or collapse a tool call card.",
            category: "Transcript",
            arguments: args(
                json!({"task_id": {"type": "string"}, "item_id": {"type": "string"}, "expanded": {"type": "boolean"}}),
                &["task_id", "item_id", "expanded"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::allow_fields(["task_id", "item_id", "expanded"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::TimelineItem),
            contexts: &["Transcript"],
            bindings: vec![],
            terminal: false,
        },
        // Composer
        Row {
            id: "composer.focus",
            label: "Focus Composer",
            description: "Move keyboard focus to the composer.",
            category: "Composer",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-3"),
                (vim, "Chat && app_vim_mode == normal", "i"),
            ],
            terminal: false,
        },
        Row {
            id: "composer.set_text",
            label: "Set Draft Text",
            description: "Replace the draft text for a task.",
            category: "Composer",
            arguments: args(
                json!({"task_id": {"type": "string"}, "text": {"type": "string", "maxLength": 200000}}),
                &["task_id", "text"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Draft),
            contexts: &["Composer"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "composer.send",
            label: "Send",
            description: "Send the current draft to the model.",
            category: "Composer",
            arguments: optional_task_arg(),
            effect: ExternalEffect,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Draft),
            contexts: &["Composer"],
            bindings: vec![(std, "Composer && input_role == composer", "enter")],
            terminal: false,
        },
        Row {
            id: "composer.attach_files",
            label: "Attach Files",
            description: "Attach files by path; with no paths the executor opens the native picker.",
            category: "Composer",
            arguments: args(
                json!({"task_id": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 32}}),
                &[],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Draft),
            contexts: &["Composer"],
            bindings: vec![(std, "Composer", "cmd-shift-a")],
            terminal: false,
        },
        Row {
            id: "composer.set_model",
            label: "Set Model",
            description: "Choose the model for a task.",
            category: "Composer",
            arguments: args(
                json!({"task_id": {"type": "string"}, "model": {"type": "string"}}),
                &["task_id", "model"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id", "model"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Composer"],
            bindings: vec![],
            terminal: false,
        },
        // Runs / queue
        Row {
            id: "run.stop",
            label: "Stop",
            description: "Stop the active model run for a task.",
            category: "Runs",
            arguments: optional_task_arg(),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![
                (std, "Chat", "cmd-."),
                (vim, "Chat && app_vim_mode == normal", "ctrl-c"),
            ],
            terminal: false,
        },
        Row {
            id: "queue.remove",
            label: "Remove Queued Message",
            description: "Remove one queued follow-up by queue ID.",
            category: "Runs",
            arguments: args(
                json!({"task_id": {"type": "string"}, "queue_id": {"type": "string"}}),
                &["task_id", "queue_id"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["task_id", "queue_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Target),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        // Questions / permissions
        Row {
            id: "question.submit",
            label: "Answer Question",
            description: "Submit answers to a model question by request ID.",
            category: "Requests",
            arguments: args(
                json!({"request_id": {"type": "string"}, "answers": {"type": "object"}}),
                &["request_id", "answers"],
            ),
            effect: ExternalEffect,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["request_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Target),
            contexts: &["Dialog && dialog == question"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "permission.respond",
            label: "Respond to Permission",
            description: "Allow or deny a tool permission request; an ordinary Full Access mutation by product decision.",
            category: "Requests",
            arguments: args(
                json!({"request_id": {"type": "string"}, "decision": {"type": "string", "enum": ["allow_once", "allow_always", "deny"]}}),
                &["request_id", "decision"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["request_id", "decision"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Target),
            contexts: &["Dialog && dialog == permission"],
            bindings: vec![],
            terminal: false,
        },
        // Settings
        Row {
            id: "settings.set_theme",
            label: "Set Theme",
            description: "Explicit setter for the theme preference.",
            category: "Settings",
            arguments: args(
                json!({"theme": {"type": "string", "enum": ["system", "light", "dark"]}}),
                &["theme"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["theme"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Global),
            contexts: &["MapleApp"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "settings.set_shortcut_profile",
            label: "Set Shortcut Profile",
            description: "Switch between the Standard and Vim profiles.",
            category: "Settings",
            arguments: args(
                json!({"profile": {"type": "string", "enum": ["standard", "vim"]}}),
                &["profile"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["profile"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Global),
            contexts: &["MapleApp"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "mcp.set_enabled",
            label: "Enable MCP Server",
            description: "Enable or disable one configured MCP server.",
            category: "Settings",
            arguments: args(
                json!({"server_id": {"type": "string"}, "enabled": {"type": "boolean"}}),
                &["server_id", "enabled"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["server_id", "enabled"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Global),
            contexts: &["MapleApp && screen == settings"],
            bindings: vec![],
            terminal: false,
        },
        // Utilities
        Row {
            id: "ui.dismiss",
            label: "Dismiss",
            description: "Close the topmost overlay, menu, or dialog.",
            category: "Utilities",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Overlay", "Dialog"],
            bindings: vec![(std, "Overlay || Dialog", "escape")],
            terminal: false,
        },
        Row {
            id: "ui.activate_selected",
            label: "Activate Selection",
            description: "Activate the selected semantic object; resolves to a concrete action that is re-authorized.",
            category: "Utilities",
            arguments: NO_ARGS(),
            effect: Navigate,
            policy: ControllerCallable,
            recoverability: Ephemeral,
            audit: AuditSpec::redact_all(),
            precondition: None,
            contexts: &["Chat", "Settings"],
            bindings: vec![
                (std, "Sidebar || Transcript || Settings", "enter"),
                (
                    vim,
                    "(Sidebar || Transcript || Settings) && app_vim_mode == normal",
                    "enter",
                ),
            ],
            terminal: false,
        },
        Row {
            id: "link.open",
            label: "Open Link",
            description: "Open a URL in the system browser.",
            category: "Utilities",
            arguments: args(
                json!({"url": {"type": "string", "format": "uri"}}),
                &["url"],
            ),
            effect: ExternalEffect,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["url"]).unwrap(),
            precondition: None,
            contexts: &["MapleApp"],
            bindings: vec![],
            terminal: false,
        },
        // Code Mode: authority changes are Human Only, execution is ordinary
        Row {
            id: "code_mode.set_enabled",
            label: "Set Python Code Mode",
            description: "Turn the Developer Preview Code Mode on or off.",
            category: "Code Mode",
            arguments: args(json!({"enabled": {"type": "boolean"}}), &["enabled"]),
            effect: MutateMaple,
            policy: HumanOnly,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["enabled"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Global),
            contexts: &["MapleApp && screen == settings"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "code_mode.set_controller_access",
            label: "Set UI Controller Access",
            description: "Set the model/Python controller mode: Off, Read Only, or Full Access.",
            category: "Code Mode",
            arguments: args(
                json!({"access": {"type": "string", "enum": ["off", "read_only", "full_access"]}}),
                &["access"],
            ),
            effect: MutateMaple,
            policy: HumanOnly,
            recoverability: Reversible,
            audit: AuditSpec::allow_fields(["access"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Global),
            contexts: &["MapleApp && screen == settings"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "code_mode.execute",
            label: "Run Python",
            description: "Execute source in the task's persistent kernel.",
            category: "Code Mode",
            arguments: args(
                json!({"task_id": {"type": "string"}, "source": {"type": "string", "maxLength": 262144}}),
                &["task_id", "source"],
            ),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
        Row {
            id: "code_mode.stop",
            label: "Stop Python",
            description: "Cancel the running execution and revoke its controller lease.",
            category: "Code Mode",
            arguments: task_arg(),
            effect: MutateMaple,
            policy: ControllerCallable,
            recoverability: Irreversible,
            audit: AuditSpec::allow_fields(["task_id"]).unwrap(),
            precondition: Some(PreconditionDomainSelector::Task),
            contexts: &["Chat"],
            bindings: vec![],
            terminal: false,
        },
    ]
}

fn descriptor(row: Row) -> ActionDescriptor {
    let bindable = !row.bindings.is_empty();
    ActionDescriptor {
        schema_version: SCHEMA_VERSION,
        id: ActionId::parse(row.id).expect("catalog IDs follow the grammar"),
        label: row.label.to_owned(),
        description: row.description.to_owned(),
        category: row.category.to_owned(),
        argument_schema: row.arguments,
        result_schema: json!({"type": "object"}),
        contexts: row
            .contexts
            .iter()
            .map(|c| SemanticContextPattern::parse(*c).expect("catalog contexts are valid"))
            .collect(),
        effect: row.effect,
        invocation_policy: row.policy,
        recoverability: row.recoverability,
        audit: row.audit,
        default_bindings: row
            .bindings
            .iter()
            .map(|(profile, context, sequence)| DefaultBinding {
                profile: *profile,
                context: (*context).to_owned(),
                sequence: (*sequence).to_owned(),
                arguments: json!({}),
            })
            .collect(),
        precondition_domain: row.precondition,
        bindable,
        terminal_host_action: row.terminal,
    }
}

/// All descriptors of the initial catalog, unvalidated.
pub fn initial_descriptors() -> Vec<ActionDescriptor> {
    rows().into_iter().map(descriptor).collect()
}

/// The initial catalog as a validated registry. Every bindable action is
/// registered with an adapter, standing in for the one typed GPUI adapter the
/// application must provide per bindable action.
pub fn initial_registry() -> Result<ActionRegistry, Vec<RegistryValidationError>> {
    let mut builder = RegistryBuilder::new();
    for descriptor in initial_descriptors() {
        if descriptor.bindable {
            builder.register_adapter(descriptor.id.clone());
        }
        builder.register(descriptor);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn initial_catalog_validates() {
        let registry = initial_registry().unwrap_or_else(|errors| panic!("{errors:#?}"));
        assert!(registry.len() >= 40);
    }

    #[test]
    fn every_family_in_section_11_1_is_represented() {
        let registry = initial_registry().unwrap();
        let families: BTreeSet<&str> = registry
            .iter()
            .map(|(id, _)| id.as_str().split('.').next().unwrap())
            .collect();
        for family in [
            "app",
            "settings",
            "shortcuts",
            "account",
            "auth",
            "task",
            "project",
            "sidebar",
            "transcript",
            "timeline",
            "composer",
            "run",
            "queue",
            "question",
            "permission",
            "mcp",
            "ui",
            "link",
            "code_mode",
        ] {
            assert!(families.contains(family), "missing family {family}");
        }
    }

    #[test]
    fn identity_and_authority_actions_are_human_only() {
        let registry = initial_registry().unwrap();
        for id in [
            "account.sign_out",
            "account.delete",
            "auth.submit_password",
            "code_mode.set_enabled",
            "code_mode.set_controller_access",
        ] {
            let descriptor = registry.descriptor(&ActionId::parse(id).unwrap()).unwrap();
            assert_eq!(
                descriptor.invocation_policy,
                InvocationPolicy::HumanOnly,
                "{id}"
            );
        }
        let respond = registry
            .descriptor(&ActionId::parse("permission.respond").unwrap())
            .unwrap();
        assert_eq!(
            respond.invocation_policy,
            InvocationPolicy::ControllerCallable
        );
        let quit = registry
            .descriptor(&ActionId::parse("app.quit").unwrap())
            .unwrap();
        assert!(quit.terminal_host_action);
    }

    #[test]
    fn secret_bearing_actions_redact_everything() {
        let registry = initial_registry().unwrap();
        let login = registry
            .descriptor(&ActionId::parse("auth.submit_password").unwrap())
            .unwrap();
        let redacted = login
            .audit
            .redact(&json!({"email": "a@b", "password": "hunter2"}));
        assert!(redacted.is_empty());
        assert_eq!(redacted.redacted_field_count(), 2);
    }

    #[test]
    fn arguments_use_stable_identifiers_not_indices() {
        for descriptor in initial_descriptors() {
            let properties = descriptor.argument_schema["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for key in properties.keys() {
                assert!(
                    !key.ends_with("_index") && !key.ends_with("_ix"),
                    "{}: {key}",
                    descriptor.id
                );
            }
        }
    }

    #[test]
    fn bindable_actions_have_adapters_and_vice_versa() {
        let registry = initial_registry().unwrap();
        for (id, descriptor) in registry.iter() {
            assert_eq!(descriptor.bindable, registry.has_adapter(id), "{id}");
        }
    }
}
