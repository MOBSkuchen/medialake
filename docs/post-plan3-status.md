# File sync: implementation status

Implements `post-plan3.md`: the engine and host (**S0–S5**), the backend half of **X2**, and the macOS
side (**A0–A2**, with the macOS half of **X1**), and a **base version of the Windows side** (**W0–W2**,
the Windows half of **X1** and **X2**): Cloud Files frontend, WinUI Sync page, tray, uninstall. The Windows
side has never run: `w0-cloud-files.md` is its checklist.
Everything below runs and is tested on a Mac, with no model loaded. Wire format: `sync-protocol.md`; the
mount decision and measurements: `a0-macos-mount.md`.

| Phase | State | Where |
|---|---|---|
| S0 spike | Done: one-way throughput **~800 MiB/s** (1 GiB, loopback TLS, release); a **5 GiB upload cut at 40 %** resumes at the byte the host held (3 GiB re-sent, 10 s); the notification stream **survived 60 minutes idle** and delivered (`SYNC_IDLE_MINUTES=60`, one run, loopback); a 2-minute run is the default. Not measured: a real LAN | `tests/network.rs` (`--ignored`: `throughput_localhost`, `cut_connection_resumes_large_upload`, `idle_notification_stream_survives`) |
| S1 protocol + host | Done | `medialake-proto/src/sync.rs`, `medialake-sync/src/host/` |
| S2 member + mirror | Done | `medialake-sync/src/member/`, bin `medialake-sync` |
| S3 conflicts | Done, with the randomized convergence test | `tests/convergence.rs` |
| S4 backups | Done | `host/hosted.rs` (blob store, both strategies, prune, gc, restore) |
| S5 core + FFI | Done | `medialake-core/src/sync.rs`, `config.rs`, `core.rs`, `medialake-ffi/src/dispatch.rs` |
| X2 backend | Done: bandwidth limits, Verify, error states, docs | see below |
| A0 spike: macOS mount | Done: built-in NFSv3 loopback chosen; macFUSE / FUSE-T not installed here, so not measured | `a0-macos-mount.md`, `crates/medialake-mount/spike/` |
| A1 mount frontend | Done: browse offline, load on open (block-wise), upload on save, evict under the limit, pin, crash cleanup, fallback to a mirror. Finder / Office checks by hand are not done (see below) | `crates/medialake-mount/`, `medialake-core/src/sync.rs` |
| A2 macOS UI + installer | Done: Sync page, status item, notifications, two-page welcome window (search with optional shortcut, then sync), uninstall. Verified by building and by driving the real app binary; not clicked through by a person. A pkg was built, not notarised | `ui/MediaLake.Mac/Sources/MediaLake/Sync*.swift`, `installer/macos/` |
| W0 spike: Cloud Files | Base version: spike program written (`examples/spike.rs`), cfapi layer type-checks for `windows-msvc`; **not run on Windows** | `crates/medialake-cloudapi/`, `w0-cloud-files.md` |
| W1 Cloud Files frontend | Base version: placeholders from the host's listing, FETCH_DATA from the transport, pin / free up, in-sync marking, leave; engine contract tested over a stand-in platform. **Not run on Windows** | `crates/medialake-cloud/` |
| W2 WinUI Sync page, tray, toasts, uninstall | Base version, **never compiled** (no .NET SDK on the dev Mac) | `ui/MediaLake.App/SyncPage.xaml(.cs)`, `Native/Shell.cs`, `App.xaml.cs`, `installer/medialake.iss` |
| X1 search integration (Windows) | Recall-attribute check in the indexer for every folder (OneDrive too) and `available` on search hits (no thumbnail that downloads); unrun on Windows | `indexer.rs` (`os_would_download`), `search.rs` |
| X1 search integration (macOS) | Done in code: indexing never downloads, name-only entries are upgraded once fetched. Needs a model to exercise end to end, which was not run | `indexer.rs` (`ContentGate`), `watcher.rs`, `sync.rs` (`would_download`) |

## Try it

```
# host (headless, no model needed)
medialake-server sync add Photos ~/Pictures --backup file:10
medialake-server serve --no-embed
medialake-server pair --sync Photos:rw              # prints SN1-… (sync only; add --embed for both)

# member
medialake-sync pair SN1-…
medialake-sync available <host>
medialake-sync join <host> Photos ~/MediaLake/Photos [--conflict ask]
medialake-sync run                                   # or `once`, `status`, `conflicts`, `resolve …`
```

Host administration: `medialake-server sync list|set|remove|grant|revoke|snapshots|snapshot|versions|restore`
(`restore --snapshot <id> [path] [--prune]`). In the app the same things are `sync_*` calls (below); a host
runs inside the app when `[[sync.hosted]]` has entries.

## What was built

- **Protocol** (`medialake-proto::sync`): `Action::Sync { auth, device, req }` / `Response::Sync`, three
  persistent calls (`Download`, `Upload`, `Subscribe`), NFC / case-insensitive-key / ignore / Windows-name
  path rules, conflict-copy names, raw frames with adaptive zstd. `PROTO_VERSION` stays 1.
- **Host** (`HostedSync`): plain folder + `<store>/state.db` (SQLite, WAL). Per-entry revisions; commits are
  checked against `base_rev` in one transaction per sync. Direct edits are found by scan + watcher; renames
  by hash become moves. Hashing and blob preservation of directly edited files happen *outside* the sync's
  lock. Host state moved into `medialake-sync::host::state` with per-client grants (old records mean
  "embedding only").
- **Server**: `serve` hosts syncs next to embedding; `--no-embed` (or missing model files with syncs
  configured) starts without llama.cpp; `pair --sync Name:rw|ro [--embed]`; `sync …` subcommands;
  `syncs.json` / `clients.json` are re-read when they change; revocation closes the notification stream.
- **Member engine**: pull, then push (`Propose` → `Upload` → `Commit`); local diff against the base, move
  detection, both conflict policies and all cases of §5, mass-delete guard, vanished-root guard, read-only
  members, resumable transfers. `Frontend` / `Transport` traits; `Mirror`, `LocalTransport`,
  `RemoteTransport` (connection pool of 4, shared up/down rate limiters, adaptive zstd).
- **Backups**: content-addressed blobs; a file is preserved when it becomes current (APFS clone, copy
  elsewhere). File strategy: last N, `min_interval` merge, `deleted_days`. Time strategy: snapshots that
  copy nothing. Restore of a file version, a folder or a whole snapshot (optionally `prune`), optionally as
  a copy, as a normal commit that reaches every member.
- **Core / FFI**: `search_enabled` (switchable at runtime: server, watcher and indexer follow; the index
  stays on disk), `[sync]` config section, `SyncManager` (one engine thread per sync, hosting inside the
  app, bandwidth limits applied live), events `sync_state` / `sync_progress` / `sync_conflict` /
  `sync_error`, and the `sync_*` methods of §11.1 plus `sync_enable`, `sync_hosting`,
  `sync_host_snapshot`, `sync_confirm_deletes`. Sync states include `offline`, `denied` and `disk_full`.
  Existing `config.toml` files load unchanged; sync is off by default.
- **One filesystem-event normaliser**: `medialake-sync::watch::normalize` (rename pairing, the FSEvents
  special case) is used by the host, the mirror frontend and the search indexer's watcher.

## Where it differs from the plan

1. **No `Reconcile` request.** Cursor 0 (first join, or after `ResetRequired`) asks for the whole live tree;
   paths the member knew that the host no longer lists were deleted there. After an epoch change revisions
   are forgotten but base hashes kept, so "changed here" vs "changed there" stays exact (tested by deleting
   the host database).
2. **No member `journal` table.** The mirror derives local changes by diffing against the base, which is
   restart-safe. A journal comes with the mount frontends, where changes are acknowledged before they exist
   on disk.
3. **No admin token.** `snapshot` / `restore` open the sync's database directly (SQLite serialises them with
   the running server); the server polls the sequence number every 2 s and notifies subscribers.
4. **Snapshots belong to the time strategy.** The file strategy keeps versions; `sync snapshot` on it says so.
5. **Restoring a snapshot does not delete newer files unless `prune` is given** (the deleted files stay in
   the history).
6. **Mass-delete guard** also needs at least 10 deletes for the "more than half" rule.
7. **Conflict names use UTC** (`name (conflict, <device>, 2026-10-11 14-03).ext`).
8. **Bandwidth limits are in kilobits per second** and enforced client-side (one limiter per host for
   uploads, one for downloads). The host does not throttle.
9. **`frontend = "auto"`** means the mount on macOS and the mirror elsewhere, decided once per sync and
   then remembered (`<state dir>/frontend`): a sync never changes where its files are by itself. Syncs that
   predate mounts stay mirrors. A mount that cannot start (folder not empty, mount refused) runs as a mirror
   with a notice. `sync_pin` / `sync_unpin` error for mirrors.
10. **Search integration (X1)**: joined folders with `index_for_search` are indexed through the mount point.
    The watcher watches the cache tree behind it (FSEvents is silent inside NFS), and the indexer asks the sync
    manager before reading: a file that would first be downloaded is indexed by name only (`status =
    deferred`) and re-processed when it has been fetched.
11. **A cache is only trusted with the engine state it was filled under** (see `a0-macos-mount.md`); an
    orphaned cache is set aside, never read as local edits.
12. **No per-file badges in Finder** (a Finder Sync extension needs the same signing as File Provider):
    "keep on this Mac" / "free up space" live in the app's folder browser, as planned.
13. The CLI keeps host secrets in `hosts.json` (mode 0600); the app uses the Keychain / DPAPI like the
    embedding credentials.

## Still open

- **Windows**: everything W0–W2 has to be built and run on a Windows machine: the cfapi layer and the
  frontend have never executed, the WinUI files (welcome window, search hint, optional shortcut, Sync
  page, tray, uninstall step, installer without a shortcut page) have never been compiled. Start with
  `w0-cloud-files.md`. Known gaps: no navigation-pane entry / "Status" column (that needs
  `StorageProviderSyncRootManager`), no `NOTIFY_DELETE` / `NOTIFY_RENAME` callbacks, no browse dialog (pin
  and free up are done in Explorer), the mirror uses plain `std::fs` and adds no `\\?\` long paths.
  Linux should work as is for the mirror.
- **By-hand checks on a Mac** (A1's acceptance list): Finder copy in and out, Quick Look and column-view
  previews (does browsing read more than the first block?), TextEdit / Pages / Preview / an Office app saving
  on the volume, Spotlight staying out, macOS 14 and 15, Intel. The Cocoa save patterns that can be scripted
  (`saves.swift`: atomic write, `replaceItemAt`, xattrs, truncate, locks) pass on the mount.
- **Signing and notarisation** of the app and pkg need the developer identity; the build script supports
  them through environment variables and was run ad hoc only.
- **One mount per sync**, no shared `~/MediaLake` volume: a sync's location is chosen per sync.
- **Partial writes** to a file that is not on this Mac fetch the whole file first (the sync is whole-file).
- **A real LAN / Wi-Fi run** of the S0 numbers (only loopback was measured).
- **Block-level delta transfer, end-to-end encryption, relay** are out of scope (§13).

## Tests

`cargo test -p medialake-proto -p medialake-sync` (needs no model, no cmake) and
`cargo test -p medialake-core --test sync`:

- `tests/basic.rs` (30): host scan, moves, both policies and every case of §5, read-only, guards, rebuilt
  host database, purged tombstones, resume, backups, snapshots, restore (+ prune), path rules.
- `tests/convergence.rs`: one host, three members, random concurrent edits (writes, deletes, moves,
  copies, direct host edits); all trees equal and every content that reached a sync point is current or in
  history. `CONV_SEEDS=150` runs green for both policies.
- `tests/network.rs`: TLS pinning, pairing grants, streams, notifications, revocation, resume, pooled
  connections, bandwidth limit, compression; ignored soak tests for throughput, a cut 5 GiB transfer and
  an hour-long idle stream.
- `medialake-core/tests/sync.rs`: two cores (one hosting from the app, one joining with an invite code),
  search toggling at runtime, sync enabling at runtime, error-state mapping, config compatibility; on macOS a
  mounted sync (placeholders, load on open, pin, write-through, leave keeping the downloaded files) and the
  fallback to a mirror.
- `medialake-cloud/tests/frontend.rs` (any host): the frontend with the real engine and host over a
  stand-in for Cloud Files: placeholders without fetching, a lost state database not downloading anything,
  edits and new files pushed and converted, remote changes freeing clean placeholders, pins, renames and
  deletes, leaving (with and without keeping), refusing a non-empty folder, uninstall without a connection.
  `medialake-cloudapi`: identity blob, transfer planner, callback paths, recall attributes.
  Windows layer: `cargo check -p medialake-cloudapi --target x86_64-pc-windows-msvc --all-targets`.
- `medialake-mount/tests/mount.rs` (macOS, really mounts): browsing fetches nothing, a read fetches a block,
  writes / rename-over / delete / junk files, host changes replace cached content, eviction and pins,
  read-only, conflicts keep both, offline, parallel readers, `touch` does not download, a Cocoa process's
  save patterns, stale-mount cleanup and the process registry.

Only macOS (arm64) was exercised.
