These are public test fixtures, signed with the deterministic test-only seed
`[7; 32]` also used by `src/tests.rs`. This key is public knowledge and must never
be pinned in an application profile or used to sign a production release.

`manifest.json` contains the exact bytes authenticated by `manifest.minisig`.
`signed-envelope.json` embeds the same bytes and signature for client, Worker
and publisher contract tests. The artifact bytes are the ASCII string
`signed payload fixture bytes`; they are not an application archive.

The Linux v2 and Windows v3 fixtures use this same public key and payload. Their
platform-specific contracts are described in `linux-v2-contract.md` and
`windows-v3-contract.md`. Neither the Windows bundle nor installer fixture is
executable or establishes Authenticode trust.
