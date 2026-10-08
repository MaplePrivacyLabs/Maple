//! System prompt for Agent Mode: harness instructions plus goose's body.
//!
//! The prompt has two parts. The harness that hosts the agent supplies the
//! first part, the *harness instructions*: who the agent is and how it
//! behaves. The host passes its own text through `MapleAgentHostResources`
//! (the Maple desktop app says the agent is Maple); an ACP client such as
//! Buzz passes the system prompt it sends with the task.
//! The second part, [`AGENT_OPERATING_PROMPT`], is the same for every host:
//! a copy of the pinned goose `crates/goose/src/prompts/system.md` with its
//! two-line "goose, created by AAIF" identity header removed. Every dynamic
//! section (turn context, extensions, response guidelines) is preserved
//! byte-for-byte so the rendered prompt keeps goose's exact structure and
//! prompt-cache stability.
//!
//! When bumping the goose pin in `Cargo.toml`, diff the body against the
//! pinned `system.md`; only the first two lines may differ.
//! `body_matches_pinned_goose_system_prompt_byte_for_byte` compares the body
//! against goose's compile-time-embedded stock `system.md` and fails on any
//! drift.

/// Build the system prompt template for goose's `Agent::override_system_prompt`.
///
/// `harness_instructions` opens the prompt verbatim (trimmed), followed by
/// [`AGENT_OPERATING_PROMPT`]. Goose renders the result through its template
/// engine, so template syntax in the instructions is neutralized: a persona
/// text cannot become template code.
pub(crate) fn system_prompt(harness_instructions: &str) -> String {
    let harness_instructions = harness_instructions
        .trim()
        .replace("{{", "{ {")
        .replace("{%", "{ %");
    format!("{harness_instructions}\n{AGENT_OPERATING_PROMPT}")
}

/// Goose's stock system prompt without its identity header. Starts with the
/// blank line that separates the header from the turn-context block.
pub(crate) const AGENT_OPERATING_PROMPT: &str = r#"
{% if moim_system_prompt_block is defined %}
{{ moim_system_prompt_block }}
{% endif %}

{% if include_extensions and not code_execution_mode %}

# Extensions

Extensions provide additional tools and context from different data sources and applications.
You can dynamically enable or disable extensions as needed to help complete tasks.

{% if (extensions is defined) and extensions %}
Because you dynamically load extensions, your conversation history may refer
to interactions with extensions that are not currently active. The currently
active extensions are below. Each of these extensions provides tools that are
in your tool specification.

{% for extension in extensions %}

## {{extension.name}}

{% if extension.has_resources %}
{{extension.name}} supports resources.
{% endif %}
{% if extension.instructions %}### Instructions
{{extension.instructions}}{% endif %}
{% endfor %}

{% else %}
No extensions are defined. You should let the user know that they should add extensions.
{% endif %}
{% endif %}

# Response Guidelines

Use Markdown formatting for all responses.
"#;

#[cfg(test)]
mod tests {
    use super::{AGENT_OPERATING_PROMPT, system_prompt};

    const MAPLE_HARNESS_INSTRUCTIONS: &str =
        "You are a general-purpose AI agent called Maple, created by Maple AI.
You run in the Maple app's Agent Mode; users know you simply as Maple.";
    use goose::agents::prompt_manager::PromptManager;
    use goose::prompt_template::get_template;

    #[test]
    fn harness_instructions_open_the_prompt_and_the_body_follows_unchanged() {
        let template = system_prompt("  <system>You are Buzzbot. Reply in ALL CAPS.</system>  ");
        assert!(template.starts_with("<system>You are Buzzbot. Reply in ALL CAPS.</system>\n\n"));
        assert!(template.ends_with(AGENT_OPERATING_PROMPT));
        assert!(
            !template.contains("Maple"),
            "a caller's prompt must not carry the Maple identity"
        );
    }

    #[test]
    fn template_syntax_in_harness_instructions_is_neutralized() {
        let mut manager = PromptManager::new();
        manager.set_system_prompt_override(system_prompt("Say {{ hello }} and {% if x %}"));
        let rendered = manager.builder().build();
        assert!(rendered.starts_with("Say { { hello }} and { % if x %}"));
    }

    #[test]
    fn renders_maple_identity_with_stock_structure() {
        // Render through goose's real PromptManager so a template that
        // upstream's mini template engine rejects fails loudly here.
        let mut manager = PromptManager::new();
        manager.set_system_prompt_override(system_prompt(MAPLE_HARNESS_INSTRUCTIONS));
        let rendered = manager.builder().build();

        assert!(
            rendered.starts_with(
                "You are a general-purpose AI agent called Maple, created by Maple AI.\n\
                 You run in the Maple app's Agent Mode; users know you simply as Maple.\n\n"
            ),
            "rendered prompt lost the Maple identity header: {rendered}"
        );
        for section in [
            "# Turn Context",
            "# Extensions",
            "# Response Guidelines",
            "Use Markdown formatting for all responses.",
        ] {
            assert!(
                rendered.contains(section),
                "rendered prompt lost stock goose section {section:?}: {rendered}"
            );
        }
    }

    /// Everything after goose's two-line identity header.
    fn stock_body(template: &str) -> &str {
        template
            .splitn(3, '\n')
            .nth(2)
            .expect("template must have a two-line identity header plus a body")
    }

    #[test]
    fn body_matches_pinned_goose_system_prompt_byte_for_byte() {
        // goose's get_template embeds the prompts directory at compile time,
        // so default_content is the exact pinned upstream system.md and is
        // unaffected by any on-disk user prompt override.
        let stock = get_template("system.md")
            .expect("the pinned goose crate ships system.md")
            .default_content;
        assert_eq!(
            AGENT_OPERATING_PROMPT,
            stock_body(&stock),
            "the operating prompt drifted from the pinned goose system.md; \
             re-copy the body below goose's two-line identity header"
        );
    }

    #[test]
    fn operating_prompt_has_no_upstream_identity() {
        // The goose turn-context block (which mentions goose machinery) comes
        // from goose at render time and is intentionally out of scope here. The
        // stock template variable `moim_system_prompt_block` must stay intact,
        // so blank it out before scanning for the "Block" creator brand.
        let template = AGENT_OPERATING_PROMPT.replace("moim_system_prompt_block", "turnctx");
        let template = template.to_lowercase();
        for leak in ["goose", "aaif", "block", "maple"] {
            assert!(
                !template.contains(leak),
                "operating prompt carries an identity {leak:?}"
            );
        }
    }
}
