# Transport V2 route and outcome diagnostics

Transport V2 carries every logical operation through one outer
`POST /v2/request`. That makes an outer edge failure ambiguous on its own: the
edge can observe a 5xx without knowing whether the record reached decryption,
which logical route ran, or whether the application returned a logical error
inside an outer `200`. These diagnostics provide bounded gateway-side evidence
when a request reaches the gateway. They cannot identify the inner route of a
particular edge 5xx without a trusted edge-to-enclave correlation key.

Source: `src/transport_v2/diagnostics.rs`, wired from
`src/transport_v2/gateway.rs`.

## Target and fields

Every per-request and aggregate event uses the dedicated target
`opensecret.transport_v2.diagnostics` and these stable field names:

| Field | Meaning |
| --- | --- |
| `kind` | `request` or `aggregate` |
| `stage` | `admission`, `envelope_decode`, `inner_dispatch`, `logical_response`, `encrypted_response`, `stream_terminal` |
| `route` | allowlisted route class, `unknown` before decryption, or `other` when no class matches |
| `method` | allowlisted HTTP method, or `other` |
| `outcome` | `admitted`, `rejected`, `decoded`, `dispatched`, `success`, `client_error`, `server_error`, `fallback`, `started`, `start_failed`, `ended`, `failed`, `disconnected` |
| `reason` | bounded failure reason, when one applies |
| `inner_status` | application HTTP status at `logical_response`; selected response-head status at later stages when known, including fallback 500 (not proof of delivery) |
| `outer_status` | outer transport status, present only when the gateway answers with an outer error: a rejection, or a response head that could not be sealed (`start_failed`, 503) |
| `window`, `total`, `suppressed` | aggregate counts (aggregate events only) |

`inner_status` and `outer_status` are deliberately separate fields. A logical
502 is `stage=logical_response outcome=server_error inner_status=502` inside an
outer `200`. The `/v2/request` gateway's outer rejections currently use HTTP
400, 413, 415, or 503; it does not generate an outer 502. An edge-generated
502 appears in edge telemetry, and may have no gateway diagnostic event.
Neither a logical 502 nor a gateway rejection establishes the cause of an edge
502.

If an application response head cannot be encoded, `logical_response` retains
the application's original status. An `encrypted_response` event records
`outcome=fallback`, `reason=invalid_response_head`, and `inner_status=500` for
the selected encrypted response. Later `started` and stream-terminal events
carry that selected 500 status when those stages are reached.

## Stages

| Stage | Emitted when |
| --- | --- |
| `admission` | a record is admitted, or rejected before decryption (content type, forbidden headers, query, oversize, session lookup, session store) |
| `envelope_decode` | the record fails incoming authentication, or the decrypted envelope is invalid, or a decrypted envelope classifies to a route |
| `inner_dispatch` | the logical request is handed to the application router, or it was handed over and never returned: `disconnected` (`request_future_dropped`) when the outer connection closed while the handler was still running, `failed` (`handler_panicked`) when the handler panicked |
| `logical_response` | the application returns, before response-head validation and encryption |
| `encrypted_response` | an invalid response head falls back to encrypted 500, the encrypted response head is sealed, or sealing it fails |
| `stream_terminal` | the encrypted body ends, fails, or is dropped before terminal |

Before the envelope decrypts there is no trustworthy `method` or `route`, so
those events carry `method=other route=unknown`. They never acquire an
inner-route label.

`disconnected` means the gateway stopped being read or waited on. At
`inner_dispatch` the outer connection closed before the application returned;
at `stream_terminal` the encrypted body was dropped before its terminal record.
Either can come from an edge or proxy timeout, a shutdown, or the client going
away. It does not identify which hop closed the connection. This is the gateway
event to look for when the edge reports a timeout or 5xx for a request that was
still running.

`logical_response` classes only 4xx as `client_error` and 5xx as
`server_error`; every other status is `success`.

## Route classes

Route classification matches only a fixed prefix table (`v1_chat_completions`,
`v1_responses`, `v1_conversations`, `v1_models`, `protected`, `platform`,
`oauth`, `auth`, `api_v1`, `health_check`, and the rest of
[`ROUTE_CLASSES`](../src/transport_v2/diagnostics.rs)). Dynamic path segments
and the whole query string are discarded, so a client cannot inject text into
`route`. Unmatched targets collapse to `other`, unknown methods to `other`.

## Volume control

Volume is controlled in code, not by `RUST_LOG` level. The source default is
`opensecret=debug`, so an operator cannot use level as the throttle.

The defaults below are compiled in and apply when the variables are unset,
which is how the enclave runs: nothing needs to be set for diagnostics to be
on. Inside the enclave the only place to set them is `entrypoint.sh`, which is
part of the measured image, so changing a value there means a new EIF and PCR
approval. They change how many lines are written, never what a line contains.

| Variable | Default | Effect |
| --- | --- | --- |
| `TRANSPORT_V2_DIAGNOSTICS_SAMPLE_PERMILLE` | `10` | per-mille sampling of per-request success events; `0` makes them opt-in |
| `TRANSPORT_V2_DIAGNOSTICS_EXCEPTIONAL_BURST` | `20` | per-`(stage, route, outcome)` token bucket size for exceptional events |
| `TRANSPORT_V2_DIAGNOSTICS_EXCEPTIONAL_PER_MINUTE` | `60` | refill rate for that bucket |

Aggregate counts advance for every event and are emitted once per 60 seconds
from the gateway maintenance loop, one line per non-zero
`(stage, route, outcome)`. Cardinality is fixed by the enums, so nothing is
dropped; suppressed events are counted, not hidden. A per-key line is written
only when its outcome survives sampling or the token bucket. The exceptional
budget is per key; there is no separate global log-line cap across keys.

Sampling is per event, not per request. A sampled success line is one stage of
some request; its other stages are usually not written, and with no correlation
key the lines cannot be regrouped into a request. Use the aggregates for rates
and the per-request lines only as spot evidence that a stage and route occur.

The deployed filter is separate from this policy. Confirm the running
enclave's `RUST_LOG` includes the target at `info` (for example
`opensecret.transport_v2.diagnostics=info`); the default `opensecret=debug`
also matches it.

## Correlation is explicitly unavailable

There is no opaque correlation key spanning edge and enclave diagnostics. The
only identifiers on this path — `X-Session-Id`, `X-OpenSecret-Routing-Key`, and
the client-chosen request ID — are client-controlled or transport material, and
the issue forbids trusting a client-supplied header or exposing raw transport
identifiers. Inventing a join from them would be spoofable. An edge event and a
gateway event therefore cannot be joined by key; use the time window, stage and
route class to compare aggregates, not to attribute an individual edge 502 to
an inner route. Treat the absence of a gateway event as unknown rather than
proof the request never reached the host.

## Operator query

The `fmt` layer writes `field=value` text. In CloudWatch Logs Insights, filter
on the target and parse the fields you need in a narrow time window. This
query needs only `info`, never broad `DEBUG`:

```text
fields @timestamp, @message
| filter @message like /opensecret.transport_v2.diagnostics/
| filter @message like /kind=request/
| parse @message /stage=(?<stage>\S+) route=(?<route>\S+) method=(?<method>\S+) outcome=(?<outcome>\S+)/
| filter stage = "admission" or stage = "envelope_decode"
| stats count() as events by route, outcome
| sort events desc
```

To separate an outer failure from a logical one in the same window:

```text
fields @timestamp, @message
| filter @message like /opensecret.transport_v2.diagnostics/
| filter @message like /outcome=server_error/ or @message like /outer_status=5/
| parse @message /stage=(?<stage>\S+) route=(?<route>\S+) method=(?<method>\S+) outcome=(?<outcome>\S+)/
| stats count() as events by stage, route, outcome
```

For encrypted 500 fallbacks caused by response-head validation,
filter this target for `stage=encrypted_response outcome=fallback`; the preceding
`logical_response` event may show a different application status. Use aggregate
counts to include fallback events suppressed by the per-key token bucket.

Then inspect `stage=aggregate` lines for the outcomes that sampling or the
token bucket suppressed:

```text
fields @timestamp, @message
| filter @message like /kind=aggregate/
| filter @message like /suppressed=/
```

## Privacy and retention

- These host-visible diagnostics intentionally disclose an allowlisted method,
  coarse route class, stage, outcome, bounded reason, numeric HTTP status, and
  aggregate counts. Someone with access to the logs can infer the operation
  class and result. This exception is part of the
  [V2 security boundary](transport-v2-protocol.md#security-boundary).
- Only the bounded vocabulary above is recorded. The raw target, query string,
  path parameters, headers, body, prompt, response, credentials, provider
  payload, user identifier, session identifier and transport request ID are
  never logged.
- `route`, `method`, `reason` and `outcome` are compile-time constants; no
  request byte reaches a field.
- Retention follows the host log group policy, not this code. Diagnostics add no
  separate store. Treat the aggregate counters as process-lifetime totals and
  the per-request lines as sampled evidence.
- These are diagnostics, not an audit trail. Do not use them to attribute a
  request to an account, and do not treat missing application events as proof a
  request never reached the host.
