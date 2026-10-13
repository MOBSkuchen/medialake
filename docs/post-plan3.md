# MediaLake – File Sync Plan (v3)

Builds on `pre-plan3.md`, `plan1.md` (Windows app) and `plan2.md` (macOS port). Goal: MediaLake can host
folders and keep them in sync across paired devices in near real time, with conflict handling and backups,
next to (and independent of) the existing search.

Decisions (✔ = answered by you, ○ = my proposal, see §14 for the ones worth a second look):

| Topic | Decision |
|---|---|
| Topology | ○ **Hub and spoke.** One *host* owns the canonical copy of a sync; *members* sync against it. No peer-to-peer, no relay |
| Who can host | ✔ **Both** `medialake-server serve` (headless) and the desktop app, through one shared crate |
| New crate | ✔ `medialake-sync`: engine, host, member client, backups, platform frontends. Protocol types go into `medialake-proto` |
| Host storage | ✔ **Plain folder**, watched. An existing folder can be shared as it is, and edited directly on the host |
| Member files | ✔ **On-demand on both platforms**: Cloud Files placeholders on Windows, a FUSE-style mount with a local cache on macOS |
| macOS backend | ✔ **Decided by a spike** (§8.3). The mount sits behind one trait, so the choice does not touch the engine |
| Conflicts | ✔ Two policies: **server wins** and **ask**. The losing version is **always kept as a conflict copy**; nothing is discarded silently |
| Backups | ✔ **On the host, deduplicated** (content-addressed). Strategies: time-based snapshots, last N versions per file, none |
| Access | ✔ **Per sync, read-write or read-only**, granted per client and revocable on the host |
| Order | ✔ **Engine + CLI → macOS → Windows** |
| Change tracking | ○ Content hash (BLAKE3) + a per-sync **sequence number** on the host. `(size, mtime)` is only the cheap "did it change" filter |
| Transport | ○ bierpc as today: TLS, pinned certificate, `auth` in every action. File content and change notifications use **persistent calls** |
| Untethered | ✔ (pre-plan) Search and sync can each be switched off; neither needs the other to run |
| Not in scope | ○ Co-editing inside a file, end-to-end encryption, relay/NAT traversal, block-level delta transfer, symlinks and extended attributes (§13) |

---

## 1. Terms

| Term | Meaning |
|---|---|
| **Sync** | One shared folder tree with an id (16 random bytes), a name, a backup policy and a member list |
| **Host** | The MediaLake instance that owns a sync: its folder is the ground truth |
| **Member** | A paired device that has joined a sync, read-write or read-only |
| **Entry** | One file or directory in a sync, identified by its relative path |
| **Revision (`rev`)** | The host's sequence number at the moment an entry last changed. Every accepted change gets the next number |
| **Cursor** | The highest sequence number a member has fully applied |
| **Frontend** | How a sync appears on a member: Cloud Files, FUSE mount, or a plain mirrored folder |

One instance can host several syncs and be a member of several others, on several hosts, at the same time.

## 2. Architecture

```
            MEMBER (desktop app)                                   HOST (app or medialake-server serve)
┌──────────────────────────────────────────┐          ┌──────────────────────────────────────────────┐
│ UI (WinUI / SwiftUI): Sync settings page │          │  bierpc listener (TLS, pinned)               │
│  status item, conflict prompts           │          │  ├─ embedding service (existing, optional)   │
├──────────────────────────────────────────┤          │  └─ sync service                             │
│ medialake-core                           │          │      ├─ auth: client → grants per sync       │
│  ├─ search (existing, optional)          │          │      ├─ commit: check revs, apply, number    │
│  └─ SyncManager ─ one engine per sync    │  control │      ├─ changes since cursor                 │
│      ├─ state.db (entries, cursor,       │ ───────► │      ├─ content streams (persistent calls)   │
│      │   journal of local changes)       │  events  │      └─ notifications (persistent call)      │
│      ├─ conflict resolver                │ ◄─────── │                                              │
│      ├─ transfer queue (up / down)       │  content │  per sync:                                   │
│      └─ Frontend                         │ ◄──────► │   <root>/…            plain files (watched)  │
│          ├─ CfApi   (Windows)            │          │   <root>/.medialake-sync/                    │
│          ├─ Mount   (macOS)              │          │      state.db   entries, versions, snapshots │
│          └─ Mirror  (CLI, tests, Linux)  │          │      blobs/     content-addressed history    │
└──────────────────────────────────────────┘          │      tmp/       staging for uploads          │
                                                      └──────────────────────────────────────────────┘
```

Key points:

- **The host is the only place where order is decided.** Members never talk to each other. A change exists
  for everyone once the host has given it a sequence number.
- **The engine is platform-free.** Scanning, diffing, conflict handling, transfers and the state database
  are the same for all three frontends. A frontend only answers "show this entry", "give me the bytes of
  that entry" and "the user changed this".
- **The plain mirror frontend is a first-class citizen**, not a test stub: it is what the CLI and a Linux
  member use, and it is how the engine is proven before any platform API is touched.
- **Search does not change.** A sync folder can be added to the search index like any other folder (§10).

### 2.1 Crates

```
medialake-proto   + sync.rs: protocol types, path rules
medialake-sync    + new: engine, host, member, backups, frontends, pairing state
medialake-server    serve also hosts syncs; new `sync …` subcommands; can run without a model
medialake-core      SyncManager; search becomes optional
medialake-ffi       new sn_call methods and events
```

Dependencies: `proto ← sync ← {server, core} ← ffi`. `medialake-sync` never depends on `medialake-llama`
or on the index, so a sync-only host builds and runs without llama.cpp being loaded.

## 3. State and change tracking

The pre-plan describes "the member reports its file system state, the host answers with a diff". That is
kept as the **fallback** (§3.4); day to day the member sends a single number instead, because a full
listing of a 200 000-file sync on every wake-up is wasteful and a listing alone cannot tell "I deleted
this" from "I never had this".

### 3.1 Host state (`<root>/.medialake-sync/state.db`, SQLite, WAL)

| Table | Content |
|---|---|
| `meta` | sync id, `epoch` (random, changes if the database is rebuilt), current `seq`, `floor_seq` (oldest change still listed), policy |
| `entries` | `path` (primary key), `path_key` (case-folded, unique), kind, size, mtime (ms), `hash`, exec flag, `rev`, `deleted` (tombstone), author |
| `versions` | `path`, `rev`, hash, size, mtime, author, timestamp. History for the file-based backup strategy and for restore |
| `snapshots`, `snapshot_entries` | Time-based backups: a snapshot is a list of `(path, hash, size, mtime)` at one `seq` |
| `blobs` | hash, size. Garbage-collected when no entry, version or snapshot refers to it |
| `uploads` | Partial uploads in `tmp/`, for resume |

The host keeps this state in step with the folder in two ways:

1. **Commits from members** go through the commit path (§4.3), which updates files and database together.
2. **Direct edits on the host** are picked up by a watcher on the root plus a reconciliation scan at start
   (the same pattern as the search indexer: compare `(size, mtime)`, hash only what differs). Each detected
   change becomes a normal commit by the author `host`. It can never be rejected: the host is the ground
   truth by definition.

Writes that MediaLake itself makes are not echoed: after applying a commit the database already describes
the file on disk, so the watcher's follow-up scan finds no difference.

Tombstones are kept for 90 days (`floor_seq` moves forward as they are purged), so a member that was
offline for a few weeks still learns about deletes.

### 3.2 Member state (`<data dir>/sync/<sync id>/state.db`)

| Table | Content |
|---|---|
| `meta` | host id, sync id, `epoch`, `cursor`, access level |
| `entries` | `path`, kind, the **base**: `rev`, hash, size, mtime as last received from the host; and the **local** side: state (`placeholder`, `hydrated`, `dirty`, `conflict`), local size / mtime / hash, pinned flag |
| `journal` | Local changes not yet accepted by the host (create, modify, delete, move), written before they are acknowledged to the frontend |

The base is what makes conflict detection exact: a file is "changed here" if its local hash differs from
the base hash, and "changed there" if the host's `rev` is newer than the base `rev`.

### 3.3 Fingerprints

- Content hash: **BLAKE3** over the whole file. Fast enough (several GB/s) that hashing is never the
  bottleneck next to disk and network.
- `(size, mtime)` decides whether a file needs re-hashing after a scan, exactly as reconciliation does for
  the index today. It is never used to decide that two files are equal across machines.

### 3.4 Full reconciliation (the pre-plan's flow)

Used on first join, when the host's `epoch` changed, when the member's cursor is older than `floor_seq`,
and on request ("Verify" in the UI). The member sends its complete listing `(path, kind, size, mtime, hash)`
in pages; the host answers with the difference in both directions. Entries that exist on the member but
not on the host and have no base are treated as new local files (uploaded), never deleted.

## 4. Protocol (`medialake-proto::sync`)

### 4.1 Framing

- One listener, one certificate, one port, as today. The existing `Action` enum gets one new variant
  `Sync { auth, req: SyncRequest }`; `Response` gets `Sync(SyncResponse)`. Embedding requests are untouched
  and `PROTO_VERSION` stays 1. Sync has its own `SYNC_PROTO_VERSION`, exchanged in `SyncRequest::Hello`.
  An old server answers an unknown variant with a decode error, which the member reports as "this host
  does not support sync".
- The server registers a **persistence handler** (`RpcServer::with_persistence`) for the three streaming
  operations. bierpc has had this since spike 0c; the embedding path never needed it.
- bierpc runs one call per connection, so a member keeps per host: **one control connection**, **one
  notification connection**, and a small **transfer pool** (2 up, 2 down by default). `ServerConfig::
  max_connections` must allow for this on hosts with many members.

### 4.2 Single calls

| Request | Answer | Notes |
|---|---|---|
| `Hello { version, device_name }` | `Host { host_id, name, version }` | |
| `ListSyncs` | `Syncs(Vec<SyncInfo>)` | Only syncs this client has a grant for: id, name, access, file count, size, backup policy |
| `Changes { sync, cursor, limit }` | `Changes { epoch, entries, next_cursor, more }` or `ResetRequired` | Entries (incl. tombstones) with `rev > cursor`, oldest first |
| `Reconcile { sync, page }` | `Diff { … }` | §3.4 |
| `Propose { sync, changes }` | `Verdict { accepted, conflicts, need }` | Dry run of a commit. `need` lists the hashes the host does not have yet |
| `Commit { sync, changes }` | `Committed { results }` | Atomic per entry; each result is the new `rev` or a conflict with the host's current entry |
| `Versions { sync, path }` | `Versions(Vec<VersionInfo>)` | For the restore UI |
| `Snapshots { sync }` | `Snapshots(Vec<SnapshotInfo>)` | |
| `Restore { sync, what }` | `Committed { … }` | Read-write members only. A restore is a normal new commit, so it reaches everyone |

A change is one of:

```
Put    { path, hash, size, mtime, exec, base_rev }   // create or modify
Delete { path, base_rev }
Mkdir  { path }
Move   { from, to, base_rev }                        // keeps history; content is not re-sent
```

`base_rev` is the member's base for that path (0 = "I believe it does not exist"). The host accepts a change
only if `base_rev` equals the entry's current `rev`. This is the whole concurrency model: optimistic,
per entry, decided by the host under one lock per sync.

### 4.3 Pushing (member → host)

This is the pre-plan's "return a diff, and if it is accepted, send the files":

1. `Propose` with everything in the journal. The host checks access, path rules and `base_rev`.
2. For each hash in `need`: `Upload` (persistent call). Content lands in `tmp/<hash>.part`, is hashed while
   it arrives, and is only kept if the hash matches. An interrupted upload resumes at the byte the host
   already has.
3. `Commit` with the same list. The host re-checks every `base_rev` (another member may have been faster),
   moves the staged files into place with a rename, sets their mtime, assigns sequence numbers, writes
   version rows, and notifies subscribers.
4. Rejected entries come back as conflicts and go to the resolver (§5).

A `Put` whose hash already exists on the host (a copy, a move the member did not recognise, a file two
members both added) needs no upload at all.

### 4.4 Pulling (host → member)

1. `Changes` since the cursor.
2. For each entry: if the local side is clean, the frontend updates the placeholder (on-demand frontends,
   no download) or queues a download (mirror frontend, pinned files). If the local side is dirty, it is a
   conflict (§5).
3. The cursor advances once all entries of a page are recorded.

### 4.5 Persistent calls

| Stream | Direction | Use |
|---|---|---|
| `Download { sync, path, rev, offset, len }` | host → member | Hydration and mirror downloads. Ranges allow partial reads (video scrubbing, Cloud Files range requests) and resume. Frames of ≤ 4 MiB, each optionally zstd-compressed |
| `Upload { sync, hash, size, offset }` | member → host | §4.3 step 2 |
| `Subscribe { syncs: [(id, cursor)] }` | host → member | The host writes `Changed { sync, seq }` whenever a sync advances, and a heartbeat every 30 s. The member answers each notification with a `Changes` call on the control connection |

`Download` is addressed by path and `rev`, not by hash, so a client can only fetch content of a sync it has
a grant for. If the file on the host no longer matches that `rev` (a direct edit the watcher has not
processed yet), the host answers `Stale`, rescans the path, and the member retries after the resulting
notification. Old revisions that are still in the blob store are served from there.

If the notification stream breaks, the member reconnects with backoff and polls `Changes` every 60 s in
the meantime. "Real time" therefore means: local debounce (default 2 s) + upload + one notification, a few
seconds end to end for small files.

### 4.6 Path rules (cross-platform)

Enforced by the host on every change and by members before applying one:

- Relative, `/`-separated, UTF-8, **NFC-normalised** (macOS APIs may hand out decomposed names). No empty
  components, no `.` or `..`.
- **Case-preserving, case-insensitively unique** per directory, on every platform. `Report.txt` and
  `report.txt` cannot coexist in a sync, because neither Windows nor a default macOS volume could hold both.
  The second one is rejected as a conflict and stored under a conflict name.
- Names a platform cannot represent (Windows: `< > : " | ? *`, control characters, trailing dot or space,
  `CON`, `NUL`, …) are valid in the sync if the host can hold them. A member that cannot shows them in the
  status list as "cannot be synced to this device" and skips them; it never renames them for everyone.
- Windows members use `\\?\` paths internally, so long paths work.
- Synced metadata: mtime (ms) and the Unix executable bit. Not synced: permissions and owners, extended
  attributes, alternate data streams, resource forks, symlinks (skipped and listed, never followed).
- Ignored everywhere: `.medialake-sync/`, `.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, `~$*`, `*.tmp`,
  `*.part`, `.~lock.*`. A sync can carry additional glob patterns, set on the host and sent to members.

## 5. Conflicts

A conflict exists when an entry changed on the member **and** on the host since the member's base, and the
two results differ. Equal hashes on both sides are not a conflict; the member just adopts the new `rev`.

The policy is set **per sync on each member** (default: the host's suggestion, initially *server wins*):

| Policy | What happens |
|---|---|
| **Server wins** | The host's version takes the original name. The local version is renamed to `name (conflict, <device>, 2026-10-11 14-03).ext` in the same folder and uploaded as a new file, so it is visible to everyone and included in backups |
| **Ask** | The entry goes into state `conflict`: the local content stays where it is, is not uploaded, and the host's version is not applied. The UI shows a notification and a list (Settings → Sync → Conflicts) with **Keep mine**, **Keep server's**, **Keep both**. Other files keep syncing meanwhile |

Details:

- **Keep mine** commits the local version with the host's current `rev` as base. The replaced server version
  stays available according to the backup policy. **Keep both** is exactly the *server wins* behaviour.
  **Keep server's** is the one deliberate way to drop a local edit.
- **Edit here, delete there:** the edit is the loser but is still kept: it becomes a conflict copy.
- **Delete here, edit there:** the host's version comes back. Nothing is lost, so there is no copy.
- **Delete a folder here, new files in it there:** the folder survives with the new files.
- **Both created the same path:** like edit/edit.
- Unresolved *ask* conflicts on a headless member (CLI) stay pending and are listed by
  `medialake-cli sync conflicts`; `… resolve <path> mine|server|both` settles them.
- Read-only members cannot produce conflicts: their frontend refuses writes (§8).

Safety nets around the resolver:

- **Vanished root guard:** if a member's sync folder or mount is missing or suddenly empty (unplugged
  drive, failed mount), the engine pauses that sync with an error instead of pushing mass deletes.
- **Mass delete guard:** a single push that would delete more than 500 entries or more than half of the
  sync is held back until the user confirms it once.

## 6. Backups (host)

All history lives in `<root>/.medialake-sync/blobs/`, addressed by hash (`blobs/ab/cdef…`). `--store <dir>`
(or the folder picker in the app) moves `.medialake-sync` elsewhere, for example onto another disk; if
that is another volume, moves into the store become copies.

| Strategy | Setting | Behaviour |
|---|---|---|
| **Time-based** | `every` (default 24 h), `keep` N (default 7) | A snapshot records `(path, hash, size, mtime)` for every entry at one sequence number. Taking one copies nothing. The oldest snapshot beyond N is dropped |
| **File-based** | `keep` N (default 10), `min_interval` (default 10 min), `deleted_days` (default 30) | The last N versions of every file are kept. A new version by the same device within `min_interval` replaces the previous one instead of adding to the count, so an editor that saves every few seconds does not burn the history. Deleted files keep their versions for `deleted_days` |
| **None** | | Only the current state exists. Deletes and overwrites are final |

How old content gets into the store:

- When a **member's commit** replaces or deletes a file, the host *moves* the old file into `blobs/`
  (a rename, no copy) if the policy still needs it.
- A **direct edit on the host** overwrites the file in place, so the old content is gone before the watcher
  sees the change. To survive that, with a backup strategy enabled every file is preserved **when it
  becomes current**: as a copy-on-write clone where the file system supports it (APFS, ReFS / Dev Drive,
  btrfs, XFS), which costs no space, and as a real copy elsewhere (NTFS, ext4). On those volumes backups
  therefore cost up to one extra copy of the current data. The alternative is in §14.
- A blob is deleted when no current entry, version or snapshot refers to it (reference check in the
  database, sweep after each prune).

Restore:

- **File version**, **folder at a snapshot**, or **whole snapshot**, from Settings → Sync (member or host)
  and from the CLI. A restore is committed by the host as new revisions, so it propagates like any edit and
  is itself undoable through the same history.
- "Restore as copy" writes `name (restored <date>).ext` instead of replacing the current file.

Deduplication is per whole file in this version. Chunk-level deduplication would shrink the history of
large files that change a little (mailboxes, VM images) and can be added inside the blob store later
without touching the protocol.

## 7. Hosting, pairing and access

### 7.1 Shared host state

`medialake-server/src/state.rs` (certificate, pending pairings, paired clients) moves into
`medialake-sync::host::state`, so the app and the server use the same code and file formats. The server's
state directory stays `<data dir>/server/`; an app that hosts uses `<data dir>/host/`.

Client records gain grants:

```json
{ "name": "Jasper's MacBook", "secret_sha256": "…", "created": 1760000000,
  "embed": true, "syncs": { "9f2c…": "rw", "41aa…": "ro" } }
```

Existing records without the new fields mean "embedding only", which is exactly what they can do today.

### 7.2 Pairing

The pairing code keeps its format (`SN1-…`: address, one-time token, secret, certificate fingerprint).
What the code is good for is stored on the host next to the pending token, not in the code:

```
medialake-server pair --sync Photos:rw --sync Archive:ro        # sync only
medialake-server pair --embed --sync Photos:rw                  # embedding and sync
medialake-server pair                                           # as today: embedding only
```

In the app: Settings → Sync → Hosting → **Invite a device** (tick syncs, choose read-write or read-only),
which shows the code.

A device that is already paired gets further access without a new code: `medialake-server sync grant
<client> <sync> rw|ro` (or the member list in the app). The new sync then simply appears in the member's
`ListSyncs`. `sync revoke` and `clients revoke` take effect on the next request; the host also closes the
client's notification stream.

### 7.3 Headless host (`medialake-server`)

```
medialake-server serve [--no-embed] [--bind …] [--public-addr …]
medialake-server sync add <name> <folder> [--backup time:24h:7 | file:10 | none] [--store <dir>]
medialake-server sync list | remove <name> | set <name> --backup …
medialake-server sync grant|revoke <client> <name> [rw|ro]
medialake-server sync snapshots <name> | snapshot <name>
medialake-server sync versions <name> <path>
medialake-server sync restore <name> <path>[@rev] | --snapshot <id> [--as-copy]
```

- `--no-embed` (or no model present) starts without llama.cpp: a pure sync host. Embedding requests are
  answered with `Rejected`.
- Configuration commands edit `syncs.json` / `clients.json`, which a running `serve` re-reads when the
  file changes, the way `clients.json` works today.
- Commands that change data (`snapshot`, `restore`) must not race the running server. They connect to it
  on localhost with an admin token from the state directory (`admin.token`, readable by the user only);
  if no server is running they open the database directly.

### 7.4 Host in the app

The same `SyncHost` runs inside `medialake-core` when at least one hosted sync is configured, listening on
`0.0.0.0:47900` by default (configurable). It runs as long as the app runs, which is "always" for a tray /
menu-bar app, but not when the machine sleeps; the hosting page says so.

### 7.5 Reachability

The member must be able to open a TCP connection to the host: same network, VPN (Tailscale, WireGuard), or
a forwarded port. There is no relay. `--public-addr` decides what goes into pairing codes, as today.

## 8. Member frontends

```rust
trait Frontend {
    // engine → frontend
    fn apply(&self, changes: &[RemoteChange]) -> Result<Vec<Applied>>; // create/update/remove placeholders, renames
    fn set_status(&self, path: &Path, status: EntryStatus);            // in sync, syncing, error, conflict
    // frontend → engine (through a handle given at start)
    //   read(path, range) -> bytes        hydrate on demand
    //   local_change(change)              create / modify / delete / move made by the user
    //   pin(path) / unpin(path)           "always keep on this device" / "free up space"
}
```

### 8.1 Mirror (all platforms; CLI, tests, Linux, fallback)

A normal folder with full copies. Local changes come from a `notify` watcher with debounce plus a start-up
scan. The event normalisation that `medialake-core/src/watcher.rs` does today (rename pairing, the FSEvents
special case) moves into `medialake-sync::watch` and is used by both the indexer's watcher and this
frontend, so the two cannot drift.

A member can choose the mirror on any platform ("Keep a full copy in a normal folder"). It is also the
fallback when the platform frontend is unavailable (§8.3).

### 8.2 Windows: Cloud Files

- **Bindings:** the `windows` crate (already a dependency) covers the API: `Win32_Storage_CloudFilters` for
  `Cf*` and `Storage_Provider` for sync-root registration. Hand-written bindings should not be needed; the
  spike (W0) confirms that and decides between raw calls and the `cloud-filter` wrapper crate.
- **Sync root:** one per joined sync, by default `%USERPROFILE%\MediaLake\<name>`, registered with
  `StorageProviderSyncRootManager` (expected to work for an unpackaged app, W0 confirms; gives the Explorer
  navigation-pane entry and the status column). Hydration policy *full*, population policy *always full*:
  all placeholders are created from the host's listing, so browsing needs no network.
- **Callbacks** (`CfConnectSyncRoot`), running in the app process inside the Rust core:
  `FETCH_DATA` → `Download` ranges → `CfExecute(TRANSFER_DATA)`; `CANCEL_FETCH_DATA`; `NOTIFY_DELETE` and
  `NOTIFY_RENAME` to journal local changes early; dehydration notifications.
- **Local changes:** the placeholders are real NTFS files, so the normal watcher sees edits. After a
  successful commit the file is marked in sync (`CfSetInSyncState`).
- **Remote changes:** `CfUpdatePlaceholder` for clean files (dehydrating unless pinned), a conflict for
  files that are not in sync.
- **Pinning:** Explorer's "Always keep on this device" / "Free up space" set the pin attributes; the
  frontend hydrates or dehydrates accordingly.
- **Read-only members:** files carry the read-only attribute; changes that happen anyway are reverted from
  the host and the modified file is kept as a local conflict copy.
- **Out of scope for v1:** custom Explorer context menu, thumbnails for dehydrated files, custom status
  icons. Each needs a COM server registered by the installer.
- **Requirements:** Windows 10 1809+, NTFS. No installer changes beyond unregistering sync roots on
  uninstall (hydrated files stay, placeholders disappear).

### 8.3 macOS: mount

File Provider is ruled out (needs a Developer account). The mount is a user-space file system; which
mechanism carries it is the spike **A0**, with three candidates. The install and licence columns are what I
believe to be true today; confirming them is the first item of the spike.

| | Install | Notes |
|---|---|---|
| **macFUSE** | Kernel extension on macOS 14 (Recovery-mode security change on Apple Silicon); kext-free FSKit backend on macOS 15.4+ (mounts under `/Volumes`) | Most complete FUSE semantics. Redistribution terms must be read before bundling |
| **FUSE-T** | A package, no kext, no reboot | FUSE API on top of a local NFS server. NFS client caching and attribute behaviour are the risk |
| **Built-in NFS loopback** | **Nothing**: a small NFSv3 server inside the app (`nfsserve` crate), mounted with `mount_nfs` on localhost | The same mechanism FUSE-T uses, without a third-party install or licence. Most work on our side; needs checking that an unprivileged mount works |

The spike builds the same toy file system (in-memory tree, slow reads, writes) on each and scores:

1. Install friction on a clean macOS 14 and 15 machine; what the pkg would have to bundle; licence.
2. Finder behaviour: copy in and out, rename, Quick Look, "Save" from TextEdit, Pages, Office, Preview
   (atomic-save patterns), `._*` and `.DS_Store` noise.
3. **Unwanted hydration:** does Finder / Quick Look / Spotlight read file contents just by browsing? Can
   the file system tell who is asking (macFUSE passes the caller's pid; NFS does not)?
4. Throughput for large sequential reads and writes; behaviour when a read blocks for seconds.
5. Recovery: the app crashes or is force-quit with the mount active.
6. Rust support: `fuser` against each libfuse, or `nfsserve`.

The plan assumes only what all three can do. Whatever wins, the frontend looks like this:

- **Mount point:** `~/MediaLake/<name>` (a symlink into `/Volumes` if the backend dictates that).
  Mounted at app start, unmounted on quit; a stale mount from a crash is force-unmounted at start.
- **Metadata** (`lookup`, `getattr`, `readdir`, `statfs`) is answered from `state.db`, never from the
  network, so browsing is instant and works offline.
- **Cache:** `~/Library/Caches/MediaLake/sync/<sync id>/`, one file per hydrated entry. `read` on a
  dehydrated entry downloads the requested range first (with read-ahead) and blocks until it is there.
  A size limit (default 10 GB, per sync) evicts least-recently-used clean files. Dirty and pinned files
  are never evicted.
- **Writes** go to the cache file; the entry becomes `dirty` and is journalled. Upload starts when the file
  is closed and has been quiet for the debounce time. `create`, `unlink`, `rename`, `mkdir`, `rmdir` are
  journalled and acknowledged at once.
- **Offline:** cached files open and save normally; changes queue in the journal. Dehydrated files fail
  with an I/O error.
- **Noise control:** the volume is marked as not to be indexed by Spotlight; AppleDouble files are refused
  (`noappledouble` or an explicit error); extended attributes return "not supported".
- **Pinning and status** have no Finder integration (a Finder Sync extension has the same signing problem
  as File Provider). They live in the app: a per-sync browser in Settings → Sync with state per folder,
  "Keep on this device" and "Free up space".
- **Read-only members:** the volume is mounted read-only.
- **If the backend is missing or the mount fails:** the sync is offered as a mirror folder instead (§8.1),
  with a clear message. A member is never left without access because of a kernel-extension dialog.

Installer: if the chosen backend needs a component, `installer/macos` installs it as a second package in
the same `productbuild` distribution, and `scripts/macos/build-release.sh` fetches the pinned version like
`setup-vendor.sh` does for llama.cpp. The built-in NFS option needs neither.

## 9. Client configuration

`config.toml` stays backwards compatible: existing keys keep their place, everything new has a default.

```toml
search_enabled = true              # new; false = no hotkey, no search window, no model, no local server

[sync]
enabled = true
device_name = "Jasper's MacBook"   # shown in conflict names and on the host
debounce_ms = 2000
upload_limit_kbps = 0              # 0 = unlimited
download_limit_kbps = 0

[[sync.hosts]]                     # hosts this device is paired with
id = "b41f…"
name = "NAS"
address = "nas.local:47900"
fingerprint = "…"
client_id = "…"
secret_protected = "…"             # DPAPI / keychain reference, as for the embedding server

[[sync.folders]]                   # syncs this device is a member of
sync_id = "9f2c…"
host_id = "b41f…"
name = "Photos"
path = "/Users/jasper/MediaLake/Photos"
frontend = "auto"                  # auto = Cloud Files / mount; "mirror" = plain folder
conflict = "server_wins"           # or "ask"
cache_limit_gb = 10                # mount frontend only
index_for_search = true
paused = false

[[sync.hosted]]                    # syncs this device hosts
id = "41aa…"
name = "Projects"
root = "/Users/jasper/Projects"
backup = { strategy = "file", keep = 10 }   # or { strategy = "time", every_hours = 24, keep = 7 } or { strategy = "none" }

[sync.hosting]
bind = "0.0.0.0:47900"
public_addr = ""                   # empty = this computer's name
```

The embedding server's `remote` credentials stay where they are. A code that grants both embedding and
sync fills both `remote` and a `sync.hosts` entry.

**Untethered in practice:**

- `search_enabled = false`: `Core::start` does not start the supervisor, the indexer or the watcher, and
  does not ask for a model. The app is a tray / menu-bar item with the Sync settings.
- `sync.enabled = false` (the default for existing installs): no `SyncManager`, no listener, no mounts.
  Nothing about today's behaviour changes.
- The first-run screen asks what the user wants: **Search**, **Sync**, or both.

## 10. Search and sync together

- A member's sync folder has `index_for_search = true` by default: it is added to the index as a folder
  owned by the sync (shown in Settings → Index as such, not removable there).
- **The indexer never triggers a download.** For a sync folder it takes its file list from the sync state.
  Hydrated entries are indexed by content as usual; dehydrated ones by name and path only, and get their
  content embedding when they are hydrated for another reason. On Windows the indexer additionally skips
  content extraction for any file with the recall-on-data-access attribute, wherever it is (this also
  stops an index run from downloading OneDrive folders).
- A host's sync root is a plain folder and can be indexed like any other. `.medialake-sync/` is a dot
  directory and already ignored.
- Opening a search result in a sync folder hydrates it through the frontend like any other open.

## 11. FFI and UI

### 11.1 `sn_call` methods (same JSON style as today)

| Group | Methods |
|---|---|
| Status | `sync_status` (per sync: state, counts, transfer progress, last error), `sync_activity` (recent changes) |
| Hosts | `sync_pair { code }`, `sync_hosts`, `sync_forget_host { id }`, `sync_available { host }` |
| Membership | `sync_join { host, sync_id, path, frontend, conflict }`, `sync_leave { sync_id, keep_files }`, `sync_set { sync_id, … }`, `sync_pause` / `sync_resume`, `sync_verify { sync_id }` |
| Files | `sync_browse { sync_id, path }`, `sync_pin` / `sync_unpin { sync_id, path }` |
| Conflicts | `sync_conflicts`, `sync_resolve { sync_id, path, choice }` |
| History | `sync_versions { sync_id, path }`, `sync_snapshots { sync_id }`, `sync_restore { … }` |
| Hosting | `sync_host_add { name, root, backup }`, `sync_host_set`, `sync_host_remove`, `sync_invite { grants, embed }` → code, `sync_clients`, `sync_grant`, `sync_revoke` |

Events through the existing callback: `sync_state`, `sync_progress`, `sync_conflict`, `sync_error`.

### 11.2 UI (both hosts, mirrored as in plan2)

- **Settings gets a fourth page, "Sync"**, with three sections:
  1. *My folders*: joined syncs with state, path, pause, conflict policy, cache limit, "Browse" (pin /
     free up space; on macOS this is the only place for it), "Version history", "Leave".
     **Add** → paste a code or pick a host → pick a sync → pick a location.
  2. *Conflicts* (only shown when there are any).
  3. *Hosting*: hosted syncs with folder, backup strategy and its numbers, members and their access,
     **Share a folder**, **Invite a device**, snapshots and restore.
- **Tray / status item:** a line per sync ("Photos: up to date", "Projects: 12 of 340 files"), "Pause all
  syncing", and a badge on the icon while syncing or when there is a conflict or error.
- **Conflict prompt** (*ask* policy): a system notification; clicking it opens the Conflicts list.
- **First run:** the Search / Sync / both choice from §9.
- **Uninstall:** leaves syncs first (unregister Cloud Files roots, unmount), asks whether to keep
  downloaded files, and removes host secrets with the other credentials.

## 12. Phases / Milestones

Order as decided: the engine headless first, then macOS, then Windows.

| # | Phase | Done when |
|---|---|---|
| **S0** | Spike: bierpc streaming | A persistence handler next to the existing embedding service on one listener. A 5 GB upload and download with resume after a cut connection; a notification stream that survives an hour idle with heartbeats; measured throughput on localhost and LAN |
| **S1** | Protocol + host core | `medialake-proto::sync`, path rules with tests (NFC, case, reserved names). `medialake-sync` host: state database, start-up scan, watcher, commit path with `base_rev` checks, `Changes`. `medialake-server serve --no-embed` hosts a folder; `sync add/list/remove` |
| **S2** | Member engine + mirror frontend | `medialake-cli sync join/status/leave` keeps a mirror folder in step with a host in both directions, including moves, deletes, large files, restart in the middle of a transfer, and offline periods. `state.rs` moved to the shared crate; `pair --sync`, grants, read-only members |
| **S3** | Conflicts | Both policies, all cases of §5, the two guards. A scripted test with one host and three mirror members making random concurrent edits converges to identical trees with no lost version (every content hash ever written is either current, a conflict copy, or in history) |
| **S4** | Backups | Blob store, clone-or-copy preservation, both strategies, pruning and garbage collection, restore of a file, a folder and a snapshot through CLI and protocol |
| **S5** | Core + FFI | `SyncManager` in `medialake-core`, `search_enabled`, config schema, all `sn_call` methods and events, hosting inside the app. Sync-only and search-only starts both work |
| **A0** | Spike: macOS mount | The comparison of §8.3 is written down and one backend is chosen |
| **A1** | macOS mount frontend | A joined sync appears under `~/MediaLake`, browses offline, hydrates on open, uploads on save, evicts under the cache limit, survives a force-quit. Finder, TextEdit, Preview and one Office app pass the save-pattern checks |
| **A2** | macOS UI + installer | Sync page, status item, conflict notifications, first-run choice, pkg including the backend if one is needed, uninstall |
| **W0** | Spike: Cloud Files | An unpackaged test program registers a sync root from Rust, creates placeholders, serves a `FETCH_DATA` from a slow source, and reacts to pin / unpin |
| **W1** | Windows Cloud Files frontend | The same checks as A1, in Explorer, with the status column and pinning |
| **W2** | Windows UI + installer | Sync page in WinUI, tray, toasts, first-run choice, uninstall unregisters sync roots |
| **X1** | Search integration | §10 on both platforms: no hydration by indexing; name-only entries are upgraded when hydrated |
| **X2** | Polish + docs | Bandwidth limits, "Verify", error states (host unreachable, disk full, revoked access), README and `abstract.md` updated, a protocol note in `docs/` |

S0–S5 need no particular OS and can be developed and tested on the Mac. The convergence test from S3 runs
again in every later phase with the mirror frontend. A0 can run in parallel with S1–S5; it depends on
nothing in the engine.

## 13. Not in scope

- **Co-editing.** "Collaboration" here means files reach everyone within seconds and concurrent edits are
  never lost. Two people typing in the same document at once still produce a conflict copy.
- **End-to-end encryption.** The host stores plain files (that is the point of the plain-folder decision).
  Transport is TLS with a pinned certificate.
- **Relay / NAT traversal, discovery.** Hosts are reached by address.
- **Block-level delta transfer.** A changed file is sent whole (resumable, optionally compressed).
  The protocol leaves room: `Put` can later carry a chunk list.
- **Symlinks, extended attributes, alternate data streams, permissions** beyond the executable bit.
- **Selective sync by rule** ("never show this folder on this device"). Pinning covers the disk-space side.
- **Mobile clients, a web view of a sync, sharing links.**
- **Linux desktop frontend.** A Linux machine can be a headless host and a mirror member.

## 14. Open questions / risks

Decisions I took without asking; say so if you want any of them the other way:

1. **Hub and spoke with a sequence number, instead of exchanging full fingerprints every time.** The full
   exchange from the pre-plan remains as the fallback (§3.4). Without a per-member base, conflicts cannot
   be detected reliably, only guessed from timestamps.
2. **Backup cost on NTFS / ext4 hosts** (§6): preserving every current file doubles the space for current
   data on file systems without clones. The alternative is *lazy* preservation: only changes that arrive
   through MediaLake are versioned, and a file overwritten directly on the host loses its previous version
   (snapshots then describe content that may be gone). I chose correctness; a per-sync switch is cheap to
   add if disk space matters more.
3. **Conflict policy is per member, per sync**, not dictated by the host.
4. **A conflict copy is uploaded and shared**, as Dropbox does, rather than staying local. It is then in
   the backups, and others can see that a conflict happened.
5. **One strategy per sync** for backups (time **or** file **or** none), as the pre-plan lists them.
   Allowing both at once is a small change in the store.
6. **Default locations:** `~/MediaLake/<name>` on both platforms, port 47900 shared with embedding.

Risks:

1. **macOS mount.** The largest unknown; hence a spike before any frontend code and a mirror fallback that
   always works. Finder and Quick Look hydrating files while the user only browses could make on-demand
   pointless on a backend that cannot identify the caller; if so, the realistic outcomes are a small
   read-size heuristic, or mirror-by-default on macOS with the mount as an option.
2. **macFUSE licence and kext.** Bundling may not be allowed without an agreement, and the kext route asks
   users to lower a boot security setting. Both count heavily against it in A0.
3. **Cloud Files documentation and behaviour.** Thin, as the pre-plan says. Known traps: fetch callbacks
   time out (about a minute) unless progress is reported, antivirus and backup tools can hydrate
   everything, placeholders exist only on NTFS. W0 exists to find the rest.
4. **Editors' save patterns.** Many applications save by writing a temp file and renaming it over the
   original, or delete and recreate. Without care each save looks like delete + create and loses version
   history. The debounce plus move detection by hash handles it; A1 and W1 test it per application.
5. **Host asleep or offline.** Members keep working on cached files and queue changes, but two members
   cannot exchange changes while the host is down. A laptop is therefore a poor host; the hosting page
   says so, and the headless server is the recommended one.
6. **bierpc under long-lived streams.** Idle and read timeouts were tuned for request / response. S0 checks
   heartbeats, half-open connections and `max_connections` with many members.
7. **Clock skew.** Nothing depends on comparing clocks between machines (order comes from the host's
   sequence, equality from hashes). mtimes are carried along as data only.
8. **Database loss.** If a host's `state.db` is lost, it is rebuilt from the folder with a new `epoch`;
   members then reconcile fully (§3.4). History in `blobs/` without a database is orphaned, so the
   database is copied alongside each snapshot.
9. **Ad-hoc signing on macOS** (plan2 §9.5) now also affects host secrets in the keychain and, depending on
   the backend, a system-extension approval after each update.
