# Private download tracking

The two update Workers record unsampled response observations in one private D1
ledger. They enqueue through `DOWNLOAD_EVENTS`; the Agent Worker consumes the
queue and writes through `DOWNLOAD_DB`. Reporting uses Cloudflare's authenticated
account API. There is no public statistics endpoint or dashboard.

## What the counts mean

Each eligible HTTP response gets a random event UUID and the original server UTC
timestamp. Queue redelivery and the producer's single retry preserve that UUID;
`INSERT ... ON CONFLICT(event_id) DO NOTHING` prevents duplicate ledger rows.
Separate HTTP requests get separate IDs, including client retries.

These are exact counts of persisted observations, with no sampling or estimates.
They do not measure unique people, completed transfers, or successful installs.
Agent payload events describe an R2 response starting. The stream is returned
unchanged: tracking never tees, clones, buffers or consumes it. `response_length`
and report `expected_response_bytes` describe expected response size, not bytes
actually delivered to a client.

Research installers remain hosted on GitHub. Their Worker records redirect intent;
it cannot observe the subsequent GitHub transfer. Research native updater payloads
also go directly to GitHub. Agent aliases and their follow-up payload responses
are separate event kinds. Never add aliases and payloads together as downloads.

Reports keep `kind`, method, status and range separate. A useful Agent full-payload
view filters `kind=artifact`, `method=GET`, `status=200`; `range=requested` can still
be a full response after an `If-Range` mismatch. `206` is a partial response,
`416` is an unsatisfied range, and `304` transfers no payload. HEAD and metadata
polls are also recorded separately. Probe exclusion is the reporting default.

## Availability and retry boundaries

Capture runs under `waitUntil` after response construction and never awaits the
queue on the download response path. A failed send receives one bounded retry
with the same ID. A missing binding, scheduling failure, or final enqueue failure
fails open: the download continues and observations can be lost before the queue
accepts them. A fail-open collector cannot promise every HTTP request is counted.

The consumer validates every bounded schema field and acknowledges only after a
successful D1 insert. SQL errors and invalid messages retry; the configured
consumer uses batches of 25, up to five seconds of batching, ten retries and a
60-second retry delay. Exhausted messages go to `maple-download-events-dlq`.
Transient SQL failure does not change downloads. Invalid events remain poison
messages until their cause is understood; they are never silently accepted.

Both provisioned queues use a 14-day message-retention setting. Queue and DLQ
retention are finite, so inspect backlog and DLQ failures before messages expire.
Check the live queue settings during incident handling; retention is not a D1
history policy. The D1 migration has no TTL or cleanup job: committed observations
remain until explicitly deleted, subject to the database's storage limits. Any
future pruning, backup or rollup policy must be deliberate.

Tracking emits only fixed JSON error codes: `queue_binding_missing`,
`capture_failed`, `queue_send_failed`, `invalid_event`, and `database_write_failed`.
Worker observability enables full-rate console error capture with
`head_sampling_rate: 1` and disables automatic invocation logs. Our emitted error
message never contains exceptions, URLs, request/response bodies, credentials or
event contents. Cloudflare may attach its own operational context, including
request metadata, to a log event; this configuration is not a guarantee that all
provider logs omit URLs, queries or IP addresses. The privacy guarantees below
describe our Queue/D1 event fields and application error messages. Infrastructure
backlog/log monitoring is separate from the private report CLI.

## Privacy and trust boundaries

Only the two fixed update hostnames and known route shapes are classified. The
ledger contains product, version when safely known, platform, fixed artifact kind,
response kind, method, status, coarse range/result, expected length, coarse client
kind, timestamp, random per-observation UUID, and a probe flag. No IP, account or
device identity, cookies, raw User-Agent, filename, path, query string, or other
arbitrary header value is retained. The UUID identifies one observation, not a
visitor. Research marketing queries are ignored while classifying the pathname.

Client kind is derived from a few User-Agent patterns and is self-reported. The
`x-maple-download-probe: 1` flag is also self-reported: anyone can set it. It is a
convenient filter for our verification requests, not authentication or proof that
other traffic is human. Versions on latest-metadata events are null because the
tracker does not read metadata bodies to infer them.

## Private reports

Use the pinned repository shell. Inject existing credentials through the owning
SecretSpec route; do not put tokens in arguments, shell history or output. The CLI
reads `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN`. A D1 Read token is sufficient
for reporting. It reads the `DOWNLOAD_DB` database ID from Agent's checked-in
`wrangler.jsonc`; `DOWNLOAD_ANALYTICS_DATABASE_ID` optionally overrides that
nonsecret ID. The script creates no resources and issues only fixed SELECTs with
bound values through the private D1 SQL API. HTTP redirects are refused.

From the repository root, with the existing credential environment already
injected:

```sh
python3 services/download-tracking/query.py
python3 services/download-tracking/query.py --bucket hour --product agent --kind artifact --method GET --status 200
python3 services/download-tracking/query.py --product research --kind alias --method GET --status 302
python3 services/download-tracking/query.py --since 2026-10-27T00:00:00Z --until 2026-10-28T00:00:00Z
python3 services/download-tracking/query.py --health --include-probes
```

The default window is the last seven days, with UTC day buckets. `--since` is
inclusive and `--until` is exclusive. Both require UTC timestamps ending in `Z`,
with seconds or exactly three decimal digits. `--bucket hour` uses UTC hours.
Windows must be positive and at most 31 days. Query longer histories in adjacent
bounded windows. Other optional filters are `--version`, `--platform`,
`--artifact`, `--range` and `--host`; `--help` lists fixed accepted values.

JSON rows group by UTC bucket, product, version, platform, artifact, kind, status,
method, range and probe flag. Responses are capped at 10,000 grouped rows; an
extra row detects overflow and sets `truncated: true`. Narrow the window or
filters before using a truncated report. Empty buckets have no row. This is not
a sampling limit: stored events are retained.

`--health` reports the latest persisted timestamp, total matching observations,
and matching observations since the start of the current UTC day, all within the
selected window. A quiet traffic period can look stale. Confirm pipeline health
with a marked probe and its subsequent ledger record, then inspect primary/DLQ
backlog and fixed failure logs. The CLI does not claim queue health or read live
queue state.

Failures emit numeric JSON codes on stderr, without upstream bodies, exception
messages or URLs: `2` argument syntax, `10` credential/config shape, `11` time
window, `12` filter value, `20` HTTP failure (plus numeric `http_status`), `21` D1
API rejection, `22` invalid reply shape, `23` oversized reply, `24` network failure.

## Manual DLQ recovery

Replay is an authenticated operational action, outside the read-only CLI. Before
replay, repair the schema/storage/consumer failure and inspect fixed failure logs
and both queue backlogs. Preserve the original message; do not fabricate counts
for a capture gap.

1. Confirm the intended account, primary queue and DLQ, including live retention.
   Use an explicitly authorized temporary pull consumer for the DLQ, without
   replacing the primary queue's deployed consumer. Pull a small bounded batch
   with enough visibility time for validation and republishing.
2. Decode the pull API's JSON message encoding as documented. Validate each event
   with `isDownloadEvent` in the trusted module. Preserve `event_id`,
   `observed_at`, and every original field. Do not log the message or credentials.
   Investigate invalid messages instead of weakening validation or auto-looping
   them through the primary queue.
3. Republish each valid original JSON event to `maple-download-events` using JSON
   content type. Wait for the primary queue's successful enqueue acknowledgement.
   Only then acknowledge that DLQ lease. On ambiguous enqueue/ack, retry with the
   same event ID; the committed D1 row deduplicates it.
4. Check D1 persistence and both backlogs, then remove the temporary DLQ pull
   consumer through its owning configuration. Do not purge queues, recreate the
   database, change old timestamps, or generate new IDs to make replay pass.

The query token need not have queue-mutation permission. Use the existing
separately authorized operations credential for replay; never broaden reporting
access merely to drain a DLQ.

## Local verification

```sh
nix develop --no-update-lock-file .#ci -c python3 -m unittest discover -s services/download-tracking -p 'test_*.py'
```

Focused TypeScript tests cover response preservation, route privacy, queue/SQL
outages, malformed events and real SQLite deduplication. The Agent service's
runtime test exercises actual local workerd request → Queue → D1 delivery,
duplicate events and a missing-table outage. These are separate from production
configuration, deployment and live collection verification.

References: [D1 query API and accepted permissions](https://developers.cloudflare.com/api/resources/d1/subresources/database/methods/query/),
[Queue delivery guarantees](https://developers.cloudflare.com/queues/reference/delivery-guarantees/),
[configurable retention](https://developers.cloudflare.com/changelog/post/2025-02-14-customize-queue-retention-period/),
[dead-letter queues](https://developers.cloudflare.com/queues/configuration/dead-letter-queues/),
[authenticated pull and acknowledgement](https://developers.cloudflare.com/queues/configuration/pull-consumers/).
