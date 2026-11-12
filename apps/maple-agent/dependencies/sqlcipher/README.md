# SQLCipher provider pins

Maple uses released rusqlite 0.40.2 with libsqlite3-sys 0.38.2 from the public
[MaplePrivacyLabs/rusqlite fork](https://github.com/MaplePrivacyLabs/rusqlite/tree/853a8daf0f20b47123d228762490d2bdd52ad77c).
The workspace pins commit `853a8daf0f20b47123d228762490d2bdd52ad77c`, one signed
patch above upstream release commit `e88f112bef7899234a497baed5cc3c3d553deeb8`.
The functional delta is confined to the crate manifest and build script; its
small README documents the adaptation and removal criteria. Maple does not
copy the upstream dependency's source tree into this repository.

The patch backports source selection from
[rusqlite#1917](https://github.com/rusqlite/rusqlite/pull/1917) and adds the
`bundled-sqlcipher-openssl4` feature. It selects `sqlcipher-src =420.0.0`
(SQLCipher 4.20.0 / SQLite 3.54.1) and `openssl-src =400.0.2` (OpenSSL 4.0.3).
OpenSSL is built from source and its static crypto archive is linked after
SQLCipher. The route explicitly selects the OpenSSL provider and rejects
competing provider/linkage features and ambient pkg-config selection.
No SQLCipher or OpenSSL C source is modified by Maple. The fork retains
upstream's old bundled C sources, but they are not compiled on this route.

## Verify the selected sources

`provenance.json` records the Git revision and source hashes, exact registry
versions and archive checksums, and SQLCipher's signed release/tag/key pins.
From the Agent's pinned Nix shell, write Cargo metadata outside the repository:

```sh
cargo metadata --locked --format-version 1 > /tmp/maple-sqlcipher-metadata.json
python3 dependencies/sqlcipher/verify_sources.py --metadata /tmp/maple-sqlcipher-metadata.json
python3 scripts/verify-sqlcipher-build.py /tmp/maple-sqlcipher-metadata.json
```

The source check requires the exact Git source and revision in metadata and
the lockfile, verifies the fork crate's recorded files, then checks registry
checksums and selected C/header/license/version hashes. The build check also
requires released rusqlite and rejects a second `openssl-sys` provider in each
supported target's actual feature graph. These are source-selection checks,
not native runtime or packaging evidence.

## Independently regenerate SQLCipher

With the pinned compiler, Tcl, make, Git, GnuPG and Python 3.11 or later:

```sh
python3 dependencies/sqlcipher/verify_sqlcipher.py --metadata /tmp/maple-sqlcipher-metadata.json
```

The script uses a disposable directory and isolated keyring, fetches the signed
SQLCipher tag, checks both tag-object and peeled-commit IDs, and requires the
pinned primary signer without a revoked-key signature. It runs upstream
`configure` and `make sqlite3.c`, then compares C, public headers and the
SQLCipher license against the registry crate selected by Cargo.

No source-crate transformation is allowed: since SQLCipher 4.20.0 ships its
own wasm `.fini_array` guard, the source crate's
[pinned assembly script](https://github.com/LucaCappelletti94/sqlcipher-src/blob/617c0cc9673cadddd1beefda6db47aa732d5a7ab/tools/assemble.py)
copies the generated `sqlite3.c` unchanged.
The release key and signer pin come from that source crate's
[verification inputs](https://github.com/LucaCappelletti94/sqlcipher-src/blob/617c0cc9673cadddd1beefda6db47aa732d5a7ab/tools/releases.sh).
Review those trust inputs when changing the pin. Matching hashes alone do not
establish signature verification or independent regeneration.

## Maintain and retire the patch

Replace the source backport when a reviewed released libsqlite3-sys supports
the selected `sqlcipher-src`; replace the provider branch when a released
compatible route supplies the same explicitly pinned static OpenSSL behavior.
Keep any fork rebase as one focused commit and update the exact revision and
hashes here. Repeat source, native runtime, static-link and packaging checks
on macOS arm64, Linux x86_64 and Windows MSVC. Bindings retain an older compatible
ABI subset, so runtime version queries remain authoritative.

OpenSSL 4.0 is supported through 2027-05-14. If Linux or Windows cannot use this
route, explicitly select and pin a current OpenSSL 3.5 LTS source; do not
silently fall back to an older provider.
