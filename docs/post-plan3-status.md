# File sync: implementation status (backend)

Implements the Rust side of `post-plan3.md`: phases **S0–S5** and the backend half of **X2**. No UI, no
Cloud Files, no macOS mount (A*, W*, X1's platform part are untouched). Everything below runs and is tested
on the Mac, with no model loaded. Wire format: `sync-protocol.md`.

| Phase | State | Where |
|---|---|---|
| S0 spike | Done: one-way throughput **~800 MiB/s** (1 GiB, loopback TLS, release); a **5 GiB upload cut at 40 %** resumes at the byte the host held (3 GiB re-sent, 10 s); the 60-minute idle notification-stream test (`SYNC_IDLE_MINUTES=60`) was started but its result is not recorded here; a 2-minute run is the default. Not measured: a real LAN | `tests/network.rs` (`--ignored`: `throughput_localhost`, `cut_connection_resumes_large_upload`, `idle_notification_stream_survives`) |
| S1 protocol + host | Done | `medialake-proto/src/sync.rs`, `medialake-sync/src/host/` |
| S2 member + mirror | Done | `medialake-sync/src/member/`, bin `medialake-sync` |
| S3 conflicts | Done, with the randomized convergence test | `tests/convergence.rs` |
| S4 backups | Done | `host/hosted.rs` (blob store, both strategies, prune, gc, restore) |
| S5 core + FFI | Done | `medialake-core/src/sync.rs`, `config.rs`, `core.rs`, `medialake-ffi/src/dispatch.rs` |
| X2 backend | Done: bandwidth limits, Verify, error states, docs | see below |

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
9. **`frontend = "auto"` resolves to the mirror** until the platform frontends exist. `sync_pin` /
   `sync_unpin` return an error for mirrors.
10. **Search integration (X1)**: joined folders with `index_for_search` are added as indexed directories.
    The "indexer never triggers a download" rule has nothing to guard with a mirror (every file is local);
    it belongs with the Cloud Files / mount frontends.
11. The CLI keeps host secrets in `hosts.json` (mode 0600); the app uses the Keychain / DPAPI like the
    embedding credentials.

## Still open

- **Platform frontends and UI** (A0–A2, W0–W2): the point of the mount / Cloud Files work. The engine and
  `Frontend` trait are ready for them.
- **Windows**: the mirror uses plain `std::fs`; `\\?\` long paths are not added, and nothing was run there.
  Linux should work as is.
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
  search toggling at runtime, sync enabling at runtime, error-state mapping, config compatibility.

Only macOS (arm64) was exercised.
