# Scenario checklist

One scenario per user-facing feature of Maple Agent. Run it before and after a
change to the Agent's runtime. Scenarios the runtime tests cover are marked
**automated** with the test that covers them; the rest are run by hand and the
result recorded in the table at the end.

Setup: a signed-in account on a local or development stack, a project folder
with a few files, the built-in computer-use backend set up (macOS or Linux),
Codex and Claude Code installed and signed in on the machine, and one custom
stdio MCP server configured in Settings > Integrations.

## Tasks

| # | Scenario | How | Pass when |
|---|---|---|---|
| T1 | Automatic then semantic title | Start a task with a one-line prompt | The sidebar shows a generated title at once and a semantic one after the first reply |
| T2 | Rename | Rename the task from its sidebar menu | The new name survives a relaunch |
| T3 | Settle and archive | Settle a finished task, archive it, then restore it | It moves between Active, Settled and Archived and comes back with its history |
| T4 | Delete | Delete a task | It disappears from every list and its folder of attachments is gone |
| T5 | Pin | `/pin` a task, then unpin it | The pin shows in the sidebar and clears |
| T6 | Projects and recent folders | Create a task in a second folder | The project switcher lists both roots; the task runs in its own root |

## Composer

| # | Scenario | How | Pass when |
|---|---|---|---|
| C1 | Queued follow-up | Press Enter with a message while a run is active | The message waits in the queue and runs as the next turn (**automated**: `desktop_send_during_active_run_stages_a_native_queue`) |
| C2 | Steering | Ctrl/Cmd+Enter during a run | The message lands inside the current turn |
| C3 | Stop and resume | Stop a long shell command, then send a follow-up | The stopped notice shows, no orphaned declined pair, and the follow-up runs |
| C4 | Attachments | Attach an image by picker and by paste, then send | The model sees the image (vision model) or a `read_image` reference (text model) |
| C5 | Voice input | Dictate a message | The transcript of the dictation lands in the composer |
| C6 | Per-task pickers | Switch the model, Web and an integration on the draft, then send | The created task carries the three choices |

## Slash commands and skills

| # | Scenario | How | Pass when |
|---|---|---|---|
| S1 | `/new`, `/pin`, `/web`, `/model`, `/help` | Run each | Each does what `/help` says |
| S2 | `/compact` | After a few turns | "Compaction completed." and the context ring drops |
| S3 | Skill command | Run a bundled skill such as `/handoff` | The skill's instructions load and the task follows them |
| S4 | Project skill behind trust | Open an untrusted project with `.agents/skills` | The trust prompt appears; after trusting, the skill is offered |
| S5 | Instruction files and system prompt | A project with `AGENTS.md`; a custom system prompt in Settings | Both reach the model (ask it to quote them) |

## Tools

| # | Scenario | How | Pass when |
|---|---|---|---|
| L1 | `read`, `shell`, `edit`, `write` | One prompt that uses all four | Each runs without a card and the files match |
| L2 | `read_image` | A local and a public https image | Both are described |
| L3 | `todo_write` | Ask for a multi-step plan | The plan pins above the composer and updates |
| L4 | `web_search` and `open_url` | Ask for a page title | Web results and the title come back |
| L5 | `request_user_input` | Ask the model to ask you a question | The card shows; the answer reaches the model; Escape skips |

## External agents

| # | Scenario | How | Pass when |
|---|---|---|---|
| E1 | Codex start, send, status, cancel | Blocking `agent_start`, then `agent_send`, `agent_status`, and Stop from the row | Rows stream activity; cancel kills the process (**automated**: `external_agents::tests`) |
| E2 | Claude Code | The same with `provider: claude` | Same |
| E3 | Background agent | `agent_start` with `background: true` | The row stays after the turn; the completion notice and the model's follow-up arrive |
| E4 | Questions on Maple's cards | Codex `request_user_input`; Claude `AskUserQuestion` | Both render as Maple's question card and the answers reach the agent |
| E5 | Approvals | A Codex configuration that asks on request; a Claude `Write` | Accepted at once, no card (**automated**: `external_agent_approvals_are_accepted_without_a_card`) |

## Computer use, MCP, ACP

| # | Scenario | How | Pass when |
|---|---|---|---|
| U1 | Built-in CUA | Enable it in Settings, ask a task to list open windows | The task sees the windows; the per-task switch works |
| U2 | Old computer-use task | Open a task saved by a build with the standalone driver | The driver does not start; the task's Cua row reads unconfigured (**automated**: `persisted_cua_driver_stdio_entry_is_stripped_before_any_agent_starts`) |
| M1 | Custom stdio and HTTP MCP servers | Enable one of each on a task | Their tools are offered directly and run |
| A1 | ACP agent mode | A stdio ACP client: `session/new`, a tool prompt, `session/list`, `session/load`, cancel, `/compact` | No modes advertised, zero `session/request_permission` for tools, the trust chooser on an untrusted project (**automated**: `acp::tests`) |
| A2 | Proxy, `login`, `--version` | Run each mode | Each works as the README says |

## Side models and the rest

| # | Scenario | How | Pass when |
|---|---|---|---|
| D1 | Tool and thinking summaries | Setting on; run a tool | One-line summaries appear and turn off with the setting |
| D2 | Image descriptions for text models | Attach an image on a text-only model | The description reaches the model |
| D3 | Context ring and compaction | A long task | The ring fills; `/compact` empties it |
| D4 | Plan meter | Settings > Usage | The plan, percent used and reset date show, nothing else |
| D5 | Notifications | Finish a task and ask a question with the window in the background | Two notifications, none for permissions |
| D6 | Appearance, fonts, motion, Vim | Change each setting | Each applies live |
| D7 | Old task with `delegate` history | Open one | Its old calls render as plain rows and new runs offer no `delegate` (**automated**: `persisted_summon_extension_is_removed_on_the_next_run`) |

## Results

Record each run here: date, build (commit), machine, lane, and any scenario
that did not pass with a note.

| Date | Build | Machine | Lane | Result |
|---|---|---|---|---|
| 2026-10-07 | `93a235d2`, `just release` | Apple M3 Max virtual machine, 8 cores, 48 GB, macOS 27.0.1 | Local | Passed on this build: T1-T5, S1, L1, L3, C1, A1, A2, D1, D3, D4, D6. Passed earlier the same day on debug builds of the same stack: C3, C6, S2, S4, L2, L4, L5, E1-E5, U2, D7, M1 (stdio). Not run: C2 (the test harness cannot deliver Cmd+Enter), C4, C5, T6, S3, S5, U1 (the VM cannot grant the macOS permissions), M1 HTTP, D2, D5. No scenario failed. |
