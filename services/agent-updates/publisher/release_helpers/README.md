# Agent release operator

`agent_release.py` provides reusable operator commands around the canonical
`publisher.py`. A routine release writes new private JSON snapshots and evidence,
never version-specific Python files. Review helper implementation changes through
the normal PR process; for unchanged code, review the release facts and current
gate evidence. There is no per-release helper diff to approve.

Use a trusted checkout and its pinned root shell:

```sh
nix develop --no-update-lock-file .#ci -c python3 -I \
  services/agent-updates/publisher/agent_release.py --help
```

These commands do not merge, bump versions, create GitHub releases or tags,
publish registries, change signing environments, deploy Workers, or run downloaded
applications. Publishing still requires the owner's release authorization. This
CLI plans the normal macOS + Linux + Windows x64 + Windows ARM64 release; it
does not remove a platform to work around a failure. Existing immutable
three-platform bindings remain usable with their original inventories.

## Private inputs

Keep operator policy and release state outside the repository. State must be a
canonical, user-owned directory with mode `0700`. Receipts are exclusive `0600`
files and never overwrite earlier facts. Every command receives the caller's
expected SHA256 separately from the referenced JSON file. Editing a file does
not update or bless its expected digest.

The one-time private policy has schema `agent-release-policy-v1` and exactly:

| Field | Contents |
| --- | --- |
| `environment` | Existing numeric `id`, `reviewer_id`, and name `updates-production` |
| `r2` | Existing HTTPS account endpoint, bucket, and references-only credential `route` with absolute `path` and `sha256` |
| `tools` | Existing `gh` and `minisign` executable references, each with absolute `path` and `sha256` |

Do not put tokens, key material, credential values, shell commands, permissions
toggles, or new infrastructure in this policy. Repository/workflow/ref identity,
feed hosts, primary-key admission, freshness bounds and no-admin-bypass rules
are implementation contracts, not policy options. Tool bytes are verified before
use, including after resolving package-manager symlinks.

`plan` consumes a separately pinned predecessor descriptor with schema
`agent-release-previous-v1`: `previous` contains version, build, payload
`source_sha`, publisher run and attempt; the remaining fields are pinned
`previous_private_r2`, `previous_root_acceptance`, `previous_public`,
`research_preservation`, and `previous_signed_envelopes` references. The last is
the exact three or four `stable/releases/<previous-version>/*manifest.json` keys
mapped to their accepted signed envelope files. `accept` generates this descriptor for
the next release. For the initial migration, prepare it once from the already
accepted release evidence; do not treat an arbitrary current HTTP response as
predecessor authority. Legacy acceptance/public receipt schemas are supported
only with their exact bound identities, hashes and primary signatures.

Bindings use schema `agent-release-bindings-v1`. They record producer source,
tree, version, build/date/run/attempt, exact artifact IDs/names/ZIP lengths and
digests, notes/key, predecessor references, policy digest, trusted code digests
and tooling digest. Later immutable snapshots add the canonical freeze,
publication selection, signed proof and successful publication. Publisher source
and payload source remain separate. Changing implementation or private policy
invalidates an active snapshot; selecting that change requires a new explicit
input snapshot rather than silently continuing with different tools.

## Routine sequence

Common arguments below are `--state-dir`, `--policy`, `--policy-sha256`,
`--bindings`, and `--bindings-sha256`. Each command prints its output path and
digest. Use those returned values for the next command. `plan` has no existing
bindings input. `--help` on each subcommand gives the exact flags.

1. **Plan:** `plan --network --producer-run … --version … --notes-file …
   --public-key … --previous-manifest … --previous-manifest-sha256 … --output …`.
   It reads the exact completed trusted master Desktop producer, executes the
   canonical concrete Prod-job policy and captures the four exact final artifact
   records. It writes qualification, notes, key and initial binding data.
2. **Download:** call
   `download-producer <macos|linux|windows|windows-arm64> --network` once per platform using the initial bindings. IDs, ZIP sizes/digests, source and attempt
   come from the plan. Safe bounded extraction does not execute any package.
3. **Prepare and freeze:** `freeze --network --output …` calls the unmodified
   canonical preparation with its default verifier: all 21 final files receive
   independent GitHub attestation checks for exact source/master/workflow and
   denial of self-hosted runners. It writes the canonical 29-file bundle, freeze
   and a new binding snapshot. ZIP digest verification never substitutes for this.
4. **Review and dispatch once:** `dispatch` without `--dispatch-once` validates
   local frozen inputs and shows the exact request, including notes, without
   HTTP or a write. After release authorization, `dispatch --dispatch-once`
   rechecks current master, publisher code, producer and artifact metadata,
   complete bounded publication history, then persists an exclusive durable
   intent before its one POST. It never approves a protected gate.
5. **Select the actual publication:** `reconcile-dispatch --network --output …`
   reads the actual new run without dispatching again. Then
   `bind-publication --selection … --selection-sha256 … --output …` creates the
   operator binding snapshot. Keep that snapshot unchanged through all three
   approvals; output receipts are separate files.
6. **Current reserve gate:** download its exact prepared artifact with
   `download-stage prepared --network`, supplying the currently successful
   producer job and exact stage artifact metadata. `verify-bundle --stage prepared`
   validates all 29 files. With explicit `--network --r2-phase before-reserve
   --r2-output …`, it also performs the fixed private metadata GET inventory and
   emits the bundle/private-state proofs. Invoke `approve --stage reserve --approve-one`
   with the exact download/proof/private-proof paths and expected digests.
7. **Current sign gate:** after reserve actually succeeds, repeat for `reserved`
   (30 files), the prepared predecessor proof and fresh `before-sign` private
   state linked to `before-reserve`. Approve **sign only** with its current proofs.
8. **Current publish gate:** after sign actually succeeds, repeat for `signed`
   (34 files), the reserved proof and fresh `before-publish` private state.
   All four primary signatures must verify before approving **publish only**.
9. **After success:** verify `after-publish` private state with the same signed
   bundle and linked private proof. `bind-public --successful-run …
   --successful-run-sha256 … --signed-stage-proof … --signed-stage-proof-sha256 …
   --signed-bundle … --output …` creates a final public binding snapshot from the
   actual successful run. `verify-public --execute-live --output …` performs the
   existing both-host delivery/preservation matrix: 125 requests for the first ARM
   release, 127 afterward, at most seven additional Research evidence lookups, and
   14 full payload hash streams. Every
   request carries `X-Maple-Download-Probe: 1`; no duplicate payload files are saved.
   Finally `accept` with pinned public/private receipt inputs emits the accepted
   release record and the next predecessor descriptor.

The credentialed `verify-bundle` command consumes the existing
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` environment. Resolve them through
the existing references-only SecretSpec/BWS route. Run `check-policy --policy …
--policy-sha256 …` before credential resolution to verify its pinned bytes/mode
and existing executable bytes locally, without network requests. The helper rechecks that route before any
R2 access and permits only the fixed ordered GETs, never list/HEAD/write
requests. The first ARM release performs 16 metadata GETs at each gate; the
next four-platform release performs 17. Each new ARM pointer must be absent
until publication when the accepted predecessor has no ARM release. Accepted
x64 pointer and immutable bytes/ETags remain mandatory throughout; an ARM
pointer that already exists without accepted predecessor authority is a conflict.
No credential value is an argument, binding field or output.

## Checks and recovery

Artifact GETs use the verified ZIP size to choose a total transfer deadline:
two minutes plus one second per 128 KiB, rounded up, with a 15-minute minimum
and a hard one-hour maximum. This allows larger combined stages more time
without changing byte bounds or adding retries. A timeout retains its private
partial ZIP and writes no success receipt. Keep that failed output intact;
an explicitly chosen new output directory starts a fresh, fully validated GET,
not a resumed download. Never delete failed evidence to reuse its output path.

Every protected approval freshly validates the **currently waiting** job,
successful concrete predecessor job/artifact time window, exact downloaded
inventory and proof chain, current environment/reviewer/master-only policy,
artifact metadata, and a private-state proof no older than five minutes.
Each private object retains its own accepted bytes and opaque ETag. Signatures
use the compiled primary key; a configured backup alone is insufficient.

Explicit effect flags (`--network`, `--dispatch-once`, `--approve-one`,
`--execute-live`) replace edited enable switches. There is no command to approve
future gates or automatically chain all approvals. Dispatch and approval intents
are exclusive and fsynced before a POST. An ambiguous result retains the intent:
inspect/reconcile existing state and never delete it to enable a retry. Workflow
recovery and any new production action still require the existing authority.

Offline tests use synthetic releases, fake transports/stores, and disposable
temporary signing keys. They cover changed source/ZIP metadata, two versions,
23→24→27 legacy and 29→30→34 four-platform inventories, per-platform signature tampering, stale/private-state
proofs, environment/pointer races, and zero duplicate POSTs. Run the existing
publisher suite; the existing `repo` and `agent-updates` CI lanes discover it:

```sh
nix develop --no-update-lock-file .#ci -c python3 -I -m unittest discover \
  -s services/agent-updates/publisher -p 'test_*.py'
```

New plans require all four concrete Prod build/signing/downloaded-consumer chains
and select architecture-specific final artifacts. The ARM package target is
`windows-aarch64`, while its operator name is `windows-arm64`. Its strict v4
manifest/feed and primary signature remain separate from the shipped x64 v3
feed. Continuing an existing three-platform snapshot preserves its 16 final-file
attestations, 23/24/27 stage inventories, 13 private reads, and 97-request/10-stream
public matrix; it cannot silently include ARM due to a changed workflow default.
