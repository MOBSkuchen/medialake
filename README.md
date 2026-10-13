# MediaLake

Local, semantic search for your files: text, code, PDF/Office documents, images, audio and
(experimental) video are embedded **by content** with EmbeddingGemma 2 (llama.cpp) and found with natural-language
queries from a Spotlight-like window.

## Models

| `model` | Dimensions | Modalities | Download per quality (heavy / normal / light) |
|---|---|---|---|
| `embeddinggemma-2` (default) | 768 | text, images, audio, video | 1.5 / 0.9 / 0.7 GB |
| `qwen3-vl-embedding-2b` | 2048 | text, images, video | 4.3 / 2.7 / 1.9 GB |
| `qwen3-vl-embedding-8b` | 4096 | text, images, video | 9.2 / 7.0 / 5.4 GB |

Choose it in *Settings > Model* (or `model = "..."` in `config.toml`, `--model` for `medialake-server`). Changing the
model or the quality rebuilds the whole index. The Qwen3-VL-Embedding models have no audio encoder: audio files are
indexed by name only. Their GGUF files are community conversions (DevQuasar, mradermacher), see
`crates/medialake-proto/src/models.rs`.

Supported platforms: Windows (WinUI 3) and macOS 14+ on Apple Silicon (SwiftUI + AppKit, see `docs/plan2.md`).

## Layout

| Path | What |
|---|---|
| `crates/medialake-proto` | bierpc message types, model manifest, pairing-code codec, TLS pinning |
| `crates/medialake-llama` | thin wrapper over raw llama.cpp / mtmd (pinned b11535 via `vendor/`) |
| `crates/medialake-server` | `medialake-server`: embedding server (`--local` child, `serve`, `pair`, `clients`, `bench`) |
| `crates/medialake-sync` | file sync: host (watched folder, history), member engine, mirror frontend, `medialake-sync` CLI |
| `crates/medialake-mount` | macOS: synced folders as a mounted volume (NFSv3 on loopback + sparse cache), see `docs/a0-macos-mount.md` |
| `crates/medialake-cloudapi` | Windows: typed layer over the Cloud Files API (type-checks from any host; spike in `examples/`), see `docs/w0-cloud-files.md` |
| `crates/medialake-cloud` | Windows: synced folders as a Cloud Files sync root (placeholders that fill in when opened); base version, not yet run on Windows |
| `crates/medialake-core` | client logic + `medialake-cli` (diagnostics) |
| `crates/medialake-ffi` | C ABI for the UI (`medialake_core_ffi.dll`; a static library on macOS) |
| `ui/MediaLake.App` | Windows: WinUI 3 app (search window, tray, settings) |
| `ui/MediaLake.Mac` | macOS: SwiftUI + AppKit app (Swift package; search panel, status item, settings) |
| `installer/` | `medialake.iss` (Inno Setup); `macos/` pkg sources (Distribution.xml, scripts, resources) |
| `scripts/windows` | `dev-env.sh`, `setup-vendor.ps1`, `build-release.ps1`, `make-icon.ps1` |
| `scripts/macos` | `dev-env.sh`, `setup-vendor.sh`, `build-release.sh`, `make-icon.sh` |

`bierpc` / `bier_derive` are path dependencies on the sibling repo `../bierpc` (it needs the two uncommitted
fixes described in `poc/README.md`).

## Build (Windows)

Prerequisites (per-user installs are fine): Rust, .NET 10 SDK, VS 2022 Build Tools (C++), CMake, libclang
(portable LLVM; `LIBCLANG_PATH`), optionally the Vulkan SDK and Inno Setup 6. `scripts/windows/dev-env.sh` shows the
paths used on the development machine.

```powershell
scripts\windows\setup-vendor.ps1               # once: llama-cpp-rs + llama.cpp b11535 into vendor/
scripts\windows\build-release.ps1 -Installer   # builds server (CPU + Vulkan), core DLL, UI; stages dist\app; builds the installer
```

Dev loop:

```bash
source scripts/windows/dev-env.sh
cargo build --release                                   # server, cli, ffi dll
cargo test --release --workspace --lib
MEDIALAKE_MODEL_DIR=<dir with gguf> cargo test --release -p medialake-core --test e2e -- --ignored --test-threads=1
cd ui/MediaLake.App && dotnet build -c Release -p:Platform=x64
```

## Build (macOS)

Prerequisites: Rust, Xcode (command line tools are enough: `swift`, `codesign`, `pkgbuild`), CMake
(`brew install cmake`) and the sibling `../bierpc` checkout. Apple Silicon, macOS 14+.

```bash
scripts/macos/setup-vendor.sh                # once: llama-cpp-rs + llama.cpp b11535 into vendor/
scripts/macos/build-release.sh --installer   # builds server (Metal), core static library, UI; stages dist/app/MediaLake.app; builds the .pkg
```

Dev loop:

```bash
source scripts/macos/dev-env.sh
cargo build --release -p medialake-server -p medialake-core
cargo rustc --release -p medialake-ffi --crate-type staticlib     # libmedialake_core_ffi.a for the Swift package
cargo test --release --workspace --lib
MEDIALAKE_MODEL_DIR=<dir with gguf> cargo test --release -p medialake-core --test e2e -- --ignored --test-threads=1
swift build -c release --package-path ui/MediaLake.Mac          # or open ui/MediaLake.Mac/Package.swift in Xcode
```

The app is **ad-hoc signed** (no Developer ID, no notarisation). After downloading the `.pkg`, macOS refuses to
open it with a double click: use System Settings > Privacy & Security > "Open Anyway" once (or right-click the pkg,
Open). Apple Silicon needs a signature to run code at all; `MEDIALAKE_SIGN_ID` / `MEDIALAKE_INSTALLER_SIGN_ID` /
`MEDIALAKE_NOTARY_PROFILE` switch `build-release.sh` to Developer ID signing and notarisation.

macOS asks for permission the first time MediaLake reads Desktop, Documents, Downloads, iCloud Drive or an external
volume. A folder that was denied shows "No access" on Settings > Index with a button that opens System Settings >
Privacy & Security > Files and Folders. Because the signature is ad-hoc, macOS may ask again after an update (and for
the Keychain item when a remote server is paired).

Uninstall with Settings > General > Uninstall (moves the app to the Trash, removes the login item and the Keychain
item, optionally deletes the data folder). Dragging the app to the Trash by hand also works but leaves
`~/Library/Application Support/MediaLake` and the login item entry behind.

Test hooks of the app (the same on Windows): `--selftest`, `--screenshot <png>`, `--background`,
`MEDIALAKE_TEST_QUERY` / `_WAIT` / `_NOWAIT` / `_SETTINGS=<page>|<png>` / `_WELCOME=<folder>` (renders the welcome
window's pages); on macOS also `_WELCOME_ACTION=skip|continue` (presses that button and logs the resulting settings)
and `_SYNC_DEMO=<mount point>`. The log goes to `logs/selftest.log`.

## Remote server

```
medialake-server --model-dir D:\models --model qwen3-vl-embedding-2b --quality heavy serve --bind 0.0.0.0:47900 --public-addr myserver.example.com:47900
medialake-server pair            # prints a one-time pairing code (10 min)
medialake-server clients list | revoke <id>
```

Paste the code in *Settings > Model > On a remote server*. The connection is TLS with the server's
self-signed certificate pinned by SHA-256 fingerprint (contained in the code); the client's long-term secret is
stored DPAPI-protected (macOS: in the Keychain). File contents are sent to the remote server for embedding.
The client adopts the server's model and quality.

## File sync

MediaLake can also keep folders in step between paired devices (hub and spoke: one host owns the folder,
members mirror it), independent of search. Design: `docs/post-plan3.md`; what exists and how it differs:
`docs/post-plan3-status.md`; wire format: `docs/sync-protocol.md`.

```bash
# host: a plain folder, watched; headless, no model needed
medialake-server sync add Photos ~/Pictures --backup file:10      # none | time:24h:7 | file:10
medialake-server serve --no-embed
medialake-server pair --sync Photos:rw                            # ro for read-only; --embed adds embedding

# member (the app does the same through its Sync settings)
medialake-sync pair <code> && medialake-sync join <host> Photos ~/MediaLake/Photos
medialake-sync run                                                # or: once | status | conflicts | resolve
```

On macOS a joined folder is a **volume under `~/MediaLake`**: every file is listed at full size at once and its
content is fetched, block by block, when something reads it; "Keep on this Mac" pins a folder, a size limit evicts
the least recently used clean files, edits made offline are sent when the host is back. It needs nothing
installed (the system's NFS client mounts a server inside the app) and falls back to a plain mirror folder if the
mount is refused. Choose "Keep a full copy in a normal folder" to always get a mirror. Why this mechanism and
what was measured: `docs/a0-macos-mount.md`.

Conflicts keep both versions (*server wins*: the local one becomes `name (conflict, <device>, <date>).ext`
and is shared; *ask*: it waits for `resolve … mine|server|both`). History lives on the host
(`medialake-server sync versions|snapshots|restore`). Sync is off by default in the app (`[sync] enabled`),
and `search_enabled = false` turns the app into a pure sync client / host.

Tests that need no model: `cargo test -p medialake-proto -p medialake-sync`; soak tests with real
transfers are `--ignored` in `crates/medialake-sync/tests/network.rs`. On macOS
`cargo test -p medialake-mount` really mounts (no root needed) and drives the volume with ordinary file calls
and a Cocoa process; `cargo test -p medialake-core --test sync` does the same through the app's core.

## Data locations

`%LOCALAPPDATA%\MediaLake\` : `config.toml`, `index.db` (+ `vectors.usearch`), `models\`, `logs\`
(`client.log`, `server.log`, `ui.log`). Override with `MEDIALAKE_HOME` / `MEDIALAKE_MODEL_DIR`. On macOS the same files live in
`~/Library/Application Support/MediaLake/`.

## License

GNU Affero General Public License v3, see `LICENSE`. Both installers show it on their licence page.
