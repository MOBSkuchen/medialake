# W0–W2: Cloud Files on Windows — what runs, what it showed, what is left to check by hand

The Windows side was written on a Mac against the `windows` crate's bindings. It has since been built and
run on Windows 11 (build 26200, NTFS, an unpackaged process). This note says what that showed: the bugs
that only a real run could find, the answers to the questions the first version left open, and the checks
that still need a person in front of Explorer. Plan: `post-plan3.md` §8.2, §10, §11.

## What exists and how it is verified

| Piece | Where | Verified how |
|---|---|---|
| Typed layer over cfapi (register, connect, placeholders, TRANSFER_DATA, update, convert, pin, hydrate, dehydrate, revert) | `crates/medialake-cloudapi/src/win.rs` | **10 tests against the real platform** (`tests/win.rs`): each registers a sync root in a temporary folder and lets Windows fetch through it |
| Platform-neutral half: identity blob, 4 KiB transfer planner, callback-path mapping, recall-attribute test | `…/cloudapi/src/{types,plan}.rs` | unit tests (any host) |
| **W0 spike** | `crates/medialake-cloudapi/examples/spike.rs` | run, driven from a second process (reads, pin / free through `attrib`, copy in, rename, delete, disconnect, unregister) |
| **W1 frontend** | `crates/medialake-cloud/` | **16 tests on the real platform with the real engine and host** (`tests/windows.rs`), next to the 11 over the stand-in (`tests/frontend.rs`, any host) |
| Core wiring: frontend kind `cloud` (auto on Windows, mirror fallback with a notice), browse / pin / unpin / leave, `sync_unregister_roots` | `medialake-core/src/sync.rs`, `core.rs`, `medialake-ffi/src/dispatch.rs` | `medialake-core/tests/sync.rs`: two cores over TLS, the member's folder a Cloud Files root (load on open, pin all, upload, host change, pause, leave, uninstall, fallback to a mirror) |
| **W2**: WinUI Settings > Sync, welcome window, tray lines and balloons, uninstall step | `ui/MediaLake.App/` | builds without warnings; the real `MediaLake.exe` driven with `--selftest` against a scratch data folder (it hosts a folder, joins it as a Cloud Files root, reads and writes through Explorer's file system, renders the Sync page and its dialogs), then `--unregister-sync-roots` |
| **X1**: files with `RECALL_ON_DATA_ACCESS` / `RECALL_ON_OPEN` / `OFFLINE` are indexed by name only | `medialake-core/src/indexer.rs`, `search.rs` | the attributes are asserted on real placeholders, and reading them with `symlink_metadata` fetches nothing; an index run with a model over a root of placeholders was **not** done |

## What the first real run found

All of these type-checked and passed the stand-in tests. Each is fixed, and the tests on the real platform
cover it.

1. **No file could be opened.** `CfConnectSyncRoot` was called without `CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH`, so
   callbacks carried no `NormalizedPath` and every fetch was refused as "not below the sync root".
2. **Asking for a file's state downloaded the file.** `CfOpenFileWithOplock`, opened from the provider's own
   process, makes the platform fetch the whole file. The engine asks for the state of every file in every
   scan: the first scan would have downloaded the sync. All calls use ordinary handles without data access
   now (`open_query` / `open_change`). `CfUpdatePlaceholder` also refused the oplock handle unless it was
   exclusive ("invalid handle").
3. **Folder placeholders were refused** (`ERROR_CLOUD_FILE_INVALID_REQUEST`): the platform wants an identity
   blob for directories too.
4. **A read waited a full minute.** With the hydration modifier `STREAMING_ALLOWED`, a small read from another
   process produced a whole-file request whose data the platform threw away, then the real request 60 s later.
5. **A file that changed size came back cut off.** A placeholder edited here, pushed, freed and opened again
   returned its old length followed by zeros: marking it in sync has to tell the placeholder the new size.
6. **A rename could delete the file on the host.** Windows clears the "in sync" mark of a renamed
   placeholder. The engine then tried to hash it, which asks the host for a name it does not know yet; the
   read failed, the new name was skipped, and the old name was pushed as a delete. With nothing kept in
   backups that would have been the only copy. Fixed twice over: a placeholder whose content is not here
   answers with the hash it stands for (so the rename is pushed as a move), and the engine never pushes the
   delete of a file while a new file of the same size cannot be read (`medialake-sync`, all platforms).

Smaller things: the marker file is hidden and goes when the sync is left; folders that end up empty are
removed on leaving; `sync_set` accepts `cloud`; leaving a sync whose root is registered but not connected
unregisters it; the uninstaller's step (`sn_unregister_sync_roots`) reads the configuration and starts
nothing, where it used to start the whole core, model server and syncs included, of an app that is being
removed.

## Design in one page

* **Sync root** per sync, default `%USERPROFILE%\MediaLake\<name>`, registered with plain `CfRegisterSyncRoot`.
  A hidden marker file `.medialake-sync` (an ignored name) says the folder is ours; a non-empty folder
  without it is refused and the sync runs as a mirror with a notice.
* **Population** is *always full*: every placeholder is created from the host's listing, so Explorer never
  asks the provider to enumerate and browsing needs no network.
* **Hydration** is *full*, as planned: a file is a placeholder or it is all here. Auto-dehydration is
  allowed (Storage Sense may free space).
* **Callbacks**: `FETCH_DATA` (served from the engine's transport with the revision from the placeholder's
  identity, in 4 KiB-aligned pieces of at most 4 MiB, with progress; the log names the program that reads)
  and `CANCEL_FETCH_DATA`. A failed fetch is answered at once with an error status.
* **No `NOTIFY_DELETE` / `NOTIFY_RENAME`**: the file watcher sees deletes and renames as it does for a mirror.
* **Pinning**: Explorer's "Always keep on this device" / "Free up space" only set attributes. The frontend
  watches attribute changes (and walks the tree once at start) and hydrates / dehydrates accordingly. The
  app's folder browser does the same per file and folder.
* **Read-only members**: placeholders carry the read-only attribute; edits that happen anyway are reverted
  and kept as a copy by the engine's existing rule.

## The questions of the first version, answered

1. **An unpackaged process and `CfRegisterSyncRoot` alone:** registers and connects. Placeholders show the
   reparse attribute to our own process (no compatibility-mode call needed). There is no navigation-pane
   entry and no "Status" column; those need `StorageProviderSyncRootManager`. *By hand:* the overlay icons.
2. **Hydration policy:** measured with a second process reading 1000 bytes at 5 MB of a 48 MiB file.
   `PARTIAL`, `PROGRESSIVE` and `FULL` all produced one request for the whole file, and the file was then
   marked as fully here. `PARTIAL` therefore buys nothing on this build; `FULL` is what the code uses.
   Reading a large file means fetching all of it first. *By hand:* a media player seeking in a large video.
3. **Transfer alignment:** an unaligned final piece is accepted (files of 11, 10 001 and 3 MiB + 17 bytes).
   Requests arrive for the whole file, so an unaligned start did not occur.
4. **Failing a request:** a read fails within milliseconds, with `ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE` (388)
   for an unreachable host and `ERROR_CLOUD_FILE_UNSUCCESSFUL` (389) otherwise; the next read works. With the
   provider disconnected, a placeholder fails at once with "the cloud file provider is not running".
5. **Callback threading:** the blocking transport runs inside the callback and serves single opens over TLS.
   *Not measured:* many large files opened at once.
6. **`std::fs` and reparse points:** `read_dir`, `symlink_metadata` and `metadata` list every placeholder and
   fetch nothing; `is_symlink()` is false for them.
7. **Attribute changes reach the watcher:** `attrib +P -U` on a placeholder is noticed and the file fetched;
   `-P +U` frees it again.
8. **Files in use:** a conversion that fails is retried on the next scan. *By hand:* a document open in Word
   while it is pushed.
9. **Convert after push:** a file created in the root becomes an in-sync placeholder, can be freed, and comes
   back with the right content.
10. **Replacing a placeholder:** `CfUpdatePlaceholder` with `DEHYDRATE` replaces a hydrated clean file with a
    placeholder of the new size; a pinned file is downloaded and renamed over instead.
11. **In-sync tracking:** an edit clears the mark and a push sets it again. A **rename** and a new timestamp
    clear it too (see bug 6); pinning does not.
12. **Leaving:** hydrated files become ordinary files, placeholders without content go, the folder is an
    ordinary folder again. `CfUnregisterSyncRoot` alone already removes dehydrated placeholders and keeps
    hydrated files.
13. **Who fetches:** nothing read the test roots by itself (no fetch within seconds of creating
    placeholders, Defender active). *By hand:* Windows Search over a real root, third-party antivirus and
    backup tools, and MediaLake's own index run with a model.
14. **Volumes:** UNC and relative paths are refused; a non-NTFS volume was not at hand. Paths longer than
    260 characters work (the root is a `\\?\` path internally).
15. **Uninstall:** `MediaLake.exe --unregister-sync-roots` leaves hydrated files as plain files and removes
    placeholders and the registration. *Not run:* the Inno Setup uninstaller around it.

## Still to check by hand

Explorer's overlay icons and the "Always keep on this device" / "Free up space" menu entries on a real root;
thumbnails and previews (each is a full download of the file); an Office application and a media player on a
synced folder; Windows Search; Storage Sense freeing space; a second user account; Windows 10 1809.
The WinUI windows were rendered by the test hooks, not clicked through.
