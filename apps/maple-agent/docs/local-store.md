# Local storage and unlock passwords

Maple Agent keeps this version's application state in a fresh `store-v1/`
directory. The host owns one SQLite database for each build profile, with
account-scoped records inside it. Desktop, ACP and local administration commands
use the host rather than opening the database themselves.

Plain SQLite is the release path. Password creation/change and experimental
salvage are host-enforced debug-only controls
until native cross-platform behavior, ordinary Dock Quit, low-end Argon2 costs
and the remaining release qualification have been demonstrated. Source tests,
benchmarks and a local build establish different evidence from those native
checks. This document describes the implemented storage contract without
claiming that password mode is ready for broad release.

## Scope and location

The profile namespace is `maple-agent` for unpackaged builds,
`maple-agent-dev` for packaged Dev, and `maple-agent-prod` for packaged Prod.
They have separate stores, accounts and unlock passwords. Choosing a debug
binary does not select Local: use the intended managed workspace and its
launcher/environment for Local, or deliberately select hosted Dev or Prod.

| Platform | Default unpackaged store |
| --- | --- |
| Linux | `~/.local/share/maple-agent/store-v1/` |
| macOS | `~/Library/Application Support/maple-agent/store-v1/` |
| Windows | `%LOCALAPPDATA%\maple-agent\store-v1\` |

An absolute `XDG_DATA_HOME` replaces the platform's data-directory base on all
three platforms. It does not replace the profile namespace. `XDG_CONFIG_HOME`
selects instruction/configuration resources, not the application database.
Managed workspaces set separate roots; preserve their generated configuration.

Keep the store on a local filesystem. Detected network filesystems and virtual
mounts such as network-backed FUSE are refused before the database opens; the
error names `XDG_DATA_HOME` so the profile can be placed on local disk. Filesystem
detection is not a guarantee about every possible mount implementation.

```text
store-v1/
  owner.lock                 host's lifetime OS lock
  db/
    generation.json          random store/generation identity
    maple.db                 application database
    maple.db-wal             SQLite WAL, when present
    maple.db-shm             SQLite coordination file, when present
    keys.json                password-wrapped data key, password mode only
    keys.backup.json         second key-file copy, password mode only
  logs/maple-agent.log        plaintext operational diagnostics
  attachments/               plaintext session-owned files and images
  resources/                 generated instruction Markdown, skills and prompts
  setup.json                 only while an erase/setup operation is unfinished
  db.tmp/ db.new/ db.old/     temporary generation replacement/recovery only
```

Directories and files are private to the OS user. Unix opens reject unsafe
ownership, permissions, links and nonregular database files. Windows creates a
protected owner-only directory ACL. The authoritative lock is inside the store,
so removal of a temporary IPC socket cannot create a second database owner.

New database state includes task metadata, complete source entries, derived timeline
rows, attachment metadata, preferences and dictionary words, sign-in credentials,
MCP definitions/secrets, send receipts and remote identities/grants. Sign-out
keeps local history. Account deletion removes only that account's new-store
records and owned attachment files after server success is confirmed. If the outcome is ambiguous, Maple
keeps the data and pending deletion intent. Automatic reconciliation is not
implemented in this version; ordinary sign-in may remain blocked by that intent.
An authentication failure by itself does not prove that the server account was
deleted.

Previous application data is untouched. This version does not inspect, import,
adopt, rename, copy, delete or fall back to old settings, credentials, sessions,
attachments, remote identities or temporary spill files. Existing installations
therefore begin with fresh application state in this namespace. There is no
migration or automatic cleanup of previous data.

Within `store-v1`, attachments use ordinary session-owned files. Attaching makes
a stable copy on the execution computer, including when the desktop is local.
Moving or editing the original does not change an accepted attachment. The
database keeps ownership and metadata; normal files and archives are given to
the model by their execution-side path. Archives are not automatically unpacked.
Images retain the model's existing vision and `read_image` behavior.

Before sending, the desktop keeps a temporary plaintext copy under its profile's
`draft-attachments/` directory, outside `store-v1`. This lets a draft survive
changes to the selected original and lets an uncertain upload be retried with
the same operation identity. Live copies are released when the draft is removed,
sent, signed out or its view is torn down. A bounded sweep removes unlocked
crash leftovers after seven days. Store reset does not manage this desktop-owned
temporary directory, and database encryption does not protect these copies.

Deleting a session schedules deletion of its owned files; interrupted cleanup
retries without starving other sessions. Ordinary read-only owned folders can be
reclaimed, but inaccessible permissions or foreign ownership still require repair;
Maple does not follow links or report incomplete erasure as success.
Compaction alone retains historical attachments. A stopped-store backup
must include both the database and the attachment tree. Unlock passwords do not
encrypt these files, and no password change converts them to encrypted payloads.

The attachment schema migration retires the former SQLite image payloads instead
of exporting them as plaintext. Conversation text and tool results remain, with
explicit unavailable-image markers where needed. Existing captions are retained
when present; an image does not necessarily have a caption. This migration is
restartable and does not reset the database. It cannot be undone by launching an
older binary, and reclaimed SQLite pages may not immediately shrink the database
file. Existing backups and filesystem snapshots are not erased.
Opening or continuing an older task prioritizes rebuilding its derived history.
If that bounded preparation is still pending, Maple reports it instead of showing
an empty conversation. Missing historical image files become unavailable; new
outgoing images still require successful preparation.

General project/workspace Markdown, `AGENTS.md`/`CLAUDE.md`, `SYSTEM.md`, skills
and prompt templates retain their resource-loading behavior. They remain files
and are outside database encryption and application-data erasure. Newly
generated delegation resources use the new `resources/` location; this does not
authorize reading old application JSON or databases.

Opening third-party notices reads the public HTML shipped beside the executable.
An unpackaged developer binary instead writes its small embedded source and
attribution page to a fresh random temporary file for the browser. Neither page
contains account or task data; they are outside the encrypted store.

## History display and replay limits

Desktop and ACP page through saved history rather than loading an entire task.
Paging supports long histories, but each interface has a limit for reconstructing
one complete timeline row from its serialized JSON:

| Interface | Full-row reconstruction limit |
| --- | --- |
| Desktop **Show full output**, local or remote | 32 MiB; the visible transcript also shares a 32 MiB view budget |
| ACP history replay | 64 MiB per row |

A row that exceeds an interface's limit produces an explicit error. It does not
cause the host to prune, truncate or replace the saved source. Ordinary history
pages may contain shortened display copies marked with their original size.
The authorized `read_row` and attachment APIs transfer at most 1 MiB of raw data
per range, before base64 encoding; they are separate from the desktop's full-row
layout and ACP's reconstruction limits. A range limit does not mean the host
can reconstruct an arbitrary row using only that much working memory.

These are interface limits, not new model-input or tool-output policies. Existing
input and attachment admission limits, tool-output truncation and temporary
spill behavior still apply before content becomes saved history.

## Plain and password modes

Plain mode uses the same SQLite/SQLCipher engine without a database key. OS file
permissions protect the files from other ordinary OS accounts; their contents
are not application-encrypted. A fresh profile begins in plain mode.

In password mode, SQLCipher encrypts the current database and WAL. A random data
key is wrapped using an Argon2id-derived password key. One authoritative host
owns the store. Its bounded, read-only integrity-check child may receive the key
over private stdin; cancellation or a deadline kills and reaps that child.
Desktop and ACP never open the store or receive its data key. The password is a
local unlock password, separate from the Maple account password. It is not saved in
an OS keychain, passed in argv/environment variables, or sent to a remote peer.

The password protects the current database while its host is stopped. It does
not encrypt attachment files, including images and tool screenshots. It does
not protect against code running as the same user while the host is unlocked,
root/administrator access, screenshots, clipboard contents, the project files
tools edit, instruction files, or copies kept by external agents. File sizes,
generation/key metadata, directory names and operational logs remain visible.
Page encryption does not provide whole-file rollback protection.

Setting the first password copies the live database into a fresh encrypted
generation. Removing the password copies it into a new plain generation. Both
need room for a second database and may take time; the UI reports progress and
offers cancellation before the commit point. Leave Maple open if it reports
that key-file synchronization, its backup copy or final recovery is pending.
After a password change warning, keep both passwords until synchronization is
confirmed.

A normal password change rewraps the existing data key. The **Also create a new
encryption key** checkbox, off by default, additionally rewrites the current
database using a new random data key. Neither operation changes existing backups, filesystem
snapshots or previous app data. An old backup still requires the password and
key files belonging to that backup; changing today's password cannot revoke
those older copies.

The form starts with empty, labeled password fields and requires confirmation.
**Generate passphrase** explicitly requests six randomly selected words; no
suggestion is generated just by opening the form. Save explains missing fields,
password-policy failures and confirmation mismatches without consuming the
inputs. Your own password's strength assessment uses zxcvbn with Maple and
account context, with no additional password corpus or dictionary-phrase cap.
It rejects fewer than 15 normalized Unicode characters or an estimated guess
count below 10¹², warns below 10¹⁴, and labels higher estimates strong. Passwords
are limited to 1,024 normalized characters; zxcvbn bounds its work by assessing
the first 100. These are estimates, not guaranteed offline attack costs.
There are no mandatory character-composition rules or password-attempt counters.
Set and unlock normalize Unicode consistently. Argon2 parameters are calibrated
for the machine, targeting roughly one second within explicit memory/work
bounds. Newly created slots can need up to 1 GiB of working memory, plus normal
application overhead, when unlocked on another computer. Low-end latency and
memory qualification remain a release gate; the UI reports insufficient memory
rather than silently reducing the saved slot's protection.

## Unlock, Close and Quit

When a password store starts locked, the host answers local status/control
requests but does not load credentials, tasks, remote identity or ordinary
settings. Unlock locally in the desktop app, or use a controlling terminal:

```sh
maple-agent unlock
maple-agent login       # only after unlock, if sign-in is needed
maple-agent acp
```

`unlock` reads a hidden password prompt from the controlling TTY. It takes no
password argument or password environment variable. `login` does not unlock the
store. ACP cannot prompt over its protocol stream: unlock first in another
terminal or the desktop app. Its stdout remains ACP-only.

An unlocked password host stays resident without attached clients. Closing its
window hides/backgrounds Maple and keeps work and remote access available. On
Linux or Windows without a usable tray, Maple offers Keep open or Quit rather
than leaving an inaccessible background UI. There is no idle lock, screen-lock
hook, timer, or separate Lock action. Use the OS screen lock for an unattended
unlocked session.

Ordinary app-menu, keyboard, Dock and tray Quit enter the same shutdown
coordinator. Maple checks active tasks/editors/controllers, asks for confirmation
when appropriate, flushes saved state, stops the host and waits for ownership
release before completing Quit. A shutdown failure keeps the frontend open
with a warning/retry instead of claiming the database is locked. The terminal
equivalent is:

```sh
maple-agent host stop
```

Run that command through the exact managed-workspace launcher when using an
MDE, and stop the development host before rebuilding or launching another
version. A Stop acknowledgement alone is not completed shutdown. Force Quit,
crashes and forced OS termination are separate: terminating only the frontend
can leave the host running and unlocked.

Plain mode retains the ordinary attached-client/idle policy unless remote
access keeps it resident. Closing a plain, nonresident window saves UI settings
and detaches the frontend, leaving other attached clients running. Explicit
Quit/host stop still stops the local host.
On Linux, logout or SSH exit may leave the process alive according to the OS's
user-session policy; Maple does not change that policy. Quit or stop the exact
host before logout when locking is required. A reboot ends key-holding processes
and returns a password store to locked startup; forced termination does not
guarantee a graceful tool drain.

The process disables ordinary core dumps/crash snapshots where supported and
keeps explicit key material in locked, zeroizing memory. Decrypted database
caches and all application text are not fully memory-locked or provably wiped.
Swap, hibernation, administrator-created dumps and compromised processes remain
OS-level concerns. Full-disk encryption and the OS's swap/dump policy apply to
those copies. Debug-only `MAPLE_ALLOW_DEBUG_ATTACH=1` deliberately bypasses the
host's dump/debug-attachment hardening for local debugging.

Remote controllers cannot unlock, change passwords or erase this computer's
store. A locked execution host does not restore remote serving until a local
unlock has opened its credentials and identity. See [remote access](remote-access.md).

## Save failures and retries

When a save runs out of disk space, Maple keeps the store open and retains
unsaved task source in its loaded session. Persistent feedback beside the chat
composer and in **Privacy & Security** asks you to keep Maple open, free disk
space and choose **Retry saving**. An unhandled failure also opens a focused
warning dialog; routine polling does not repeatedly open it. That action flushes
pending desktop preferences, sign-in state and loaded task source without
stopping running tasks. The warning clears only after a successful retry;
unrelated successful writes cannot clear it. A remote controller shows the
warning for its connected computer; retry saving from Maple on that computer.

If the disk fills before Maple can open its store, the store remains
unavailable for normal access rather than being labeled corrupt. Free disk
space, quit and reopen Maple, then unlock again if prompted. Live **Retry
saving** is available only after the store opens. A failed save is not a saved
message acknowledgment, and Force Quit or a crash can lose the source still
held only in memory.

## Manual backup and restore

There is no application backup feature. Saved authoritative writes use WAL,
`synchronous=FULL`, and platform full-sync settings. Checkpoints run separately
from commits. These settings protect acknowledged commits; they do not make a
live file-by-file copy of SQLite, its WAL and its key files an atomic backup.
An inconsistent live copy can be corrupt and is not promised recoverable.

For a manual copy of this version's store:

1. Finish any password/erase/setup operation and resolve pending synchronization
   warnings. Close desktop/ACP clients and stop the exact profile with
   `maple-agent host stop`. Wait for successful completion/ownership release.
2. Keep every client closed throughout the copy. An automated copy helper must
   hold `store-v1/owner.lock` for the entire copy, using the same OS lock, so a
   new client cannot start a writer halfway through it. Ordinary file-copy
   commands alone do not acquire this lock.
3. Copy the complete `store-v1/` directory to a separate destination. Include
   the complete `db/` generation, any WAL still present, both key files when
   present, `generation.json`, and the complete `attachments/` tree. Never combine a database and key/generation
   files from different copies. Preserve private permissions and appropriate
   ownership; attachments, instructions/resources and logs remain plaintext.
4. Verify the copy independently while retaining the original. A file-transfer
   success is not an integrity or password-unlock check. Reopen the original
   only after the copy has finished.

Restore with the destination host stopped and clients closed, as a complete
matching generation rather than individual replacement files. Preserve the
original source you chose to preserve until the restored copy is checked. A restored
profile retains its remote identity; avoid running duplicate restored identities
as independent computers. Do not edit SQLite/key/setup files by hand to bypass
an ownership, generation or wrong-password refusal.

## Forgotten passwords, erase and damage

Maple cannot reset a forgotten local unlock password. **Forgot password?** opens
**Erase this version's local data**, requiring the exact text `ERASE`. This
removes this version's saved app-state database and owned attachment files,
including tasks/history, settings, sign-in, MCP secrets and this store's local remote
pairings. It does not delete a server account, other computers' state, external
agents' own copies, backups, snapshots, old app data, or the instruction-file
exception above. Setup then requires an explicit new password or **Continue
without a password** choice.

Erasing a computer does not revoke the stale peer record on your other
computers. Remove it from **Remote computers** there. Sign-out is different and
retains this computer's local history and pairing records.

Integrity checks distinguish confirmed damage from a check that could not
complete. An unavailable checker, timeout, cancellation or operational read/lock
failure is incomplete, rather than evidence of corruption by itself. It cannot
produce a clean full-check receipt or advance the completed-check timestamp.
Confirmed damage remains damage even if a later check stage cannot complete.
Damage observed during ordinary reads, writes or checkpoints also quarantines
the current generation durably; restarting does not make it writable merely
because a bounded startup check failed to rediscover that damage.

A damaged database is quarantined rather than silently replaced. The damage
screen offers a content-free diagnostic report and integrity checking. There is
no in-place index/timeline repair of the only damaged source. Experimental
**Save what can be read** remains debug-only: its preview identifies readable
and dropped records, and commit requires `SAVE READABLE DATA`. A verified
replacement installs through the crash-safe generation protocol. The old source
exists only as temporary build/recovery state and is resumably deleted before
that operation reports completion; Maple does not keep a permanent damaged
backup. Make your own stopped copy first if you want to retain that source.

**Start fresh** requires exactly `START FRESH` and explicitly confirms destruction
of the current damaged store. It deletes that current generation and starts a
new empty store; it does not archive it under `db.damaged-*` or
`db.unclassified-*`. Erase is likewise explicitly destructive. Only qualified
temporary `db.tmp`/`db.new`/`db.old` generations remain while a replacement or its
cleanup is incomplete. No old-version data or unrelated copy directory is
scanned, adopted or swept. Repeated or interrupted reset/setup uses operation
and generation identity so recovery cannot discard a replacement that already
accepted new work.

Authenticated repair of generation identity metadata, where available, is
separate from repairing content in place: it verifies the current encrypted
database with the existing password and does not replace history or set a new
password. Recovery controls do not claim broad release qualification merely
because an action is visible in a debug build.

After an encryption mode change, a content-free notice remains in the store
across host and desktop restarts until acknowledged. A successful local Settings
operation stays on that page with an inline success message and acknowledges
only the exact notice returned with that operation. Unobserved changes and
failed acknowledgements retain the recovery notice in **Privacy & Security**;
errors and pending key-file synchronization stay in their form or a focused
warning dialog when that form is unavailable. Setup, reset and repair success
return to their normal destination without a global completion notice. Store
feedback never adds rows above the active page. A stale window cannot dismiss
a notice from a replacement store.

## Contributor contract and performance evidence

Only the authoritative host and its bounded read-only integrity checker open
production stores. Keep raw connections/transactions in
`crates/maple-agent/src/store/`, run blocking operations away from the UI, use
short snapshots and `BEGIN IMMEDIATE` for writes, and do not retain a reader
across an await, IPC write or UI frame. Authoritative source writes remain
Durable. Entries are append-only except for explicitly authorized deletion.
Append schema migrations; changes to item builders/derivation require a
`TIMELINE_FORMAT` bump and plain/keyed parity checks.

Generation changes revoke cached account/task-store handles; numeric account IDs
cannot carry ownership into a replacement database. Credential writes compare
the exact stored owner even when its row is hidden from the current API view.
Image payloads are never retained as inline database fallback data. Invalid or
unavailable media becomes an explicit unavailable marker without changing a tool
result's success status. Valid image references are resolved only when needed
for a preview or effective model context; ordinary history reads do not load
every image payload.

Idle maintenance reclaims at most 64 freelist pages per chunk and periodically
runs bounded `PRAGMA optimize`, yielding to foreground work. Idle full-integrity
attempt admission is persisted before the child starts. Incomplete attempts
back off from five minutes up to 24 hours across host restarts; their finite
budgets grow from 30 seconds to at most five minutes. Only a complete clean
check advances the clean-check timestamp and clears that retry state.

Disk-full regression tests use the actual SQLite session adapter. A constrained
`max_page_count` checks rollback during a statement, while an isolated write fault
on the writer's WAL handle checks `SQLITE_FULL` during COMMIT after the statement
has succeeded. Both verify rollback and absence of saved-change notifications;
the session regression verifies retained source and a successful retry after
capacity is restored. These engine tests do not establish native feedback behavior
or real operating system disk exhaustion; those require exact-app
validation on a constrained filesystem.

The [store benchmark](../crates/store-bench/README.md) records the full schema,
real projector/cache work, effective durability, versions, load and output
digests. Its runner references begin explicitly pending until actual completed
runner results are recorded. Historical NORMAL-mode timings are not evidence
for Durable append latency and must not become a reason to weaken saved state.
