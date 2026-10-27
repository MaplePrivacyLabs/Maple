# Vana local MCP integration

Maple Agent connects directly to the **Local MCP endpoint** served by Vana
Desktop, using native OAuth with PKCE and Vana's own scope approval screen.
There is no Maple Vana wallet key, capability file, Node bridge, hard-coded
server port, or public MCP endpoint in the new connection path.

## Connect

1. Keep an up-to-date Vana Desktop running. In **Settings → Server → Connect
   apps**, copy **Local MCP endpoint**.
2. In Maple Agent on **This Computer**, open **Settings → Integrations → Vana**.
   Paste the endpoint and click **Connect Vana**.
3. In Vana Desktop, select the scopes Maple may read and approve the request.
   The native Vana consent screen is the authority; Maple does not simulate it.
4. Return to Maple and select Vana in the task's **Integrations** menu. The
   setting for using Vana by default in new chats is a separate choice.
5. Ask Maple to list granted scopes, then answer a question using approved data.

Copy the actual URL each time rather than assuming a port. This integration
accepts only `http://127.0.0.1:<port>/mcp`; public hosts, other interfaces,
credentials in URLs, query strings and fragments are rejected. Setup is local
to the selected computer and cannot be started through a remote-control caller.

OAuth discovery, registration, token requests and data requests are bound to
that same loopback origin, with HTTP redirects and inherited proxies disabled.
The browser callback is bounded and uses PKCE/state validation. OAuth credentials
are held by Maple's authoritative local host and target/account-bound store;
secret values are not sent to the settings UI. Ordinary token refresh is native.
Removing/changing a definition or losing its account/store generation prevents
late refreshes from silently restoring stale credentials.

In Vana, **Settings → Server → Connected clients → Disconnect** revokes access.
A revoked session must be connected again. Vana Desktop must stay running.

## Privacy boundary

The Vana-to-Maple MCP data path uses loopback and does not require Vana's remote
tunnel. This is not a claim that every Vana account/grant/storage operation is
offline. Retrieved records enter Maple's configured model context and history.
Maple's store follows its normal local-store mode and OS permission policy;
plain mode is not application encryption or Keychain storage.

Current official instructions:
https://docs.vana.org/applications/connect-a-local-agent

## Synthetic fixture

[demo-data](demo-data/README.md) preserves the fictional Alex Rivera ChatGPT and
Oura fixture. No real ChatGPT, wearable or GitHub source account was connected.
Do not reimport or reset an existing Vana vault merely to test the new transport.
Approve ChatGPT alone for the main test, and confirm an Oura read is denied.

During the macOS smoke, an external-wallet test identity left Vana Desktop reporting
"Timed out waiting for external-wallet registration signature", even while its
loopback listener is healthy. In that state the native approval screen stays
loading. A normal **Server → Retry** restored the real consent screen during
this test. The test identity's registration still needs repair for repeatable
startup; preserve the imported vault and do not replace native consent with a
manually installed bearer.


## Validation

Focused tests cover endpoint/callback/origin boundaries, native OAuth and
refresh, target/account/store generation persistence, and remote setup rejection.
Use the component's pinned Nix environment and normal validation workflow.
Live acceptance requires the exact native Maple app, Vana approval, an approved
scope read with the tunnel disabled, a denied ungranted scope, and revocation.
The macOS smoke against Vana 1.63.0 completed those steps with the synthetic
fixture, plus explicit reconnect in the existing chat and a fresh approved read
after a full Maple restart. Token-expiry refresh is covered by automated tests;
a live expiry cycle and Linux/Windows runtime were not exercised.
Run the automated checks from `apps/maple-agent`:

```sh
nix develop --no-update-lock-file -c just ci
```
