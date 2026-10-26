# Local store encryption

Agent uses a fresh `store-v1` namespace. Previous Agent/Research data is neither
read, adopted, migrated nor deleted by this implementation. Plain mode remains
the default. Password mode encrypts the SQLite database, WAL, attachment metadata
and other database records with a random local DEK wrapped by the password.
Attachment payloads, including images and tool screenshots, are ordinary
session-owned files outside that encryption boundary. They remain plaintext
even while the database is locked. See [local storage](local-store.md) for
their lifecycle, migration and backup rules.

## Release exposure

`store_status.password_settings_available` is true only in debug builds. The
host also enforces that gate for creating or changing password protection.
Unlock, password removal, erase and start-fresh remain available.

An already-open store can acknowledge a repeated setup request in a release
build: password mode requires current-primary authentication against the exact
installed generation. This dedicated path cannot create or change a store.
Plain SQLite remains the release path; it uses the same store and requires its
own cross-platform qualification. Password creation/change and experimental
salvage remain host-enforced debug-only. Unlock and explicitly destructive reset
remain available in release builds.
In-place index/timeline repair of a damaged source is not offered. The hold
remains until native macOS/Linux/Windows validation, ordinary Quit and
Dock termination deferral, crash recovery, hardening and performance gates are
complete. Successful source compilation alone does not lift it.

Confirmed Start fresh/Erase destroys the current store rather than creating a
permanent damaged-original archive. A salvage candidate may retain the source
only while uncommitted or recovering a crash; after verified installation the
old generation is resumably removed before completion. Temporary swap state is
not a backup feature, and no previous-version or unrelated-copy sweep is added.

## Host lifetime and shutdown

Password mode starts locked. Only the local unlock/status/reset controls are
admitted. ACP fails with the unlock instruction, and no Iroh endpoint is bound.
`maple-agent unlock` prompts on the controlling terminal, sends the password
over private local IPC, then detaches. Passwords are not accepted in arguments
or environment variables. An open password store stays resident until Quit or
`maple-agent host stop` completes.

A Stop acknowledgement means the request was accepted. Completion requires
accepted calls to drain, remote and ACP connections to end, source history to
flush durably, integrity children to be reaped, database connections to close,
keys to be wiped and the authoritative owner lock to be released. Credential
reconciliation saves the current SDK revision, including earlier failed token
rotation or sign-in writes, and completes retained exact-owner logout deletion
before runtime teardown. A failed flush retains ownership and pending session
source in a stopped, nonrunnable runtime. Retry first saves that source durably;
normal operation then uses a fresh runtime rather than reviving a cancelled
lifetime. Remote serving is restored when failed Stop leaves the host open.
Spill cleanup precedes native close; a refused final WAL checkpoint retains the
native connections and locked key, restores one checkpoint worker and keeps the
store open for retry. Other failures expose the actual store state. The caller waits for connection
closure and probes the owner lock; an ordinary Quit cannot claim success while
that owner remains.

Each local connection is bound to the verified store identity from its handshake.
Queued operations check that identity after admission, so they cannot mutate a
replacement after erase/setup. A changed identity closes the connection after
its result; reconnect drops old pending and unsent requests.

## Source and provider verification

The static provider is SQLCipher 4.19.0 / SQLite 3.53.4, with OpenSSL 4.0.3 from
`openssl-src`. A revision-pinned public `MaplePrivacyLabs/rusqlite` fork carries
one focused `libsqlite3-sys` patch above the release base, while rusqlite 0.40.2
remains the released registry crate. The dependency source is fetched by Cargo
rather than copied into Maple. Exact Git/registry pins, source hashes, signed
tag/commit/key pins and reproduction commands live in
`dependencies/sqlcipher/README.md` and `dependencies/sqlcipher/provenance.json`.
The old codec amalgamation retained in upstream's tree is not compiled. There
is no beta or silent alternative crypto-provider fallback.

From the Agent's pinned Nix shell:

```sh
python3 scripts/audit-sqlcipher-dependencies.py
python3 scripts/generate-third-party-notices.py
python3 scripts/generate-third-party-notices.py --check
```

The dependency audit evaluates each supported target separately. Combining
macOS's native-TLS activation with Linux's backend selection creates a false
OpenSSL edge. The build verifier additionally checks each target's actual Cargo
feature graph. The signed-source verifier regenerates the amalgamation and
compares its bytes, allowing only the source crate's documented WASM guard.
CI is configured to run released rusqlite's suite against this static provider
on all three platforms. Local macOS results do not substitute for Linux or Windows.

On 2026-10-21, the local pinned macOS shell passed the released rusqlite 0.40.2
`modern-full` suite against the patched provider: 334 passing tests and three
ignored doctests. Source/hash checks, all three target feature-graph checks,
target-specific advisory/banned-provider checks and generated-notice checks
also passed. These are local runtime and source-graph results; Linux/Windows
native linking, packaging and lifecycle evidence remain separate release gates.

For isolated same-machine qualification, `MAPLE_REMOTE_BIND_LOOPBACK=1` is
accepted only by an unpackaged debug build using a Local HTTP loopback API,
`MAPLE_REMOTE_RELAYS=none`, and no relay-only override. Invalid combinations
fail explicitly. The endpoint then binds only `127.0.0.1` on an ephemeral port;
the driver must verify that every published direct hint is loopback and that
there are no relay hints. This exercises actual Iroh pairing and control
between separate local hosts without changing the VM firewall. It does not
qualify LAN reachability, relay routing, or packaged network defaults.

## Password and process handling

Argon2id work is serialized and runs on blocking workers. Slot bounds and the
database salt are validated before calibration or deriving a key. Calibration
is cached per host. There is no attempt counter or extra lockout delay.
Generated suggestions stay host-owned and carry an opaque receipt; only the
unchanged suggestion receives its generated strength classification.

The application's DEK and temporary raw-key representation use dedicated locked
mappings. Initial keying calls the C API directly. Export attaches its destination
using a borrowed `SQLITE_STATIC` BLOB; the native statement is finalized inside
the mapping's callback before the mapping is wiped. This avoids rusqlite's usual
transient binding copy and the conversion of a text parameter. SQLCipher still
owns its native codec context. See the [SQLite binding lifetime contract](https://www.sqlite.org/c3ref/bind_blob.html)
and [SQLCipher memory-security defaults](https://www.zetetic.net/sqlcipher/sqlcipher-api/#cipher_memory_security).

The host and desktop apply core-dump hardening before accepting secrets. Linux
also disables dumpability; Windows registers the executable's WER exclusion.
`MAPLE_ALLOW_DEBUG_ATTACH=1` is honored only by debug builds. IPC buffers and
owned sensitive parameters use wiping guards, including rejected requests and
cancelled queued work. Client reply JSON stays guarded during parsing, queueing
and delivery. Small secret replies use borrowed decoding so the original JSON
is wiped after success or a schema error; ordinary history pages transfer their
owned values without cloning. Generated phrases become a wiping typed secret.
Wiping remains best effort: libraries, allocators, the
UI and the operating system can retain copies.

Readonly checker children have an independent watchdog: Unix checks the parent
PID, and Windows retains a parent process handle validated against its creation
time before reading any key. The child reads DEK bytes from a duplicated OS
stdin handle directly into locked, wiping key memory, bypassing std's global
buffered stdin. Completed Stop also cancels and reaps the child.
Native Windows abrupt-parent qualification remains pending.

Full integrity checks keep the database connection read-only. Alongside SQLite
structure, SQLCipher authentication and foreign-key checks, the checker scans
the supported schema's application-owned `CHECK` predicates: SQLite's read-only
schema loading omits their expression trees, so `integrity_check` alone cannot
validate them. The supplemental queries use `NOT INDEXED` and a fixed,
versioned catalog compiled into Maple. They do not execute SQL taken from the
checked database's schema, and never reopen it writable. Catalog tests compare
every predicate with Maple's trusted migrations for each supported schema
version. The scans share the check's progress handler, deadline, cancellation
and finding bound.

Operational failures report `partial` / `checker_unavailable` without a clean
receipt; deadline and cancellation retain their distinct incomplete reasons.
`SQLITE_CORRUPT`, `SQLITE_NOTADB` and confirmed structural, authentication,
foreign-key or constraint findings remain damage, including when a subsequent
stage is unavailable. Only a complete clean full check can produce a receipt,
which the host must still validate against the current store generation before
recording it.

Encryption protects copies of the locked store. It does not protect an unlocked
host from code running as the same user. Windows permits same-user process
memory reads. SQLCipher's decrypted page cache, swap, hibernation and OS backups
remain subject to their respective platform protections. A password change
preserves remote pairing; erase creates a fresh local identity and grants.

Desktop notifications in password mode omit task titles and question text by
default. General Settings offers an explicit **Show task details in password
mode** opt-in; the operating system can retain that text after Maple quits.
Plain mode retains its existing detailed notifications. An unavailable verified
store mode keeps notification details private. This policy belongs to the local
computer even when a remote computer runs the task.

## Notices

The About pane opens the native package's third-party notices. Complete notices
are generated offline into ignored build output for that application's target
and features, then distributed alongside the binary and EFF wordlist. Ordinary
unpackaged builds use a small source/attribution page without invoking a license
generator during Cargo builds. Regeneration and provenance details are in
`app/assets/licenses/README.md`. The inventory retains compiled C build
dependencies and the existing upstream licensing distinctions. SQLCipher
attribution comes from the exact compiled `sqlcipher-src` license, rather than
only the fork's uncompiled older codec license; generation checks assert that
compiled-source attribution is present.
