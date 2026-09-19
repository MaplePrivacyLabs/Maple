---
name: handoff
description: Hand a self-contained piece of work to an external coding agent (Codex or Claude Code) that runs in this project with its own context. Use when the user asks to delegate, hand off, or have Codex or Claude Code implement something.
metadata:
  maple: external-agents
  argument-hint: "<what to hand off>"
---

# Hand work to an external agent

Maple can start an external coding agent (Codex or Claude Code) inside this project.
The agent runs with its own context and its own account. It does not see
this conversation. It runs under its own sandbox and approval settings;
whatever it asks approval for comes to the user through Maple.

## Steps

1. Call `list_agent_providers` first. If no provider is usable, tell the
   user what is missing (install, PATH, or the provider’s sign-in command) and stop.
2. Write a self-contained briefing. The agent has zero context, so the
   briefing must carry everything:
   - **Task**: what to do, in one or two sentences.
   - **Context**: why, and what the project is.
   - **Relevant files**: paths the agent should read first.
   - **Current state**: what already works, what is broken.
   - **What was tried**: dead ends to skip.
   - **Decisions**: choices already made that it must not revisit.
   - **Acceptance criteria**: how to know it is done, including tests to run.
   - **Constraints**: what it must not touch or change.
3. Call `agent_start` with `provider`, the briefing as `prompt`, and
   `background: true` unless the user is waiting on the result right now.
4. Do not poll `agent_status`. Maple tells you when the agent finishes, in
   this turn or the next. Continue with other work meanwhile.
5. When Maple reports the end, call `agent_status` once, read the changed
   files yourself, and verify the acceptance criteria before you rely on
   them. If more is needed, call `agent_send` with the same `agent_id` so
   the agent keeps its context.

Agents take time. Ten to thirty minutes is routine for a real task.
