# App transcription

`POST /v1/audio/transcriptions` retains the SDK's encrypted JSON request and
`text` response contract. It requires a user JWT and a paid-plan entitlement for
both registered and guest users. API keys are rejected. Transcription is included
in the plan: remaining chat credits do not control access and this route publishes
no token-usage debit event.

OpenSecret sends the whole recording to the configured attested Continuum proxy
using `whisper-large-v3`. There is no enclave provider fallback, chunk splitting,
text stitching, or automatic POST replay. The proxy continues to own its existing
attestation, encryption, and transport recovery; one enclave send is not an
end-to-end exactly-once execution guarantee.

## Bounds and validation

- Maximum decoded upload: 39,000,000 bytes; maximum recording: 20 minutes.
- WAV input is validated from RIFF chunks, PCM/float format, consistent frame
  sizes and rates, and the actual data length before a provider request.
- Exact digital silence in validated WAV is rejected before sending. There is no
  volume threshold: even one nonzero PCM bit remains eligible, so quiet speech
  is not filtered out.
- Supported compressed containers remain bounded passthrough inputs. Their
  decoding and duration are provider-owned; an over-duration response is rejected
  after processing and may already have incurred provider cost.
- At most one active transcription per user and four per backend process. There
  is no unbounded waiting queue. Cancellation releases admission.
- Billing lookup has a five-second deadline and fails closed when unavailable.
- The complete provider send and response read share a 120-second deadline.
  Responses are limited to 1 MiB and require a nonempty text string and validated
  duration evidence. Known WAV durations must agree within one second for rounding.
- Missing or malformed text, empty transcription, inconsistent duration,
  oversized response, provider failure, and timeout have stable sanitized errors.
  An empty response is not a successful transcript.

The provider/model, audio size, duration, elapsed time, and returned billed seconds
(when supplied) are safe operational metadata. Audio, transcripts, filenames,
prompts, credentials, and arbitrary provider diagnostics must not enter logs.

## Existing-client limits

The app records 16 kHz mono PCM16 WAV. Ten minutes is about 19.2 MB of audio;
twenty minutes is about 38.4 MB. Both fit the SDK's 50 MiB Transport V2 JSON-body
limit after base64 encoding. Thirty minutes is about 57.6 MB of audio and 76.8 MB
of base64, so the existing SDK rejects it before upload. Enclave-side compression
or splitting cannot change that ingress limit.

Provider duration agreement confirms reported audio processing, not transcript
accuracy or completeness of recognized speech. Use the real-audio validation
harness with ordered beginning, middle, and end anchors to check those properties.
See [transcription validation](transcription-validation.md).

Local API/provider tests do not establish hosted edge timeouts, Nitro/PCR identity,
or recovery after app closure or loss of the downstream response. Validate the
actual hosted request deadline separately before rollout; increasing the enclave
timeout alone does not increase an upstream gateway's deadline.
