# Maple Agent update delivery

The `maple-agent-updates` Cloudflare Worker serves Agent update feeds and
downloads from the private `maple-agent-updates` R2 bucket. `publish.py` fills
that bucket at the end of a release run; see
[Agent updates](../../apps/maple-agent/docs/updates.md) for releasing and the
wire contract. Research's `services/updates/` Worker is separate.

The Worker is deployed by hand and has no deploy workflow. Its routes
(`/agent/stable/*` and `/download/agent/stable/*` on `updates.maple.ai` and
`updates.trymaple.ai`) are managed by the owner in Cloudflare, not in
`wrangler.jsonc`. Download metrics use the shared Queue and D1 ledger in
[download tracking](../download-tracking/README.md); this Worker consumes events
from both update Workers.

## Checks

```sh
nix develop --no-update-lock-file .#ci -c ./scripts/ci/agent-updates.sh
```

## Routes

Only GET and HEAD are supported. Metadata and errors are `no-store`.

| Path                                                                                              | Serves                                                                           |
| ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `/agent/stable/latest.json`                                                                       | macOS v1 envelope (R2 `stable/latest.json`)                                      |
| `/agent/stable/linux/latest.json`                                                                 | Linux v2 envelope (`stable/linux/latest.json`)                                   |
| `/agent/stable/windows/latest.json`                                                               | Windows x64 v3 envelope (`stable/windows/latest.json`)                           |
| `/agent/stable/windows-arm64/latest.json`                                                         | Windows ARM64 v4 envelope (`stable/windows-arm64/latest.json`)                   |
| `/agent/stable/releases/V/{manifest,linux-manifest,windows-manifest,windows-arm64-manifest}.json` | That version's envelopes, cached as immutable                                    |
| `/agent/stable/releases/V/FILE`                                                                   | A file listed in that version's manifests; ranges and ETags, cached as immutable |
| `/download/agent/stable/{macos,linux,windows,windows-arm64}`                                      | 302 to the current installer                                                     |

A release file must be listed in one of its version's manifests and match its
recorded size, or the Worker returns 404 or 503.

## Bucket layout

`publish.py` uploads, in order:

1. `stable/releases/V/<file>` for every package (`.dmg`, `.app.tar.gz`, Linux
   `.tar.gz`, Windows `.tar.gz` and `-setup.exe` for both architectures);
2. `stable/releases/V/{manifest,linux-manifest,windows-manifest,windows-arm64-manifest}.json`;
3. the four `latest.json` pointers, with the same envelope bytes.

The release job reads its credentials from Bitwarden Secrets Manager through the
`updates-production` environment: the secret `AGENT_UPDATES_BWS_ACCESS_TOKEN` and
the variables `AGENT_R2_ACCOUNT_ID`, `AGENT_R2_BUCKET`,
`AGENT_R2_ACCESS_KEY_ID_SECRET_ID`, `AGENT_R2_SECRET_ACCESS_KEY_SECRET_ID` and
`AGENT_UPDATE_SIGNING_KEY_ID`.
