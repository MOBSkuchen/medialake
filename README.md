# MediaLake

Local, semantic search for your files: text, code, PDF/Office documents, images, audio and
(experimental) video are embedded **by content** with EmbeddingGemma 2 (llama.cpp) and found with natural-language
queries from a Spotlight-like window.

Supported platforms: Windows (WinUI 3) and macOS 14+ on Apple Silicon (SwiftUI + AppKit, see `docs/plan2.md`).

## Layout

| Path | What |
|---|---|
| `crates/medialake-proto` | bierpc message types, model manifest, pairing-code codec, TLS pinning |
| `crates/medialake-llama` | thin wrapper over raw llama.cpp / mtmd (pinned b11535 via `vendor/`) |
| `crates/medialake-server` | `medialake-server`: embedding server (`--local` child, `serve`, `pair`, `clients`, `bench`) |
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

Test hooks of the app (same as on Windows): `--selftest`, `--screenshot <png>`, `--background`,
`MEDIALAKE_TEST_QUERY` / `_WAIT` / `_NOWAIT` / `_SETTINGS=<page>|<png>`; the log goes to `logs/selftest.log`.

## Remote server

```
medialake-server --model-dir D:\models --quality heavy serve --bind 0.0.0.0:47900 --public-addr myserver.example.com:47900
medialake-server pair            # prints a one-time pairing code (10 min)
medialake-server clients list | revoke <id>
```

Paste the code in *Settings > Model > On a remote server*. The connection is TLS with the server's
self-signed certificate pinned by SHA-256 fingerprint (contained in the code); the client's long-term secret is
stored DPAPI-protected (macOS: in the Keychain). File contents are sent to the remote server for embedding.

## Data locations

`%LOCALAPPDATA%\MediaLake\` : `config.toml`, `index.db` (+ `vectors.usearch`), `models\`, `logs\`
(`client.log`, `server.log`, `ui.log`). Override with `MEDIALAKE_HOME` / `MEDIALAKE_MODEL_DIR`. On macOS the same files live in
`~/Library/Application Support/MediaLake/`.
