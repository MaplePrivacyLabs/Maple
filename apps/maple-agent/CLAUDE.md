# Maple Agent (GPUI)

GPUI desktop-v2 prototype for Maple, under `apps/maple-agent/`.
Read the repository-root `AGENTS.md` and `$develop-maple-agent` as well.
Commands below run from this component directory through its pinned Nix shell.

GPUI desktop app for Maple. Workspace crates: `app` (binary `maple-gpui`),
`crates/maple-agent`, `crates/maple-billing`. See `README.md` for the
layout, prerequisites, and command line modes.

## Build, test, run

`just` lists the recipes. `just ci` runs the same checks as CI (format,
clippy with `-D warnings` for every feature set, tests). Run it before a
commit. The repository pre-commit hook (`.githooks/pre-commit` here) runs
format, one workspace clippy pass, and the workspace tests when Agent files
are staged; `MAPLE_HOOK_FULL=1` runs the full `just ci`.

Build and test through `just` or `nix develop` so this checkout shares
Cargo intermediates with other maple-gpui worktrees
(`CARGO_BUILD_BUILD_DIR` under `~/.cache/cargo-build/maple-gpui/`). Do
not set a per-worktree `CARGO_TARGET_DIR` or
`MAPLE_GPUI_DISABLE_SHARED_CARGO_BUILD_DIR` unless asked. Raw `cargo`
outside those environments rebuilds gpui into this checkout's `target/`.

```sh
just build     # debug binary
just run       # debug binary with RUST_LOG=warn,maple_gpui=debug
just release   # release binary (fat LTO, one codegen unit)
just headless  # acp and proxy modes only, no window
just clean     # this checkout's target/ and dist/ only
```

Raw `cargo clean` would delete the shared cache; use `just clean` or
`just clean-local`.

Stop only the exact process launched for this checkout, through its originating
terminal or recorded PID after verifying the full executable path. Multiple
workspaces can run `maple-gpui`; a process name is not ownership. Managed
workspaces provide `bin/maple-agent` with separate config/data roots and a
shared proxy-port reservation. Never start two proxies on that reservation.

## Logs and freezes

The app logs to stderr and to `~/.local/share/maple-gpui/logs/maple-gpui.log`
(level `info` by default; `RUST_LOG=debug` for more). Panics are logged
there too. Tail it with `tail -f ~/.local/share/maple-gpui/logs/maple-gpui.log`.

If the app freezes, dump all thread backtraces while it is still hung:

```sh
gdb -p VERIFIED_PID -batch -ex "thread apply all bt" > /tmp/maple-hang.txt 2>&1
```

The default release profile strips symbols. `just release-debug` builds a
release binary that keeps line tables for usable backtraces.

## Popups

Every dropdown, overflow menu, and context menu goes through
`ui/popup.rs`. Do not wire a floating panel by hand. A view keeps one
`Popup` per nesting level and wires each opener with `Popup::trigger`.
Commands and keys that open a menu call `Popup::open` (or
`open_highlighted` to start on the current choice). The view rebuilds the
open `Menu` on every render, as a child of a `relative` box around the
trigger, and calls `Popup::sync_focus` first thing in its render (an outer
popup before a nested one). The popup handles the rest:

- a second press on the trigger closes the menu
- a press outside closes it; a menu opened inside it counts as inside
- occlusion, and placement beside the trigger that flips instead of
  covering it
- one open menu per window
- keyboard and Application Vim navigation; a text field inside the menu
  keeps its keys, and its Escape goes to the view first
- returning focus when the menu closes, even if the view stops drawing it
- accessibility

A text field inside a menu that goes away calls `Popup::refocus`, so the
menu keeps the keyboard. Menu keys live in the keymap's `Menu` context.

## Performance

This app must feel instant. Treat frame time and UI-thread stalls as bugs.

- Render functions run on every `cx.notify()`. Do not parse, sort, group,
  or clone collections inside `render_*`. Precompute when state changes
  (see `Sidebar::rebuild_sections`, `MarkdownCache`) and read it in render.
- The transcript renders through `gpui::list` with `ListState`. Keep it
  that way: never emit all timeline items as plain children. When an item
  changes in place, call `list_state.remeasure_items(ix..ix + 1)`: it
  keeps the row's last height as a hint and leaves the scroll anchor
  alone. Reserve `splice` for items arriving or leaving; a splice drops
  the measurement and moves the scroll anchor to the spliced row. The
  list follows its own tail (`FollowMode::Tail`); call `scroll_to_end`
  to pin it and `pause_following_tail` to hold it, never a render-time
  scroll (regression test: `test_streaming_chunk_keeps_wheel_scrolling_up`).
- Never block the UI thread. File dialogs, file reads, and SQLite go
  through `tokio::task::spawn_blocking` or `AgentBackend::spawn`.
- Markdown parses off the UI thread. `MarkdownCache::get` returns the
  previous document (or the raw text) while a background parse runs and
  `ChatScreen::markdown_parsed` installs the result; only a short cold
  source parses inline. Never call `markdown::parse` from a render path.
- Batch events. The backend pump drains the channel and applies a batch
  in one update; `apply_service_event` returns whether anything visible
  changed so a batch with no visible change does not re-render.
- Poll only when needed, and only `cx.notify()` when a value changed.
  Prefer pushed events over timers.
- Reuse handles: entities (`cx.new`), SQLite connections, and shaped text
  are created once and cached, not per frame or per call.
- Panels are entities embedded with `Entity::cached`, so a notify on the
  screen does not rebuild them (the sidebar is `ui/chat/sidebar.rs`).
  A cached view must never read the screen entity during render (that
  is a re-entrant borrow and a dependency that defeats the cache): the
  screen pushes what the panel shows through setters, and the panel
  answers through `cx.emit` events or, when a click needs the window,
  through a plain closure holding a `WeakEntity` of the screen. Never
  call the screen from a `cx.listener` on a panel: listeners run inside
  the panel's update, and the screen may update the panel back. Caching
  needs a definite size; a content-sized box (the composer) cannot be a
  cached view. Avoid `Window::refresh`; it discards every cached view.
- Release builds use fat LTO and one codegen unit. Use release builds
  for any performance check; the dev profile is `opt-level = 1`.
