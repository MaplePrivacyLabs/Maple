---
name: advisor
description: Ask an external agent (Codex or Claude Code) for read-only analysis or review of code, a plan, or a problem, without letting it change anything. Use when the user wants a review, an audit, or advice from another model.
metadata:
  maple: external-agents
  argument-hint: "<what to review or ask about>"
---

# Ask an advisor

An advisor reads and reasons; it never changes the project. Use it for a
code review, a design check, a bug hunt, or a second opinion on a plan.

## Steps

1. Call `list_agent_providers`. If no provider is usable, tell the user and
   do the review yourself.
2. Write a self-contained briefing: the question, the files to read, the
   constraints, and the shape of answer you want. End it with:
   "This is analysis only. Do NOT edit, create, or delete any files."
3. Call `agent_start` with `provider` and the briefing. Use blocking mode
   when the user is waiting on the answer; use `background: true` when you
   have other work to do first.
4. When the result arrives, read it critically. Verify claims against the
   code before you pass them on, and say which points you checked.
5. For follow-up questions, call `agent_send` with the same `agent_id` so
   the advisor keeps its context.
