# MediaLake – macOS Port Plan (v2)

Builds on `plan1.md` (the Windows app, all phases done). Goal: the same app on macOS, with an equivalent
native UI and an installer, keeping the repository layout and the five Rust crates. Nothing in this plan
changes what the Windows build produces.

Decisions:

| Topic | Decision |
|---|---|
| Rust crates | **Unchanged set and names.** Windows-only code moves behind `cfg(windows)` with a macOS sibling next to it; no new crate, no fork |
| UI | **Swift, SwiftUI + AppKit**, in `ui/MediaLake.Mac/`, mirroring `ui/MediaLake.App/` file by file. UI only, like the C# host |
| Core in the UI process | `medialake-ffi` linked into the Swift app as a **static library** (same `sn_*` C ABI, same JSON). One process, as on Windows |
| Local server | `medialake-server` child process inside the app bundle, same supervisor, same bierpc path |
| GPU | **Metal** (llama.cpp's Apple backend) replaces Vulkan. **One** server binary; no `-cpu` twin |
| Targets | **Apple Silicon (arm64), macOS 14+.** Intel is not planned (see §9) |
| Data | `~/Library/Application Support/MediaLake/` with the same files as `%LOCALAPPDATA%\MediaLake\` |
| Secrets | **Keychain** replaces DPAPI |
| Installer | **`.pkg`** (pkgbuild + productbuild). Hotkey and autostart are asked on first run in the app, because a pkg cannot show input pages |
| Distribution | Outside the App Store, not sandboxed. Developer ID signing + notarisation are optional switches in the build script |
| Scripts | `scripts/windows/` and `scripts/macos/`, one variant of each script per platform ✅ done with this plan |

---

## 1. Architecture

Identical to plan1 §1; only the host and the platform services differ.

```
┌──────────── MediaLake.app (always running, menu bar) ─────────────┐
│                                                                    │
│  Swift / SwiftUI + AppKit (UI only)   Rust: libmedialake_core_ffi.a│
│  ┌────────────────────┐  C ABI   ┌───────────────────────────────┐ │
│  │ Search panel       │ ───────► │ medialake-core (unchanged     │ │
│  │ Settings window    │ ◄─────── │ logic, macOS cfg branches)    │ │
│  │ Status item, hotkey│ callbacks└──────────────┬────────────────┘ │
│  └────────────────────┘                         │                  │
└─────────────────────────────────────────────────┼──────────────────┘
                     local: 127.0.0.1 + token     │ remote: TLS, pinned
            ┌─────────────────────────────────────┴──────┐
            ▼                                            ▼
 Contents/MacOS/medialake-server (child)      medialake-server (any OS)
 llama.cpp + Metal                            llama.cpp
```

A Mac client can pair with a Windows server and the other way round: the protocol, the pairing code and
the index format do not change.

## 2. Repository Layout

New entries are marked `+`. Everything else stays where it is.

```
medialake/
├─ Cargo.toml
├─ crates/                              # same five crates
│  ├─ medialake-proto/
│  ├─ medialake-llama/
│  ├─ medialake-server/
│  ├─ medialake-core/
│  └─ medialake-ffi/
├─ ui/
│  ├─ MediaLake.App/                    # C# WinUI 3 (Windows)
│  └─ MediaLake.Mac/                  + # Swift package (macOS)
│     ├─ Package.swift
│     ├─ Sources/
│     │  ├─ CMediaLakeCore/           + # C module: generated header (≙ NativeMethods.g.cs)
│     │  │  ├─ include/medialake_core_ffi.h
│     │  │  └─ shim.c
│     │  └─ MediaLake/
│     │     ├─ App.swift              + # ≙ App.xaml.cs
│     │     ├─ MainWindow.swift       + # ≙ MainWindow.xaml(.cs)
│     │     ├─ SettingsWindow.swift   + # ≙ SettingsWindow.xaml(.cs)
│     │     └─ Native/
│     │        ├─ CoreClient.swift    + # ≙ CoreClient.cs
│     │        └─ Shell.swift         + # ≙ Shell.cs + Win32.cs (hotkey, status item, file actions)
│     └─ Resources/                   + # Info.plist, MediaLake.entitlements, AppIcon.icns
├─ installer/
│  ├─ medialake.iss                     # Inno Setup (Windows)
│  └─ macos/                          + # Distribution.xml, component.plist, scripts/postinstall, resources/
├─ scripts/
│  ├─ windows/                          # build-release.ps1, setup-vendor.ps1, make-icon.ps1, dev-env.sh
│  └─ macos/                            # build-release.sh, setup-vendor.sh, make-icon.sh, dev-env.sh
└─ docs/
```

The app builds with the plain `swift` CLI from a Swift package, in the same spirit as the Windows UI
building with the plain `dotnet` CLI. There is no `.xcodeproj` to maintain; Xcode opens `Package.swift`
directly. The build script assembles the `.app` bundle by hand.

## 3. Rust changes

The port needs a Mac to build and test; none of this can be verified from Windows. Each row is a place
where the code is Windows-only today.

### 3.1 Platform map

| Where | Windows today | macOS |
|---|---|---|
| `medialake-core/Cargo.toml` | `windows-sys` and `windows` are unconditional dependencies | Move both under `[target.'cfg(windows)'.dependencies]`. Add `[target.'cfg(target_os = "macos")'.dependencies]`: `security-framework`, `libc`, and the `objc2-*` crates for video (§3.4) |
| `medialake-llama/Cargo.toml` | `llama-cpp-sys-2` with `mtmd` + `openmp`; `vulkan` feature | Target-specific dependency tables: keep `mtmd` + `openmp` off macOS, use `mtmd` + `metal` on macOS. Apple clang has no OpenMP; llama.cpp threads without it. The `vulkan` feature stays and is simply not used on macOS |
| `medialake-server/src/main.rs:107-111` | `backend_name` and the `gpu_layers` default only know `vulkan` | macOS reports `metal` and offloads all layers by default |
| `core.rs:91`, `core.rs:192`, `tests/e2e.rs:14` | `medialake-server.exe`, `medialake-server-cpu.exe` | Build names with `std::env::consts::EXE_SUFFIX`. The CPU twin is looked up as today and is absent on macOS, which the code already tolerates |
| `supervisor.rs:275` | `CREATE_NO_WINDOW` | Nothing needed (already `cfg(windows)`) |
| `supervisor.rs` `job` module | Job Object kills the child with the client | No Job Object. The server already exits when its stdin closes and the child is `kill_on_drop`; verify by force-quitting the app and checking that the server is gone |
| `config.rs:132`, `server/state.rs:60` | `LOCALAPPDATA` | `~/Library/Application Support/MediaLake` (and `…/MediaLake/server`). Put the lookup in one helper so the two crates cannot drift. `MEDIALAKE_HOME` / `MEDIALAKE_MODEL_DIR` keep working |
| `config.rs` default hotkey | `Ctrl+Alt+Space` | `Alt+Space` (shown as ⌥Space). Not Cmd+Space, which is Spotlight |
| `config.rs` `normalize_path` | Strips `\\?\` | No-op on macOS already |
| `remote.rs` `protect` / `unprotect` | DPAPI | **Keychain** (§3.3). Today's non-Windows fallback stores the secret as plain hex, which must not ship |
| `core.rs:258`, `server/serve.rs:52` | `COMPUTERNAME` | One `host_name()` helper: `COMPUTERNAME`, else `gethostname` |
| `indexer.rs:686` | `THREAD_PRIORITY_BELOW_NORMAL` | `pthread_set_qos_class_self_np(QOS_CLASS_UTILITY)`. Not `BACKGROUND`: that pins the thread to efficiency cores and throttles I/O, which makes a first index take far too long |
| `extract.rs:89` `is_hidden_or_system` | File attributes | `UF_HIDDEN` from `st_flags` |
| `extract.rs` `is_ignored` | `$RECYCLE.BIN`, `System Volume Information`, `thumbs.db`, `desktop.ini` | Add `.Trash`, `.Spotlight-V100`, `.fseventsd`, `.DocumentRevisions-V100`, and `~/Library`. Dot-files are already skipped. Do **not** descend into packages: `.app`, `.framework`, `.bundle`, `.photoslibrary`, `.xcodeproj` and similar |
| `video.rs` | Media Foundation | AVFoundation (§3.4) |
| `watcher.rs` | `notify` → ReadDirectoryChangesW | `notify` → FSEvents, same crate (§3.2) |
| `medialake-ffi/build.rs` | csbindgen → `NativeMethods.g.cs` | Also run **cbindgen** → `ui/MediaLake.Mac/Sources/CMediaLakeCore/include/medialake_core_ffi.h`. Both files are generated on every build and checked in |
| `medialake-ffi` crate type | `cdylib` | Unchanged in `Cargo.toml`. The macOS build runs `cargo rustc -p medialake-ffi --release --crate-type staticlib`, so the Windows build output is untouched |

Unchanged and expected to just build: `medialake-proto`, bierpc, `rusqlite` (bundled), `usearch` (keep the
`=2.21.4` pin so both platforms run the same version), `symphonia`, `pdf-extract`, `zip`, `image`, `ureq`,
`rcgen`, the model manager, search and calibration.

### 3.2 Watcher

`notify` picks FSEvents on macOS. Two differences matter:

- **Renames arrive unpaired.** FSEvents reports a rename as a name change on each path, without the
  `From`/`To`/`Both` pairing Windows gives. `watcher.rs:59` already has a catch-all for
  `ModifyKind::Name(_)`; it must resolve each path by checking whether it exists (exists = new name,
  missing = old name). Test: rename a file, move it between two indexed roots, move it out of the index,
  and rename a directory with files in it. Content must not be re-embedded for a pure rename.
- **Events are coalesced** and can be delayed by about a second. The per-file debounce already absorbs this.

Paths are compared lower-cased today (`indexer.rs:55`), which matches the default case-insensitive APFS.
A case-sensitive volume could hold `A.txt` and `a.txt` in one folder and the index would treat them as one
file. Accepted for v1 and listed in §9.

### 3.3 Credentials

`protect` stores the secret as a generic password in the login keychain (service `MediaLake`, account =
`client_id`) and returns the reference `keychain:<client_id>`, which goes into the existing
`secret_protected` config field. `unprotect` reads it back. The signatures and the config schema do not
change. Uninstall deletes the item.

Keychain access is tied to the code signature. Ad-hoc signed development builds get a new identity on every
rebuild, so macOS asks for the keychain password again after each build. That is expected and goes away
with a stable Developer ID.

### 3.4 Video and images

- **Video:** same contract as today (`prepare_video` → up to 8 evenly spaced JPEG frames plus a hash),
  implemented with `AVAssetImageGenerator` through the `objc2-av-foundation` family of crates, so the
  logic stays in `medialake-core` like the Media Foundation code. Nothing is bundled.
- **HEIC:** the `image` crate cannot decode HEIC, and most photos on a Mac are HEIC. Decode them with
  ImageIO (`CGImageSource`) behind `cfg(target_os = "macos")` and feed the pixels into the existing
  downscale + JPEG path. Without this the port would index a typical photo folder by file name only.

### 3.5 FFI

The C ABI does not change: `sn_version`, `sn_last_error`, `sn_init`, `sn_shutdown`,
`sn_set_event_callback`, `sn_call`, `sn_free_string`, with the same method names and JSON as in
`dispatch.rs`. The Swift host passes `server_exe` to `sn_init`, as the C# host does.

## 4. MediaLake.Mac (Swift)

Every control and behaviour of the Windows UI has a counterpart. Text, defaults, slider ranges and the
log-scale mappings are copied from the C# code so both apps read the same.

### 4.1 App shell (`App.swift`, `Native/Shell.swift`)

| Windows | macOS |
|---|---|
| Tray icon, left click toggles, right click menu (Open / Settings / Quit) | `NSStatusItem` in the menu bar with the same three menu items. `LSUIElement = true`: no Dock icon, no app menu |
| `RegisterHotKey` + window subclass | Carbon `RegisterEventHotKey`. It needs **no** Accessibility or Input Monitoring permission |
| Hotkey string `Ctrl+Alt+Space` | Same string format in `config.toml`. Tokens: `Ctrl`, `Alt` (Option), `Shift`, `Cmd`. `Win` and `Meta` parse as `Cmd`, so a config copied from Windows still loads. Shown with symbols (⌃⌥⇧⌘) |
| Named mutex + event for single instance | `LSMultipleInstancesProhibited`; a second launch reaches `applicationShouldHandleReopen`, which shows the search panel |
| `--background` (started at login: stay hidden) | Same flag, passed by the login item |
| `--selftest`, `--screenshot <png>` and the `MEDIALAKE_TEST_*` variables | Same flags and variables. Screenshots via `NSView.cacheDisplay`. `selftest.log` goes to the logs folder, because writing into a signed bundle breaks its signature |
| Core events marshalled with `DispatcherQueue` | Callback arrives on a Rust thread; hop to `DispatchQueue.main` |
| `ui.log` in the logs folder | Same file, same folder |

### 4.2 Search window (`MainWindow.swift`)

- A borderless `NSPanel` (`.nonactivatingPanel`, can become key) hosting a SwiftUI view, with an
  `NSVisualEffectView` backdrop and rounded corners. This is the native counterpart of the borderless Mica
  window. It floats above other windows, joins all Spaces and shows over full-screen apps.
- Shown centred on the **screen under the mouse pointer**. Hidden on Esc and when it loses key status. The
  panel is created once and only shown and hidden.
- It grows when there are results and shrinks back when empty, like the Windows window.
- Layout, top to bottom, same as `MainWindow.xaml`:
  1. Large bold title on the left, gear button on the right.
  2. Search field (placeholder "Search files, images, audio…"), debounced, newest request wins.
  3. Thin indeterminate bar while a search runs.
  4. Result list. Each row: 44 pt thumbnail or file icon, name, folder, optional two-line snippet, coloured
     modality badge, score, and a **"Show in Finder"** button.
  5. Empty-state text, or the **first-run panel**: quality picker with sizes, Download button, progress.
  6. Index progress bar and status line, including the ETA ("… left").
- Thumbnails: `QLThumbnailGenerator`, falling back to `NSWorkspace.icon(forFile:)`.
- Keys: Up / Down move, Return opens with the default app (`NSWorkspace.open`), **⌘Return** shows in Finder
  (`activateFileViewerSelecting`, the counterpart of Ctrl+Enter), Esc hides.

### 4.3 Settings window (`SettingsWindow.swift`)

A normal window with a `NavigationSplitView` sidebar: **General**, **Model**, **Index**. Settings load with
`get_settings` and save with `set_settings` as one JSON object; the Swift side keeps the object it loaded
and only changes the fields it edits, so fields it does not know survive a save.

| Page | Windows control | macOS control |
|---|---|---|
| General | Hotkey box (click, press combination; validated by registering it) | Recorder view with the same flow and the same error texts |
| General | "Start MediaLake when I sign in" (HKCU Run key) | Toggle backed by `SMAppService.mainApp` |
| General | "Kill daemon" with confirmation | Same button and dialog; quits the app and the server |
| General | "Uninstall" (starts the Inno uninstaller) | Same button; runs the in-app uninstall (§5.3) |
| Model | Local / Remote radio buttons | Radio-style `Picker` |
| Model | Model dropdown, Quality dropdown, info text, Download now, progress, Cancel | Same controls. Switching quality shows the same confirm-reindex dialog |
| Model | Pairing code box, Connect, status, remote quality, privacy info bar | Same, with the same privacy text |
| Model | Confidence threshold slider | `Slider` 0–1, step 0.01 |
| Model | "Experimental" expander with "Embed video content" | `DisclosureGroup` with a toggle and the same note |
| Index | Folder list with "Include subfolders" and remove | `List` with a checkbox and a remove button per row |
| Index | "Add folder…" (folder picker) | `NSOpenPanel` (directories only) |
| Index | Three sliders: wait after modification, minimum change, maximum file size | Same three sliders, same ranges and labels |
| Index | Stats text, "Reindex all" | Same |

### 4.4 Permissions

- The app is **not sandboxed**: it must read arbitrary folders the user picks and start a child process.
- macOS asks the user before an app reads Desktop, Documents, Downloads, iCloud Drive, removable or network
  volumes. `Info.plist` carries a usage text for each. Only the app reads files; the server receives bytes
  over the socket and never touches the disk, so the user sees prompts for "MediaLake" only.
- If the user denies a folder, reading fails with a permission error. The index page shows that folder as
  "no access" with a button that opens System Settings → Privacy & Security → Files and Folders.

## 5. Installer

### 5.1 Package

`MediaLake-<version>.pkg`, built by `scripts/macos/build-release.sh --installer`:

1. `pkgbuild` wraps `dist/app/MediaLake.app` as a component for `/Applications`. The component plist sets
   `BundleIsRelocatable = false`; otherwise Installer silently "updates" any other copy of the app it finds
   (for example the one in the build folder) instead of installing to `/Applications`.
2. `productbuild` adds the wizard: welcome, licence, install, summary (`installer/macos/Distribution.xml`
   and `resources/`). It allows "Install for me only" (`~/Applications`, no admin password), matching the
   per-user Windows install.
3. `scripts/postinstall` starts the app for the logged-in user, the counterpart of "Start MediaLake now".
4. Optional: sign with a Developer ID Installer certificate, notarise, staple.

### 5.2 What moves from the installer into the app

| Windows installer | macOS |
|---|---|
| Hotkey wizard page, written to `config.toml` on first install | **First-run screen in the app.** A pkg cannot show an input page. The search panel's first-run panel gets a hotkey recorder above the model download |
| "Start MediaLake when I sign in" task | Checkbox on the same first-run screen (`SMAppService`); also in Settings |
| Desktop shortcut task | Dropped; not a macOS convention |
| Model download | Already in the app on both platforms |
| `CloseApplications` on upgrade | `scripts/preinstall` quits a running MediaLake before files are replaced |

### 5.3 Uninstall

macOS has no uninstaller concept, so Settings → General → Uninstall does the work:

1. Ask for confirmation, with the same question as Windows: also delete the index, settings and models?
2. Unregister the login item and delete the keychain entry.
3. Optionally delete `~/Library/Application Support/MediaLake`.
4. Start a detached helper script that waits for the app to exit and then moves `MediaLake.app` to the Trash.
5. Quit.

Dragging the app to the Trash by hand also works; it leaves the data folder and the login item entry
behind, which the README documents.

### 5.4 Signing

- Apple Silicon refuses to run unsigned code, so the build script always signs: ad-hoc by default, with a
  Developer ID when `MEDIALAKE_SIGN_ID` is set. The nested `medialake-server` is signed first, then the
  bundle, both with the hardened runtime.
- Without a Developer ID and notarisation, Gatekeeper blocks the downloaded pkg; the user has to allow it
  under System Settings → Privacy & Security. That matches the unsigned Windows installer (SmartScreen
  warning) and is acceptable for now. A Developer ID needs a paid Apple Developer membership.

## 6. Scripts ✅

Done together with this plan. Each script has one variant per platform:

| Purpose | `scripts/windows/` | `scripts/macos/` |
|---|---|---|
| Fetch the pinned llama.cpp into `vendor/` | `setup-vendor.ps1` | `setup-vendor.sh` |
| Toolchain environment | `dev-env.sh` (Git Bash) | `dev-env.sh` |
| Build, stage, optionally build the installer | `build-release.ps1 [-Version x] [-Installer]` | `build-release.sh [--version x] [--installer]` |
| Generate the app icon | `make-icon.ps1` → `.ico` | `make-icon.sh` → `.icns` |

The Windows scripts only moved; their repo-root lookup went up one level. `build-release.sh` and
`make-icon.sh` target the layout in §2 and **have not been run**: they need `ui/MediaLake.Mac` and
`installer/macos`, and a Mac. `.gitattributes` forces LF for `*.sh`.

## 7. Docs

| File | Change |
|---|---|
| `README.md` | ✅ Layout table, script paths, a macOS build section and macOS data locations (marked as planned). Once the port works: remove "planned", add permissions and uninstall notes |
| `abstract.md` | ✅ UI section names both hosts; "Open in Explorer" / "Show in Finder" |
| `vendor/README.md` | ✅ Both setup scripts |
| `plan1.md` | ✅ Script path, pointer to this plan |
| `installer/medialake.iss` | ✅ Script path in the comments |
| `docs/calibration.md` | Later: add a Metal row if scores differ from Vulkan/CPU (they should not) |

## 8. Phases / Milestones

Prerequisite: both repositories on the Mac. `medialake` has no git remote yet, and `bierpc` /
`bier_derive` are path dependencies on the sibling `../rpc`, including the two fixes from `poc/README.md`.

| # | Phase | Done when |
|---|---|---|
| M0 | **Spikes (risk first)** | (a) `medialake-llama` builds with Metal on arm64 at llama.cpp b11535 and the ignored embedding test passes for text, image and audio. (b) A Swift package links the Rust static library, calls `sn_version`, and receives a callback from a Rust thread. (c) A non-activating `NSPanel` takes keyboard input and a Carbon hotkey toggles it |
| M1 | Rust builds and passes | §3.1 done; `cargo test --workspace --lib` and the e2e test pass on macOS; `medialake-server bench` numbers for Metal recorded; `medialake-cli search` works end to end; the Windows build is unchanged |
| M2 | Watcher + platform services | Rename/move/delete tests from §3.2 pass; Keychain round-trip; child server dies with the client; ignore list and package handling |
| M3 | App shell + search window | Hotkey → panel on the current screen → live results → open / show in Finder; status item; single instance; first-run model download |
| M4 | Settings | All three pages wired; folder add/remove; quality switch + download; login item; remote pairing against a Windows server |
| M5 | Media | Video frames via AVFoundation behind the experimental toggle; HEIC via ImageIO; thumbnails |
| M6 | Installer | `build-release.sh --installer` produces a pkg; clean-machine install, upgrade over a running app, uninstall with and without data |
| M7 | Polish + docs | Permission-denied states, `--selftest` / `--screenshot`, README final, signing + notarisation if a Developer ID exists |

## 9. Open Questions / Risks

1. **llama.cpp + Metal at the pinned tag.** The multimodal path (`mtmd`, the `gemma-embedding2`
   architecture) has only been run with Vulkan and CPU. If Metal gives wrong or NaN vectors for media,
   fall back to `--gpu-layers 0` for the projector or the whole model and compare speed. This is spike M0a
   and decides whether the port is worth continuing as planned.
2. **Text input in a non-activating panel.** This is how Spotlight-style launchers work, but focus handling
   is fiddly (first responder, hide on resign key, full-screen Spaces). Spike M0c.
3. **Static linking.** The Swift package must link the `.a` by path and name the system frameworks and
   `libc++` the Rust code needs (`cargo rustc … -- --print native-static-libs` lists them). If this turns
   out brittle, embed the `cdylib` in `Contents/Frameworks` and sign it instead.
4. **Folder permissions.** Prompts appear on first access per protected folder and can be denied. A denied
   folder must show as such rather than as an empty index.
5. **Developer ID.** Without the paid membership the pkg is not notarised and Gatekeeper warns. Decide
   before the first public release, not before development.
6. **Bundle identifier.** `com.medialake.app` is a placeholder. It keys the keychain entry, the login item
   and the permission grants, so it must be final before anyone installs a build.
7. **Intel Macs.** Not planned: no Intel hardware to test on, and Metal performance for this model on
   Intel GPUs is unknown. A universal build is possible later (`lipo` of two server builds).
8. **Case-sensitive volumes.** See §3.2.
9. **Window title.** The Windows search window still shows "SearchNow" after the rename to MediaLake. The
   Mac window should show whatever Windows settles on; decide once and use it in both.
10. **Not in scope:** App Store distribution, auto-update, iCloud "dataless" files (indexing would download
    them; skip files that are not local), Spotlight importer or Quick Look integration.
