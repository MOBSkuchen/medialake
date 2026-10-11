# Sync protocol (v1)

Types: `crates/medialake-proto/src/sync.rs`. Everything rides on the existing bierpc listener: one port,
one self-signed certificate pinned by the fingerprint in the pairing code, `auth = client_id:secret_hex` in
every request. `PROTO_VERSION` (embedding) is unchanged; sync has `SYNC_PROTO_VERSION = 1`, exchanged in
`SyncRequest::Hello`. An old server answers `Action::Sync` with a decode error, which a member reports as
"this host does not support sync".

## Single calls

`Action::Sync { auth, device, req }` → `Response::Sync(SyncResponse)`. `device` is the member's display name;
it becomes the `author` of its changes and appears in conflict names.

| Request | Answer | Notes |
|---|---|---|
| `Hello { version }` | `Host { host_id, name, version }` | |
| `ListSyncs` | `Syncs` | only syncs this client has a grant for |
| `Changes { sync, epoch, cursor, limit }` | `Changes { epoch, seq, entries, next_cursor, more }` / `ResetRequired { epoch }` | entries with `rev > cursor`, oldest first. `cursor = 0` = the whole live tree (no tombstones). A non-zero cursor below the host's `floor`, or an `epoch` that is not the host's, gives `ResetRequired` |
| `Propose { sync, changes }` | `Verdict { results, need }` | dry run; `need` = content hashes the host lacks |
| `Commit { sync, changes }` | `Committed { results }` | atomic per entry, one transaction per request |
| `Versions { sync, path }` | `Versions` | |
| `Snapshots { sync }` | `Snapshots` | |
| `Restore { sync, what, as_copy }` | `Committed` | read-write only; a normal commit by the requester |

`Change` = `Put { path, hash, size, mtime, exec, base_rev }` | `Delete { path, base_rev }` | `Mkdir { path }` |
`Move { from, to, base_rev }`. The host accepts a change iff `base_rev` equals the entry's current `rev`
(for a missing entry or a tombstone: `0` or the tombstone's `rev`). Results: `Ok { rev }`,
`Conflict { reason, current }` (`BaseMismatch`, `CaseCollision`, `NotEmpty`, `ParentIsFile`,
`TargetExists`), `Invalid`, `MissingContent`, `Denied`.

Entries are identified by NFC-normalised, `/`-separated relative paths; two live entries may not share a
case-folded key. Ignored names (`.medialake-sync`, `.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, `~$*`,
`*.tmp`, `*.part`, `.~lock.*` + per-sync globs) are rejected as `Invalid`.

## Persistent calls

Used through `RpcClient::call_persistent`; the action is a `StreamAction`, the answer a `StreamResponse`
that arrives after the raw part below. Content is raw frames: `u32` big-endian length, then bytes; an
empty frame ends a stream. The top bit of the length marks a zstd frame (inflated size ≤ 4 MiB); writers
compress while it pays off and stop after three frames that do not shrink by 10 %.

| Call | Raw part |
|---|---|
| `Download { auth, sync, path, rev, offset, len }` | host: `u8 status` (0 ok, 1 stale, 2 not found); if ok `u64 total`, then frames from `offset` (`len` 0 = to the end) |
| `Upload { auth, sync, hash, size }` | host: `u64 have` (bytes it already holds); member: frames from `have`, then the empty frame. The host verifies the BLAKE3 hash and keeps the content only if it matches |
| `Subscribe { auth, syncs }` | host: `Notice { sync, seq }` (bierpc-encoded) whenever a sync advances, and `Notice { sync: "", seq: 0 }` every 30 s. Closed with `Denied` when access is revoked |

`Download` is addressed by `(path, rev)`: a client can only fetch content of syncs it has a grant for. If the
file no longer matches that `rev` the host answers `Stale` and rescans the path; an older revision still in
the blob store is served from there.

A member that shows files on demand (the macOS mount) uses the same call for ranges: `offset` and `len` of
the blocks a read touches (1 MiB blocks, up to 32 per request), each on a pooled connection like any other
download and subject to the same bandwidth limit. Nothing was added to the protocol for it.

## Member behaviour (what the host relies on)

1. **Pull** `Changes` from the cursor; per entry compare with the base and the local file (§5 of the plan);
   advance the cursor after a page is applied. Entries with `rev ≤` the base revision are skipped (the echo
   of the member's own commit).
2. **Push**: diff the folder against the base → `Propose` → `Upload` each `need` → `Commit`. Conflict
   results carry the host's current entry, which is fed back through the pull logic.
3. After `ResetRequired` (or a first join) revisions are forgotten and the full tree is walked; paths the
   host no longer lists were deleted there. Base *hashes* are kept, so "changed here" vs "changed there" stays
   exact.
4. Transfers resume: the member's partial download is appended to at its length; the host reports `have`
   for uploads. A cut connection costs at most the unacknowledged tail.

## Limits and timing

- Single messages ≤ `MAX_MESSAGE_BYTES` (32 MiB); content frames ≤ 4 MiB; `Changes` pages ≤ 2000 entries.
- Tombstones are kept 90 days; older cursors reconcile in full.
- Connections per member: one control, one notification stream, and up to four pooled transfer connections
  (each is reused for the next transfer; an idled-out one is replaced transparently).
- A host that is asleep or unreachable just means "offline" to the member: it keeps working on its folder
  and retries with backoff (2 s … 60 s).
- Measured on loopback TLS (release): ~800 MiB/s per direction; a 5 GiB upload cut at 40 % resumed at the byte
  the host held (`cut_connection_resumes_large_upload`, `--ignored`).
