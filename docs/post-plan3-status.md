# File sync: implementation status (backend)

Implements the Rust side of `post-plan3.md`: phases **S1–S5**. No UI, no Cloud Files, no macOS mount
(A*, W*, X1 are untouched). Everything below runs and is tested on the Mac, with no model loaded.

| Phase | State | Where |
|---|---|---|
| S0 spike | Partly: persistent calls carry content, resume and notifications; measured **~800 MiB/s** one-way for 1 GiB over loopback TLS (release). Not done: 5 GB over a cut LAN connection, an hour-idle stream | `crates/medialake-sync/tests/network.rs` (`throughput_localhost`, `--ignored`) |
| S1 protocol + host | Done | `medialake-proto/src/sync.rs`, `medialake-sync/src/host/` |
| S2 member + mirror | Done | `medialake-sync/src/member/`, bin `medialake-sync` |
| S3 conflicts | Done, with the randomized convergence test | `tests/convergence.rs` |
| S4 backups | Done | `host/hosted.rs` (blob store, both strategies, prune, gc, restore) |
| S5 core + FFI | Done | `medialake-core/src/sync.rs`, `config.rs`, `core.rs`, `medialake-ffi/src/dispatch.rs` |

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

Host administration: `medialake-server sync list|set|remove|grant|revoke|snapshots|snapshot|versions|restore`.
In the app the same things are `sync_*` calls (see below); a host runs inside the app when
`[[sync.hosted]]` has entries.

## What was built

- **Protocol** (`medialake-proto::sync`): `Action::Sync { auth, device, req }` / `Response::Sync`, three
  persistent calls (`Download`, `Upload`, `Subscribe`), NFC / case-insensitive-key / ignore / Windows-name
  path rules, conflict-copy names. `PROTO_VERSION` stays 1; `SYNC_PROTO_VERSION` is separate.
- **Host** (`HostedSync`): plain folder + `<store>/state.db` (SQLite, WAL). Revisions are per-entry
  sequence numbers; commits are checked against `base_rev` under one transaction per sync. Direct edits
  are found by scan + watcher (`watch::PathWatcher`), renames by hash become moves. Host state
  (`state.rs`) moved into `medialake-sync::host::state` and gained per-client grants (old records mean
  "embedding only").
- **Server**: `serve` hosts syncs next to embedding; `--no-embed` (or missing model files with syncs
  configured) starts without llama.cpp; `pair --sync Name:rw|ro [--embed]`; `sync …` subcommands;
  `syncs.json` / `clients.json` are re-read when they change; revocation closes the notification stream
  at its next heartbeat.
- **Member engine**: pull (`Changes`) then push (`Propose` → `Upload` → `Commit`), local diff against the
  base, move detection, both conflict policies and all cases of §5, mass-delete guard, vanished-root guard,
  read-only members (edits reverted, edited file kept as a local copy), resume of interrupted transfers.
  `Frontend` / `Transport` traits; `Mirror` and `LocalTransport` / `RemoteTransport` implementations.
- **Backups**: blobs are content-addressed; a file is preserved when it becomes current (APFS clone,
  copy elsewhere). File strategy: last N, `min_interval` merge, `deleted_days`. Time strategy: snapshots
  that copy nothing. Restore of a file version, a folder or a whole snapshot, optionally as a copy, as a
  normal commit that reaches every member.
- **Core / FFI**: `search_enabled` (false = no server, indexer, watcher), `[sync]` config section
  (hosts, folders, hosted, hosting), `SyncManager` (one engine thread per sync, hosting inside the app),
  events `sync_state` / `sync_progress` / `sync_conflict` / `sync_error`, and the `sync_*` methods of
  §11.1 plus `sync_enable`, `sync_hosting`, `sync_host_snapshot`, `sync_confirm_deletes`.
  Existing `config.toml` files load unchanged; sync is off by default.

## Where it differs from the plan

1. **No `Reconcile` request.** A member's cursor 0 (first join, or after `ResetRequired`) asks for the whole
   live tree; paths it knew that the host no longer lists were deleted there. After an epoch change
   revisions are forgotten but base hashes kept, so "changed here" vs "changed there" is still exact
   (tested by deleting the host database).
2. **No member `journal` table.** The mirror derives local changes by diffing against the base, which is
   restart-safe. A journal comes with the mount frontends, where changes are acknowledged before they exist
   on disk.
3. **No admin token.** `snapshot` / `restore` open the sync's database directly (SQLite serialises them with
   the running server); the server polls the sequence number every 2 s and notifies subscribers.
4. **Snapshots belong to the time strategy.** The file strategy keeps versions; `sync snapshot` on it says so.
5. **Restoring a snapshot brings back missing and changed files; it does not delete files that are newer
   than the snapshot.** (Deleting on restore is easy to add; leaving files alone is the safe default.)
6. **Mass-delete guard** also needs at least 10 deletes for the "more than half" rule, so deleting one of two
   files in a tiny sync is not held.
7. **Conflict names use UTC** (`name (conflict, <device>, 2026-10-11 14-03).ext`).
8. **One connection per transfer** (no pool of 2+2), **no zstd**, bandwidth limits are in the config but not
   enforced. All three are inside the transport and do not touch the protocol.
9. **`frontend = "auto"` resolves to the mirror** until A1 / W1. `sync_pin` / `sync_unpin` return an error for
   mirrors (every file is already a full copy).
10. **Search integration (X1)**: joined folders with `index_for_search` are simply added as indexed
    directories. The "indexer never triggers a download" rule has nothing to do yet with a mirror.
    `search_enabled` changes take effect at the next start.
11. `watch::PathWatcher` is new and shared by the host and the mirror; **core's indexer watcher was not
    migrated to it yet** (it still has its own copy of the rename handling).
12. Hashing a large file that was edited directly on the host happens under that sync's lock, so commits to
    that sync wait for it (seconds for GB files). A per-file pre-hash outside the lock is a possible refinement.
13. The CLI keeps host secrets in `hosts.json` (mode 0600); the app uses the Keychain / DPAPI like the
    embedding credentials.

## Tests

`cargo test -p medialake-proto -p medialake-sync` (needs no model, no cmake):

- `tests/basic.rs` (29): host scan, moves, both policies and every case of §5, read-only, guards, rebuilt
  host database, purged tombstones, resume, backups, snapshots, restore, path rules.
- `tests/convergence.rs`: one host, three members, random concurrent edits (writes, deletes, moves,
  copies, direct host edits); all trees equal and every content that reached a sync point is current or in
  history. `CONV_SEEDS=150` ran green for both policies.
- `tests/network.rs`: TLS pinning, pairing grants, streams, notifications, revocation, resume over the wire.
- `medialake-core/tests/sync.rs`: two cores, one hosting from the app, one pairing with an invite code,
  joining and syncing both ways with search off.

Only macOS (arm64) was exercised. Linux should work as is; Windows needs a run (path handling in the
mirror uses plain `std::fs`, `\\?\` long paths are not added yet).
