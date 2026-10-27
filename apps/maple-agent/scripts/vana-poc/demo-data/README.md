# Synthetic Alex Rivera vault fixtures

Every record is fabricated. No real ChatGPT, wearable, personal, work, or GitHub account was used. Alex Rivera is a fictional privacy and AI engineer preparing an eight-minute DC talk on October 15, 2026. The demo is about portability and permissioned reads; the wearable fixture supports descriptive summaries, not medical claims.

| Import body | Fixture scope schema | Records |
| --- | --- | --- |
| `chatgpt.conversations.json` | ChatGPT Conversations 2.0.0 | 26 threads, 104 messages |
| `chatgpt.memories.json` | ChatGPT Memories 2.0.0 | 12 saved memories |
| `oura.sleep.json` | Oura Sleep 1.0.0 | 14 daily scores, 14 sleep periods |
| `oura.readiness.json` | Oura Readiness 1.0.0 | 14 daily scores |

Oura spans September 30 through October 13. Sleep stages sum to total sleep, sleep plus awake time equals time in bed, timestamps match durations, and ChatGPT counts match the message arrays. The fictional recent sleep pattern does not establish diagnoses or causes.

`chatgpt-export.json` and `oura-export.json` follow the bundled collectors' canonical result envelope, with synthetic disclosure in permitted `exportSummary.details`. They are optional native export artifacts; the four raw scope bodies are for `POST /v1/data/{scope}`.

On October 13, 2026 at 4:30 PM PDT, the four authenticated owner API imports each returned HTTP 201 after server health and identity checks. The actual Vana native Home screen automatically showed **68 Memories, 3 Sources, and 1 App**. The ChatGPT source exposed two data tabs with the imported JSON, and the Oura source card appeared. No native export registry mutation was needed. These observations verify native visibility of the imported synthetic data; they do not imply a real ChatGPT or Oura account connection.

These fixtures were validated against Vana 1.53.1's ChatGPT and Oura connector manifests and scope schemas. No Whoop scope schema was found in that bundle. WHOOP was nevertheless offered by the native Sources catalog. This is historical schema evidence, not a claim about the currently installed Vana version; revalidate against its actual schemas before a new import. The new local OAuth integration does not reimport or reset this data.

## Historical fixture maintenance (manual)

These commands were originally run with Node 24.12.0 against the matching
Vana 1.53.1 schemas. They are not required for connecting Maple to Vana and
are not part of Agent CI. The validator uses the older bundle layout; supply
the matching original schema files before running it.

From this fixture directory:

```sh
node generate.mjs
node validate.mjs
node --test fixtures.test.mjs
```

Validation uses Ajv and ajv-formats from the Vana Personal Server bundle and requires the matching connector schema files from the original fixture version. Newer Vana releases can change their location or schema. No live API, signing key, or account token is touched by these commands.

Suggested main demo prompt:

> Use my Vana ChatGPT conversations and memories to help me prepare my eight-minute DC privacy talk in two days. Give me three priorities in my preferred style, mention the local transport caveat, and cite the vault records you used. Do not access health data.

Optional separately granted health prompt:

> Using the explicitly shared Oura sleep and readiness fixtures, summarize the recorded date range and recent sleep duration trend. Connect it only to my stated scheduling preferences; do not make medical conclusions. Cite the scopes.
