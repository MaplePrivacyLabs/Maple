# Maple runtime smoke references

Read only the relevant runtime scenario. Select the
[environment and supported login path](../../../../docs/development-environments.md)
first. Local integration uses encrypted password/account fixtures; provider
OAuth/native handoff belongs to intentional hosted/provider validation.

## Smoke a configured local backend

Preserve existing configuration. Create a local environment file only when it
is absent:

```bash
test -e apps/maple-research/frontend/.env.local || cp apps/maple-research/frontend/.env.example apps/maple-research/frontend/.env.local
```

Never replace an existing `apps/maple-research/frontend/.env.local`; it may be externally managed
or contain checkout-specific endpoints and application identity. Start the
OpenSecret backend under `services/opensecret/` by
[its component guide](../../../../services/opensecret/README.md),
including required migrations, then launch Maple from this checkout with:

```bash
nix develop --no-update-lock-file -c just desktop-dev
```

`just desktop-dev` consumes the configured development endpoints and applies
the active `.local/tauri-workspace.json` overlay when present. It is the path
for a configured local-backend desktop smoke; it is not evidence about a fixed
PR package. Record the exact identity fields below, use a disposable local password
account/fixture through its owning setup instructions, confirm the effective
API/project and login, then exercise the relevant backend operation.

Before launch, read the active Tauri `devUrl` and inspect its listener. If it is
occupied, stop the process only when you can prove it belongs to this checkout,
using its owning lifecycle mechanism where one exists; otherwise select another
overlay or port. When packaged behavior and the overlay identity are both
required, build with:

```bash
nix develop --no-update-lock-file -c just desktop-build-debug-overlay
```

On macOS, verify the produced app's `CFBundleIdentifier` from its actual
`Contents/Info.plist` before launch. Do not infer the identifier from the config
file alone.

For privileged IPC, manually trigger the real React user action, observe the
Tauri command and native validation, verify the exact filesystem/process/native
effect, and exercise a representative denied input. This manual exact-app smoke
is required when no checked-in integration test crosses React, IPC, and Rust.

## Prove exact application identity

Before any desktop GUI smoke test, record:

1. Commit SHA, build profile, build command, and active Tauri configuration overlay.
2. Effective OpenSecret API/project ID, login method and auth origin, plus billing/flags endpoints when relevant.
3. Exact executable or `.app` path.
4. Actual bundle/application identifier. The standard identifier is `cloud.opensecret.maple`, but an overlay can change it.
5. Dev-server URL and owning PID. The standard Tauri dev URL is port `5173`, but an overlay can change it.
6. Native application PID and, when relevant, local proxy port plus listener-owning PID.
7. Disposable account and test-data scope.

Target the recorded identifier and path. Never select or terminate an app only by the display name `Maple`; multiple checkouts can share it. If an overlay is active, use it consistently for build, launch, automation, and cleanup.

A package with the standard identifier can share installed Maple state and
single-instance identity. Do not launch it against non-disposable state. Use a
package whose overlay-derived identifier you verified is distinct, or a
disposable OS account or VM when the standard package identity itself is under
test.

Distinguish these targets:

- A browser tab proves web behavior only.
- A raw `tauri dev` executable can prove native development behavior but not packaged resources, signing, updater, installer, or bundle registration.
- A packaged application proves only the scenarios actually observed after launching that exact artifact.

If debug packaging emits an application and then exits nonzero because `TAURI_SIGNING_PRIVATE_KEY` is absent, report the packaging failure. You may separately smoke the emitted application, but do not call the build successful.

Clean up only the exact PIDs, listeners, temporary accounts, and artifacts created by the test. Never kill processes by generic app name or perform broad cleanup.

## Smoke critical native surfaces

Choose only scenarios relevant to the change, but cross every changed boundary.

### Agent Mode, MCP, and local proxy

- Run Agent Mode in the desktop app; it is unavailable in the web build.
- For presentation-only Agent Mode changes, exercise the exact changed states,
  interactions, accessibility, focus, theme, and layout in the native app. Do
  not add file writes, shell execution, account switching, or lifecycle
  manipulation merely to populate the UI.
- For behavioral changes, verify the applicable start, intermediate state,
  permission, cancellation, completion, restart, and shutdown boundaries. Run
  `$change-maple-agent-mode` for the proportional lifecycle matrix.
- Confirm process and listener ownership before and after cancellation, logout,
  account switch, and app exit when those long-lived boundaries are affected.
- For MCP, follow `apps/maple-research/docs/agent-mode-mcp.md`: use the pinned Everything server, send a unique marker, verify server request, arguments, result, and final answer, then disable or stop the server and verify a clear failure.
- Verify that stale sessions cannot cross account boundaries when session or
  account ownership changed.
- Before local-proxy smoke, choose and verify an unused checkout-specific
  loopback port. After start, verify that the recorded Maple PID owns the
  listener; do not assume the default port is available.
- Keep two proxy smoke targets separate. For the standalone binary, launch the
  exact checkout on loopback with explicit backend/PCR configuration, no saved
  key in browser-facing CORS mode, and record the binary and listener PID. For
  the Tauri Local OpenAI Proxy, start it through the exact packaged or
  development app UI and verify its account, lifecycle, saved-key, CORS, and
  logout behavior through that app. One target does not prove the other.
- When forwarding behavior changes, exercise authenticated `/v1/models`, one
  non-streaming chat response, one streaming response with exactly one
  `[DONE]`, and embeddings against the intended backend. Add invalid/missing
  authentication, timeout, cancellation, and upstream-error cases as
  applicable. `/health` alone is only liveness evidence.

### PDF and OCR

- Test a text PDF, scanned PDF, mixed PDF, malformed or locked input, and applicable size/page limits.
- Exercise cold and warm model-cache paths when OCR behavior changes.
- Run the ignored model-backed test only when its external model prerequisites are available, following `apps/maple-research/docs/pdf-ocr.md`; label it separately from the default Rust suite.
- Verify cancellation and recovery from extraction failures through the exact app.

### Deep links and native services

- Invoke the real link against the recorded bundle identifier; do not merely paste its payload into an internal route.
- Verify cold start and already-running behavior where relevant.
- Exercise real dialogs, filesystem access, keyring, updater, microphone, and packaged resources on each affected platform.

### CUA only: macOS Open/Save

Codex must use its built-in Computer Use instead of CUA, even when CUA is
installed. Other agents should skip this unless driving the GUI with
cua-driver (pid + `window_id`).

On macOS every Maple native file or folder picker — Agent project folder,
chat photo/document upload, Save, and any other Tauri `dialog` — is hosted
by **Open and Save Panel Service**
(`com.apple.appkit.xpc.openAndSavePanelService`), not Maple and not Finder.
After the picker appears, `list_windows` with no pid filter and take the
panel-service window whose title is `Open` or `Save` and whose bounds
match Maple's new stub.

Do not act on Maple's `Open`/`Save` window: its AX is empty, background
pixels fail `off_space_or_ax_unresolved`, and Maple-pid clicks (including
a correct foreground crosshair) do not change the sheet. Sheet AX on the
Maple parent is observation-only; presses return
`element_outside_target_window`.

Act on the panel-service pid/`window_id` (`foreground` pixels). That
window may be `is_on_screen: false` and screenshot black. Confirm on
Maple (`Where:` / selected name, then the product result: Trust/PROJECTS
for an Agent folder, or the composer attachment for an upload). Do not
escalate the CUA session to desktop because Maple's stub refused input.
After the panel closes, Maple web AX is the target again.
