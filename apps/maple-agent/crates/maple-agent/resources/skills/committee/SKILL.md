---
name: committee
description: Get several independent opinions on a question or design by asking external agents (Codex) and comparing them with your own analysis. Use when the user wants a second opinion, a review from another model, or a comparison of approaches.
metadata:
  maple: external-agents
  argument-hint: "<question or design to review>"
---

# Convene a committee

A committee gives the user independent views on one question, then a
comparison. Each member works from the same briefing and none of them sees
the others' answers.

## Steps

1. Call `list_agent_providers`. If no provider is usable, do the analysis
   yourself and say that no external members were available.
2. Write one briefing that states the question, the relevant files, the
   constraints, and the exact output you want (for example: a ranked list of
   options with trade-offs, or a verdict with reasons). End it with:
   "This is analysis only. Do NOT edit, create, or delete any files."
3. Start two or three members with `agent_start`, `background: true`, and
   the same briefing. Give each a different `model` or `effort` when the user
   has not asked for a specific one, so their views differ.
4. Do your own analysis while they work. Do not poll `agent_status`; Maple
   tells you when each member finishes.
5. When every member has reported, call `agent_status` for each, then give
   the user a comparison: where the members agree, where they disagree and
   why, and your recommendation. Attribute each view to its member.
