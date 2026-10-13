# W0–W2: Cloud Files on Windows — what is built, what is assumed, what to check

This is a **base version**. It was written on a Mac, against the `windows` crate's bindings: the code
type-checks for `x86_64-pc-windows-msvc` and its platform-independent logic is tested, but **none of it has
run on Windows**. This note is the list of things to confirm (and the first things to fix) on a Windows
machine. Plan: `post-plan3.md` §8.2, §10, §11.

## What exists

| Piece | Where | Verified how |
|---|---|---|
| Typed layer over cfapi (`CfRegisterSyncRoot`, `CfConnectSyncRoot`, `CfCreatePlaceholders`, `CfExecute` TRANSFER_DATA, `CfUpdatePlaceholder`, `CfConvertToPlaceholder`, pin, hydrate, dehydrate, revert) | `crates/medialake-cloudapi/src/win.rs` | **type-checks**: `cargo check -p medialake-cloudapi --target x86_64-pc-windows-msvc --all-targets` (works from a Mac) |
| Platform-neutral half: identity blob, 4 KiB transfer planner, callback-path mapping, recall-attribute test, the `CloudApi` / `Provider` traits | `…/cloudapi/src/{types,plan}.rs` | unit tests (host) |
| **W0 spike**: registers a root, creates placeholders, serves FETCH_DATA from a slow source, reacts to pin / unpin | `crates/medialake-cloudapi/examples/spike.rs` | type-checks; **not run** |
| **W1 frontend**: the sync engine's `Frontend` over a Cloud Files root; the provider that serves FETCH_DATA from the host | `crates/medialake-cloud/` | 11 integration tests against the real engine and host over a *stand-in* platform (sparse files + a read that calls the provider) |
| Core wiring: frontend kind `cloud` (auto on Windows, mirror fallback with a notice), browse / pin / unpin / leave, `sync_unregister_roots` | `medialake-core/src/sync.rs`, `core.rs`, `medialake-ffi/src/dispatch.rs` | core test for the refusal off Windows; the Windows branch is not runnable here |
| **X1**: the indexer treats any file with `RECALL_ON_DATA_ACCESS` / `RECALL_ON_OPEN` / `OFFLINE` as "would download": name only, content once it is local (this also covers OneDrive folders). Search hits carry `available`, so that UIs show an icon instead of a thumbnail that would download the file | `medialake-core/src/indexer.rs`, `search.rs`; `ui/*/…` | logic is the same code path as the macOS gate (not model-tested); the Windows attribute read is untested |
| **W2**: WinUI Settings > Sync page, tray lines + "Pause all syncing" + balloon notifications, uninstall leaves the syncs and unregisters the roots (`--unregister-sync-roots`) | `ui/MediaLake.App/SyncPage.xaml(.cs)`, `Native/Shell.cs`, `App.xaml.cs`, `installer/medialake.iss` | **never compiled** (no .NET SDK on the dev Mac) |
| **X2** (Windows UI half): speed limits as switch + number, Check again (verify), the error states, welcome window | same | never compiled |

What the engine side guarantees (tested with the stand-in): a clean placeholder is never read; its
`{rev, hash}` identity answers `content_hash`, so a lost `state.db` re-adopts the tree without downloading it;
pushed or downloaded files become in-sync placeholders; a remote change to a clean placeholder updates and
frees it unless it is pinned; leaving keeps hydrated files and removes placeholders.

## Design in one page

* **Sync root** per sync, default `%USERPROFILE%\MediaLake\<name>`, registered with plain `CfRegisterSyncRoot`
  (no package identity, no `StorageProviderSyncRootManager`; see question 1). A marker file
  `.medialake-sync` (an ignored name) says the folder is ours; a non-empty folder without it is refused and
  the sync falls back to a mirror with a notice.
* **Population** is *always full*: every placeholder is created from the host's listing, so Explorer never
  asks the provider to enumerate and browsing needs no network.
* **Hydration** is *partial*: only the ranges that are read are fetched (a thumbnail of a 2 GB video does not
  pull 2 GB). The plan said "full"; see question 2.
* **Callbacks**: `FETCH_DATA` (served from the engine's transport with the revision from the placeholder's
  identity, in 4 KiB-aligned pieces of at most 4 MiB, with progress) and `CANCEL_FETCH_DATA`. A failed fetch
  is answered at once with an error status (`NETWORK_UNAVAILABLE` for an unreachable host) instead of leaving
  Explorer to time out.
* **No `NOTIFY_DELETE` / `NOTIFY_RENAME`**: registering them would oblige us to acknowledge every delete and
  rename; the file watcher sees them as it does for a mirror.
* **Pinning**: Explorer's "Always keep on this device" / "Free up space" only set attributes. The frontend
  watches attribute changes (and walks the tree once at start) and hydrates / dehydrates accordingly.
* **Read-only members**: placeholders carry the read-only attribute; edits that happen anyway are handled by
  the engine's existing revert-and-keep-a-copy rule.

## Questions to answer on a Windows machine

Run the spike first (`cargo run -p medialake-cloudapi --example spike -- C:\Users\you\MediaLakeSpike`), then
the app. Each item says what the code assumes.

1. **Does an unpackaged process get a usable sync root from `CfRegisterSyncRoot` alone?** The code assumes it
   registers and connects. It does *not* create the navigation-pane entry or the "Status" column
   (those come from `StorageProviderSyncRootManager.Register`, which may need package identity). Look for the
   cloud / check overlay icons on files. If the pane entry is wanted and WinRT registration works unpackaged,
   add it next to `register`.
2. **Hydration policy.** `win.rs::HYDRATION` is `PARTIAL`. Open a large file's thumbnail in Explorer and
   watch the spike log: do the requests stay small? Try `PROGRESSIVE` (streaming reads) and `FULL` (the
   plan's choice) and compare. Does a media player seek correctly?
3. **Transfer alignment.** The code sends 4 KiB-aligned offsets and lengths, except for the piece that ends
   at the end of the file. Confirm that unaligned final pieces are accepted and that a request that starts
   unaligned (Office does this) is answered correctly. The failing case would show as
   `CfExecute` errors in the log.
4. **Failing a request.** `abort()` ends a FETCH_DATA with `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE` /
   `…_UNSUCCESSFUL` and a null buffer. Disconnect the network (or stop the host) and open a placeholder:
   Explorer must show an error at once, and the next open after the host is back must work.
5. **Callback threading.** The fetch runs the engine's *blocking* transport inside the system's callback.
   Open several large files at once and watch for stalls: if Explorer hangs, move the work to a thread pool
   and keep the callback short (the platform waits for TRANSFER_DATA by `RequestKey`).
6. **`std::fs` and reparse points.** The scan uses `read_dir` / `symlink_metadata`, which must not open file
   data, and `FileType::is_symlink()` must be false for cloud placeholders (the mirror's walk skips
   symlinks). Scan a root with 1 000 placeholders and check that nothing is hydrated (the spike log stays
   silent) and that every file is listed.
7. **Attribute changes reach the watcher.** Pin and free up a file in Explorer. The frontend relies on the
   `notify` crate reporting attribute changes; if it does not, pins are only noticed at the next start
   (`reconcile_pins`). A periodic walk is the fallback.
8. **`CfOpenFileWithOplock` on files that are in use**, and on directories. Edit a file in Word and let the
   sync push it: `mark_synced` may hit a sharing violation; the frontend retries on the next scan. Confirm
   that it succeeds eventually and that the file ends up shown as synced.
9. **Convert after push.** A file created in the root becomes a placeholder with `CfConvertToPlaceholder`
   (`MARK_IN_SYNC`). Check that "Free up space" then works on it and that the file is re-fetched correctly.
10. **Replacing a placeholder.** `place_placeholder` for a changed file uses `CfUpdatePlaceholder` with
    `DEHYDRATE`; a plain file in the way is deleted and the placeholder created. Check both, and
    `place_file` (rename over a placeholder) for pinned files.
11. **In-sync tracking.** `InSync = TRACK_ALL`. Edit a hydrated file and check that its state flips to "not in
    sync" (the engine uses it to decide whether to hash) and back after the push.
12. **Leaving.** `leave_root` reverts hydrated files (`CfRevertPlaceholder`), deletes the placeholders without
    content and unregisters the root. Check that nothing is left that Explorer shows as a broken cloud file,
    and that unregistering a root whose placeholders were not reverted behaves as expected.
13. **Windows Search, antivirus, backup tools.** Do they hydrate the root? (They are the usual cause of "the
    whole sync downloaded".) The indexer of MediaLake itself must not: with a model, run an index over a
    root of placeholders and confirm that the status stays `deferred` and nothing is fetched.
14. **Volumes.** Roots on a non-NTFS or network drive are refused (`available`). Confirm the message and the
    fallback to a mirror. Long paths (`\\?\`) are not handled yet in the mirror either.
15. **Uninstall.** Run the uninstaller with a synced root: `MediaLake.exe --unregister-sync-roots` must run
    after the app is killed, leave the hydrated files in place and remove the registration.

## WinUI notes (first compile)

Expect compile errors in the files under `ui/MediaLake.App` that are new or changed since the last build
(`WelcomeWindow`, `SyncPage`, `App.xaml.cs`, `Native/Shell.cs`, `SettingsWindow`, `MainWindow`):
`SyncPage` builds its content in code from the core's JSON, using `Application.Current.Resources` brushes
and styles by name (`CardBackgroundFillColorDefaultBrush`, `SystemFillColorCautionBrush`, …); a missing key
only means a plainer look. The tray balloon is `NIF_INFO` on the existing notification icon, so it needs no
registration; a click raises `BalloonClicked` and opens Settings > Sync.
