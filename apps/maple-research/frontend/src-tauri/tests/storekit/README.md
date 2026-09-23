# Local StoreKit bridge experiment

This harness targets an exact prebuilt **Maple.app** through its real WebView,
Tauri IPC, and vendored `tauri-plugin-iap`. The tiny `StoreKitHarnessHost` target
exists only so Xcode can build an independent UI-test runner. The Python runner
replaces `UITargetAppPath` in Xcode's generated `.xctestrun` with the supplied
Maple bundle, then calls `test-without-building`. It never rebuilds Maple.

`SKTestSession` reads `Maple.storekit` from the test bundle and targets the default
`XCUIApplication`, whose target is the supplied Maple app. Merely installing or
launching Maple with `simctl` does **not** enable local StoreKit testing.

## Build and isolated test cases

Use the exact frozen simulator `Maple.app` and a simulator you booted explicitly.
The runner does not choose or boot a device. Build Maple separately when needed:

```sh
nix develop --no-update-lock-file .#apple -c scripts/testing/storekit/build-simulator.sh
```

That build uses the repository Xcode pin and prepares both the exported
`arm64-sim/Maple.app` and the exact debug simulator product resolved with
`xcodebuild -showBuildSettings`. It enables local networking in the built bundles,
re-signs/verifies them, and generates the local experiment scheme. Source plist
and entitlements are restored. Preserve the existing frozen artifact when
comparing toolchains; rebuilding Maple is a separate action.

These test commands explicitly select installed Xcode 26.6 **after entering** the
pinned Apple shell, without changing repository pins or global Xcode selection.
Use `run.sh`, which prepares native linker variables; do not invoke `run.py`
directly from an unprepared Nix shell. Substitute the exact frozen app path and
intended simulator UDID when different from these examples.

First build the small runner and run the catalog canary, which needs no fixture:

```sh
nix develop --no-update-lock-file .#apple -c env \
  MAPLE_NIX_XCODE_VERSION=26.6 DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
  bash scripts/testing/storekit/run.sh \
  --app apps/maple-research/frontend/src-tauri/gen/apple/build/arm64-sim/Maple.app \
  --udid "<YOUR_BOOTED_SIMULATOR_UDID>" \
  --only MapleStoreKitUITests/test01CatalogAndStorefront
```

Then run the certificate canary with a fresh runner-owned bootstrap fixture:

```sh
nix develop --no-update-lock-file .#apple -c env \
  MAPLE_NIX_XCODE_VERSION=26.6 DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
  bash scripts/testing/storekit/run.sh --skip-build --fixture-bootstrap \
  --app apps/maple-research/frontend/src-tauri/gen/apple/build/arm64-sim/Maple.app \
  --udid "<YOUR_BOOTED_SIMULATOR_UDID>" \
  --only MapleStoreKitUITests/test02SigningCertificate
```

Managed mode refuses occupied port 38863. Stop an existing manual fixture through
its owner first; the runner never kills an existing listener or deletes a journal.
It starts one fixture process with a new durable journal for each selected
purchase case and stops that owned process in cleanup. Bootstrap supplies the
fixed local account token but rejects all transaction submissions. A
native-verified Xcode purchase exports only its public signing certificate through
XCTest. The runner saves `MapleStoreKitSigningCertificate.cer` in the timestamped
result directory; it never writes a raw signed transaction to test artifacts.

Pass that certificate to the remaining cases. They reuse the runner build but
each gets a separate XCTest invocation, fixture process and journal:

```sh
nix develop --no-update-lock-file .#apple -c env \
  MAPLE_NIX_XCODE_VERSION=26.6 DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
  bash scripts/testing/storekit/run.sh --skip-build \
  --fixture-certificate /absolute/path/to/MapleStoreKitSigningCertificate.cer \
  --app apps/maple-research/frontend/src-tauri/gen/apple/build/arm64-sim/Maple.app \
  --udid "<YOUR_BOOTED_SIMULATOR_UDID>" \
  --only MapleStoreKitUITests/test02PurchaseAcknowledgmentThenFinish \
  --only MapleStoreKitUITests/test03FailureRelaunchAndRecovery \
  --only MapleStoreKitUITests/test04PendingApprovalUsesListener \
  --only MapleStoreKitUITests/test05CancelledPurchase
```

`--fixture-certificate` without `--only` selects all six cases. Bootstrap mode
rejects acknowledgment cases. `--fixture-external` supports an explicitly owned
manual fixture for **one purchase case per invocation**; restart that server with
a fresh journal between cases. Both runner and XCTest reject stale journals.
See [`fixture-server.README.md`](../../../../../../scripts/testing/storekit/fixture-server.README.md).

The tests check a StoreKit control round-trip before launching Maple, then cover
the exact three-product catalog/default USA storefront, deferred finish until
local acknowledgment, failed acknowledgment followed by relaunch/recovery,
Ask to Buy approval through the transaction listener, and simulated cancellation.
Acknowledgment tests check the exact transaction ID is absent from the durable
journal before recovery, remains absent during an outage, and appears with its
matching original ID/product afterward. Reused local transaction IDs cannot make
an old fixture record count as fresh proof. Each test clears StoreKit transactions
for this app; use dedicated simulator test state. These are local Xcode purchases.
The separate CAN storefront-override diagnostic remains unresolved; the default
USA check does not claim to test dynamic storefront changes.

Artifacts under `.local/storekit-harness/<timestamp>/<case>/` include the exact
test configuration, log, `.xcresult`, screenshots, public certificate, fixture
process identity and durable journal. Root metadata records Xcode/SDK/developer
path, simulator identity, Git state, harness source hashes and the complete Maple
bundle digest. Every case verifies the installed bundle matches the frozen input.
`--skip-build` rejects changed runner source or toolchain identity. Duplicate,
skipped, missing or unexpected test results never count as a pass.

XCTest cases have a 120-second execution allowance. `--timeout` bounds each
isolated test invocation (default 180 seconds); `--build-timeout` bounds the build
(default 180). Execution stops at the first failed case and records the planned
and completed lists. Cleanup stops owned process groups and the exact launched
app/runner bundles on the selected device; it does not shut down the simulator.
Interrupted `.xcresult` bundles may be incomplete; logs and available evidence
remain preserved. Capture-only does not launch, terminate, configure or reinstall
Maple and cannot manage or contact a fixture.

## Interactive payment sheet

```sh
open apps/maple-research/frontend/src-tauri/gen/apple/maple.xcodeproj
```

Select `MapleStoreKitExperiment` and the intended simulator, then choose
**Product → Perform Action → Run Without Building** (Control-Command-R). The
generated scheme enables the StoreKit configuration on the real Maple target;
open the `.xcodeproj` directly so its workspace-relative configuration reference
resolves correctly. The Run action uses the existing debug app in Xcode's
DerivedData products directory, which the build script has already prepared with
the local-network setting and ad-hoc signature. The exported `arm64-sim/Maple.app`
and DerivedData product are separate bundles; both prepared paths are printed by
the build command. The generated experiment scheme is local, reproducible output
of `prepare-xcode.py`; keep that generated scheme out of the source change.

The helper also registers the original `tests/storekit/Maple.storekit` in the
generated project's navigator. A scheme path alone was insufficient on this
host: Xcode's missing-file indication cleared after adding the navigator
reference. The helper adds one `PBXFileReference` and its main-group entry,
without target membership or a resource build entry. It uses macOS `plutil` to
validate the project before and after the narrow insertion, preserves unrelated
project bytes, and makes repeated preparation idempotent. It reuses an existing
matching source-root or main-group reference and refuses matching target
membership or unexpected project structure. It does not regenerate the project,
change package references, or remove other manually added navigator entries.

The generated Run action disables debugger attachment and uses Xcode's
`PosixSpawn` launcher by default. On this host, LLDB symbol loading left Maple
suspended before its WebView started; the same built app launched promptly with
debugger attachment disabled. This changes only Run: the Test action retains its
original debugger configuration. Use `python3 scripts/testing/storekit/prepare-xcode.py
--debugger` when you explicitly need LLDB for the experiment. The StoreKit
configuration remains selected in either mode; a successful launch alone does
not prove that StoreKit loaded it.

To prepare existing products again without rebuilding (for example, after an
Xcode build replaced its DerivedData product), use the same pinned toolchain:

```sh
nix develop --no-update-lock-file .#apple -c bash -c \
  'source scripts/ci/_common.sh; use_xcode_toolchain; python3 scripts/testing/storekit/prepare-simulator-products.py; python3 scripts/testing/storekit/prepare-xcode.py'
```

The preparation helper also accepts `--check-only` to resolve and validate both
existing bundles without modifying them. It never patches the source plist or
entitlements.

Use **Load products**, **Purchase Pro**, the system confirmation sheet, then
**Recover purchases**. Keep automatic recovery disabled when observing the
unfinished-before-acknowledgment boundary. Xcode's **Debug → StoreKit → Manage
Transactions** provides interactive transaction controls.

To export metadata from an already running purchased diagnostic screen without
reinstalling or restarting Maple, pass the exact GUI-built DerivedData app:

```sh
nix develop --no-update-lock-file .#apple -c env \
  MAPLE_NIX_XCODE_VERSION=26.6 DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
  bash scripts/testing/storekit/run.sh --app /absolute/path/to/Maple.app \
  --udid "<YOUR_BOOTED_SIMULATOR_UDID>" \
  --only MapleStoreKitCaptureUITests/testExportPublicSigningCertificate
```

This case verifies the installed bundle matches the supplied app, uses the
placeholder only as Xcode's install target, then queries the running Maple by
bundle ID. It never creates an `SKTestSession` or calls launch/terminate on Maple.
It obtains one `debugDescription` snapshot and parses the diagnostic JSON already
in that snapshot. The parser rejects missing, malformed, conflicting, or oversized
status objects. It requires the Xcode environment and either a successful purchase
or an explicit `verified_recovery` certificate source from a nonempty native
verified transaction listing. Recovery extraction preserves `purchaseStatus` and
does not prove a new purchase. It attaches the public certificate immediately,
with no later element queries or screenshot request. This is diagnostic extraction from XCTest's current text
format, which Apple does not promise as a stable test API; it is not a selector
mechanism for the normal purchase tests. The initial snapshot can still hang.
Timeout or failed extraction remains a failed test, even when partial artifacts
are recoverable.

The exact Foundation-only parser can also be checked on the host, without
building an app or operating a simulator:

```sh
DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer \
  python3 scripts/testing/storekit/check-snapshot-parser.py
```

Pass `--snapshot /absolute/path/to/saved-accessibility-attachment.txt` to also
check a preserved real snapshot. These fixtures exercise braces and escapes in
strings, duplicate agreeing copies, truncated values, conflicting status, and
input limits; they do not establish that live accessibility capture works.

Bundle identity includes every relative file path and byte, including
`Maple.debug.dylib`, resources, and code signatures. Symlink targets are hashed
without following links outside the bundle. The digest ignores timestamps and
filesystem ownership. A focused mutation check covers debug-library/resource
changes and symlink behavior:

```sh
nix develop --no-update-lock-file .#apple -c python3 -B -m unittest discover \
  -s scripts/testing/storekit -p 'test_*.py'
```

The Python checks use temporary files and mocked process/network calls. They do
not build or launch Maple, start a fixture, or operate a simulator. The revised
runner and XCTest checks still require coordinated simulator validation; these
changes do not resolve the previous WebKit accessibility hang by construction.

## Validation and handoff

### September 20: Xcode 26.6 / iOS 27 follow-up

Local StoreKit now supports selected native tests and manual Maple recovery on
this host. The final Maple bundle was built with the repository's Xcode 26.5 pin
and run under Xcode 26.6 (17F113), iOS Simulator 27.0 (24A434), macOS 26.4.
The full Maple build under 26.6 remains blocked by the ONNX package's verified
toolchain-hash list. No checksum bypass or repository pin change was made.

The current preparation helper's registered StoreKit reference and debugger-free
Run action were exercised in Xcode. Actual Maple recovered and durably
acknowledged a Pro subscription, opened the local native management sheet,
received a signed Max upgrade event, and recovered that upgrade after an app
restart. Explicit restore returned successfully. The acknowledgement response
kept `stripe` selected; repeated requests did not duplicate the fixture journal.
The management-created upgrade omitted its account token. The fixture accepts
that omission only for an exact original transaction ID already durably owned in
the same fixture journal; unknown originals and present wrong/null tokens still
fail. These are Xcode fixtures, with no charge or real Maple entitlement.

Separate tests using the real Swift plugin and Tauri Invoke passed exact-ID
finish safety, purchase-error sanitization/lock release, and rejection of an
actually unverified transaction. The exact-ID case verified that retrying finish
for already-finished A cannot finish a later unfinished B. This is native command
coverage; it does not replace the complete renderer/backend flow.

Two further selected native cases passed. Cancelling a visually confirmed Xcode
test purchase sheet returned status-only `cancelled`, no transaction/entitlement,
and allowed another purchase on the same plugin instance. A single pending Ask to
Buy transaction exposed no signed recovery row until exact test-session approval;
then it became verified/unfinished and explicit finish cleared it. Approval was
observed by native polling, not JavaScript event assertions. Separate broader
controls remain failed: cancellation error injection threw instead of returning
cancelled, and buying B while A was pending unexpectedly exposed both unfinished
IDs before explicit approval. Those failures were not changed into passes.

Two important limits remain. First, a raw StoreKit control with no plugin
listener or finish call dropped the first of two active transactions from
`Transaction.unfinished`; both remained current entitlements. That assertion is
still failed, and no specific Apple bug is established. Maple's native unfinished
queue also became empty before acknowledgement during one 503 test, so an empty
queue alone is insufficient finish proof. Second, the automated Maple UI suite
has not passed: XCTest sometimes hangs or returns a WebView accessibility tree
without the visible HTML controls/status. A final capture-only retry after the
scheme correction failed `missingStatus`; manual success is not a suite pass.
Dynamic storefront override and the older Transaction Manager finish discrepancy
also remain unresolved. A fresh CAN-default fixture returned CAN from raw
Storefront.current before purchase, but an unexpected sign-in prompt was cancelled
and the full transaction-country comparison did not complete. No credentials
were entered. Physical-device sandbox and production server validation are still
required. After the selected checks, the exact test device was shut down and
Xcode/Simulator and owned test/fixture processes were closed; evidence and device
data were preserved.

The dedicated **Show public certificate** screen provides the native-verified
transaction's public Xcode signing certificate without showing the JWS. If
XCTest capture is unavailable, select/copy that public text in the simulator to a
local editor and validate the decoded P-256 certificate before independently
pinning it in the fixture. Do not trust arbitrary request-supplied certificates
or substitute Xcode's legacy RSA receipt-certificate export. Preserve provenance
and the journal for the same run.

### September 19: historical Xcode 26.5 / iOS 26.5 check

The final manual check used the experiment-enabled Maple build on Xcode 26.5
with iOS Simulator 26.5 (23F77), activated through the generated Xcode Run scheme.
It exercised the actual Maple WebView, Tauri plugin, StoreKit purchase UI, and
the certificate-pinned loopback acknowledgment fixture.

| Check | Observed result |
| --- | --- |
| Catalog and purchase | Three configured products and the USA storefront loaded. The native Xcode purchase sheet explicitly stated there would be no charge. Purchase transaction `1` succeeded, with native verification and an unfinished queue containing `1`; the fixture journal had no acknowledgment for `1`. |
| Acknowledgment outage | With the fixture returning HTTP 503, **Recover purchases** reported `storekit_recovery_incomplete`. Transaction `1` remained in the unfinished queue and was absent from the durable acknowledgment journal. |
| Successful acknowledgment | With the fixture restored, recovery acknowledged transaction `1`, retained the fixture's selected provider `stripe`, and durably journaled `1`. The native finish call returned and the native unfinished API reported an empty queue. |
| Independent finish confirmation | **Unresolved:** Xcode's transaction manager continued to display an Unfinished warning for transactions `0` and `1`, including after selecting the rows again. The empty native queue and successful finish return do not establish that finishing is fully validated. |
| Automated suite | The runner compiles, but the suite has not passed. `SKTestSession` reported `SKInternalErrorDomain Code=3` while saving configuration/settings; XCTest also encountered a white WebView and intermittent WebKit accessibility hangs. Automated pending/approval, cancellation, and relaunch recovery remain unverified. |

The finish discrepancy must be resolved independently before treating this as a
completed integration. Preserve the current screenshots, fixture journal,
public certificate, bundle-identity metadata, logs, and `.xcresult` bundles.
Public-certificate extraction from a saved accessibility attachment succeeded
despite a timed-out capture test; that partial evidence does not make the test
pass.

Apple's [iOS 26.6 release notes](https://developer.apple.com/documentation/ios-ipados-release-notes/ios-ipados-26_6-release-notes)
identify a fix for simulator StoreKit test-session connection failures
(174738526 / FB22500243), but the earlier recommendation to install an iOS 26.6
simulator was incorrect: Apple's public runtime catalog had no such download
on 2026-09-19. The stable iOS 27.0 runtime (24A434) was instead installed through
Xcode's component installer. A separate iPhone 17 Pro simulator completed its
first boot using the existing Xcode 26.5 on macOS 26.4; the small XCTest runner
also built successfully for that destination. The catalog-only canary then
failed while saving StoreKit configuration and changing session settings:
`SKServiceErrorDomain Code=2`, underlying `SKInternalErrorDomain Code=4`.
XCTest showed a white Maple screen. The failed run was interrupted and its
result bundle preserved; installed app identity matched the frozen input.
The runtime upgrade alone did not clear the test setup failure, and no new
purchase or finish behavior was validated. The original iOS 26.5 device and
evidence are preserved.

[Apple's Xcode requirements](https://developer.apple.com/xcode/system-requirements)
list macOS 26.6 or later for Xcode 27. The runtime installation did not upgrade
Xcode, change the repository's Xcode pin, or update the host OS. A complete
toolchain upgrade remains a diagnostic step, not a demonstrated fix for these
errors.

On 2026-09-20, Xcode 26.6 (17F113) was installed alongside 26.5 from Apple's
signature-verified archive, and first-launch setup completed. The repository
pin and global Xcode selection remain 26.5; comparisons selected 26.6 explicitly.
A separate minimal native app-hosted XCTest removed Maple, Tauri, WebView,
accessibility and retargeted test configuration from the probe. All five source
hashes matched between its baseline and comparisons, and both Xcode 26.6 runs
used the same newly built probe bundle.

| Native probe destination | Observed result |
| --- | --- |
| Xcode 26.5 / iOS 26.5 baseline | Code 3 while configuring StoreKit; simulated-error readback nil, failing before catalog lookup. |
| Xcode 26.6 / same iOS 26.5 device | Same Code 3 and readback failure. |
| Xcode 26.6 / iOS 27 | Configuration round-trip and exact three-product catalog assertions succeeded. Immediate StoreKit 2 storefront readback was USA after requesting CAN, so the test failed. No StoreKit internal errors logged. |

Each run reports one failed test, zero passed/skipped; none attempted a purchase.
The original probe's unconditional combined success print is misleading;
the `.xcresult` records the storefront failure. iOS 27 shows partial recovery,
but the remaining storefront assertion needs separate diagnosis: the original
test did not read the test-session property or SK1 storefront back and did not
wait for StoreKit 2 updates. These app-hosted results do not establish a working
Maple UI-test path or resolve the existing finish discrepancy.

A separate diagnostic source copy then reported two passing tests and one
failure on Xcode 26.6/iOS 27. Configuration/catalog passed. A local StoreKitTest
purchase produced a verified Xcode transaction; its unfinished queue contained
the ID before finish and was empty afterward. Purchase history retained the
expected purchased row; `SKTestTransaction` has no independent finished flag,
and Transaction Manager was not inspected. Storefront remained USA in both SK1
and SK2 despite session-property readback CAN over eight seconds, with no
storefront updates. This remains an unresolved storefront failure, and the
native purchase/finish result does not validate Maple's complete recovery flow.
The diagnostic source and result are preserved separately from the frozen
comparisons under `diagnostic-xcode266-ios27/`.

Evidence is preserved under `.local/storekit-harness-xcode266/`, including
source hashes, commands, logs and `.xcresult` bundles. After each run, wait for
owned test processes to exit, shut down only the exact test device and close
its window. Preserve device data and prior evidence.

This initial slice is handed off with those runtime gates open. A compatible
toolchain and the required Apple sandbox/account credentials are prerequisites
for the remaining validation. Local fixture acknowledgment does not prove App
Store Server API validation, notifications, production entitlement projection,
real billing, or App Review acceptance; those remain separate sandbox/backend
stages.
