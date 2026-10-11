# A0: which mechanism carries the macOS mount

Spike for `post-plan3.md` §8.3. Result: **the built-in NFS loopback** (a small NFSv3 server inside the
app, mounted with the system's `mount_nfs`). It needs nothing installed, no root, no kernel extension and
no third-party licence. This note records what was measured, what was not, and what follows for A1.

Reproduce with `crates/medialake-mount/spike/` (`mount.sh`, `score.sh`, `saves.swift`) and the toy server
`cargo run -p medialake-mount --features spike --bin mount-spike`.
Machine: Apple silicon, macOS 26.2, no macFUSE or FUSE-T installed.

## Candidates

| | Install | Measured here? | Verdict |
|---|---|---|---|
| **Built-in NFSv3 loopback** (`nfsserve` crate + `mount_nfs`) | none | **yes** | chosen |
| FUSE-T (FUSE API over a local NFS server) | a package | no | the same mechanism as the chosen one with a third-party install in between; nothing it adds is needed |
| macFUSE | kernel extension (macOS 14) / FSKit backend (15.4+) | no | needs a boot-security change or a recent macOS, and redistribution terms to read; the only thing it offers over NFS is the caller's pid (§ "Unwanted hydration" below says why that is not needed) |

macFUSE and FUSE-T were not installed or run: installing a kernel extension or a system package on the
development machine was not something to do from a spike. Their rows are the plan's own claims, not
measurements. Nothing in the decision depends on them being wrong.

## What was measured (NFS loopback)

| Question | Result |
|---|---|
| Unprivileged mount | Works. `mount_nfs -o vers=3,tcp,… localhost:/ ~/dir` as the logged-in user, on a directory the user owns; `mount` shows "mounted by <user>" |
| Browsing | `readdir` (READDIRPLUS) and `getattr` are answered from the server's own metadata; `ls -la`, `stat`, `mdls` caused **no** `READ` |
| Spotlight | `nobrowse` keeps the volume out of Finder's sidebar and out of Spotlight (`mdls` on a file reports it as unknown) |
| Copy in / out, rename, mkdir, rmdir, delete | Correct; `cp` out of a 6 MB file = 18 READs of 1 MiB with `rsize=1048576` |
| Atomic save (temp file + rename over the original) | Correct (`Data.write(.atomic)`) |
| `NSFileManager.replaceItemAt`, with the temporary item in the same or another directory | Correct (this is what `NSDocument` does for TextEdit, Pages, Preview) |
| Safe save (backup copy, write, delete backup) | Correct |
| Delete + recreate, directory move with contents | Correct |
| `ftruncate` + sparse `pwrite` | Correct |
| Extended attributes | `setxattr` succeeds; the client stores them in a `._name` AppleDouble file next to the file |
| `flock` | Fails with `ENOTSUP` under `nolocks`; **works with `locallocks`** (locks are local to this Mac, which is what a single-writer-per-file sync wants) |
| Throughput, release build | sequential read 760 MB/s, write 3 GB/s (client write-back), 500 small creates in about 1 s |
| Server killed under a live mount | with `soft,intr,timeo=20,retrans=3` the client gets `ETIMEDOUT` after about 8 s instead of hanging; with `deadtimeout=15` the system unmounts the dead mount by itself within about 30 s; `umount -f` cleans up immediately |
| A single read that blocks for 20 s / 60 s on a live connection | not cut off: the client keeps waiting (soft only matters when the connection dies) |

Not measured, because they need an interactive Finder session: Finder copy and Quick Look previews,
Pages / Office saves, the Finder "volume unresponsive" dialog. `qlmanage -t` hung in this headless
session. They are listed as the A1 manual checklist.

## Findings that shape A1

1. **No close, no caller.** NFSv3 is stateless: there is no `open`/`release`, and the server does not
   know which process reads. "Upload when the file is closed" becomes "upload when it has been quiet for
   the settle time" (the engine already works that way, 1 s). "Do not hydrate for a Quick Look peek" cannot
   be done by identifying the caller; it is done by never fetching more than was asked:
   the cache is **sparse and block-based**. A read fetches only the blocks it covers (plus read-ahead when
   the access is sequential); a file counts as hydrated when all its blocks are present. A thumbnail or
   `file(1)` reads the first blocks of a 1 GB movie, not the movie.
2. **`._*` and `.DS_Store` are real files** the client creates through NFS. They are kept in the local
   tree and never synced (the sync ignore list already names them); the sync-side removal of a folder
   deletes them with it.
3. **Locks**: mount with `locallocks`, not `nolocks`.
4. **Names**: mount with `nfc`, so that names arrive in NFC as the sync's path rules expect.
5. **Failure behaviour**: `soft,intr,retrans=3,timeo=20,deadtimeout=30`. A crash of the app leaves a dead
   mount that the system removes by itself; at start the app force-unmounts anything of its own that is
   still listed.
6. **Who may connect.** nfsserve does no authentication and listens on loopback, so any local user could
   mount it. A1 mounts an unguessable export name and puts 128 random bits in every file handle
   (`id_to_fh` / `fh_to_id` are overridable), so a handle cannot be forged without seeing the traffic.
7. **Mount options used**: `vers=3,tcp,locallocks,noresvport,port=P,mountport=P,rsize=1048576,
   wsize=1048576,nobrowse,nodev,nosuid,soft,intr,nfc,timeo=20,retrans=3,deadtimeout=30,actimeo=0,
   nonegnamecache` (plus `rdonly` for read-only members). `actimeo=0` / `nonegnamecache` are not optional:
   the sync engine changes the cache tree behind the server's back (a pulled file, an evicted block), and a
   client that caches attributes and directory listings does not see it. They cost one loopback RPC per
   `stat`.
8. **Read-only members** get `rdonly` on the mount and `NFS3ERR_ROFS` from the server.
9. **Setting times**: `setattr` must honour `mtime` (the toy did not; `NSFileManager.setAttributes`
   failed there). The engine carries mtimes as data.

## Not solved by this choice

- No Finder badges or context menu (same signing problem as File Provider); pin and free-up live in the
  app's sync browser, as planned.
- The volume is local to this Mac's NFS client; another user on the same Mac cannot use it (by design).
- A hung Finder window while the app is not running: the mount disappears with the app (we unmount on
  quit), so `~/MediaLake/<name>` is an empty folder until the app starts.

## As built (A1)

- **Cache tree.** `~/Library/Caches/MediaLake/sync/<sync id>/tree/` holds one real file per entry, at full
  size and sparse until its blocks arrive; the sync engine scans this tree like a mirror folder, and the NFS
  layer serves it. `index.db` next to it knows which 1 MiB blocks are present, what is dirty, what is pinned
  and the inode numbers.
- **Hydration.** A read fetches the blocks it touches (host `Download` with `offset`/`len`), read-ahead grows
  to 16 MiB once the access is sequential, the file is hash-checked when its last block lands. A write to a
  partial file first fetches the rest (the sync is whole-file). `touch` and `chmod +x` do not download
  anything (the engine learns the content hash from the index).
- **Eviction.** Least recently used clean, unpinned files, down to 90 % of the limit (default 10 GB per sync).
  An evicted file becomes a hole again with its size and mtime unchanged, so the engine still sees it clean.
- **Offline.** Cached files open and save; anything else answers `EIO`. Edits queue in the tree and go out
  when the host is reachable (tested with a transport that goes away).
- **Cache and engine state belong together.** `<state dir>/cache-id` and `<cache>/owner` must match and
  the state database must exist, otherwise the old tree is moved to `<cache>/set-aside-<time>/` and the
  sync starts from an empty cache (with a notice). Without this a leftover cache looks like a pile of local
  additions to a fresh engine and is pushed or conflicts.
- **Crash.** `mounts.json` records which process owns which mount; at start the app force-unmounts the
  mounts of dead processes. The watchdog remounts a mount the system dropped.
- **Search.** FSEvents does not report changes inside an NFS mount, so the watcher watches the cache tree and
  maps its paths to the mount point; the indexer asks the sync manager before reading a file and indexes it
  by name only when reading would download it.
- **Not measured here:** Finder's own copy and Quick Look previews (headless session), Pages / Office, a LAN
  host, an Intel Mac, macOS 14 / 15 (this machine runs 26.2).
