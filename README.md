# MediaLake

Local, semantic search for your files: text, code, PDF/Office documents, images, audio and
(experimental) video are embedded **by content** with EmbeddingGemma 2 (llama.cpp) and found with natural-language
queries from a Spotlight-like window.

Windows is the supported platform. A macOS port is planned in `docs/plan2.md`; the macOS parts below are marked.

## Layout

| Path | What |
|---|---|
| `crates/medialake-proto` | bierpc message types, model manifest, pairing-code codec, TLS pinning |
| `crates/medialake-llama` | thin wrapper over raw llama.cpp / mtmd (pinned b11535 via `vendor/`) |
| `crates/medialake-server` | `medialake-server`: embedding server (`--local` child, `serve`, `pair`, `clients`, `bench`) |
| `crates/medialake-sync` | file sync: host (watched folder, history), member engine, mirror frontend, `medialake-sync` CLI |
| `crates/medialake-core` | client logic + `medialake-cli` (diagnostics) |
| `crates/medialake-ffi` | C ABI for the UI (`medialake_core_ffi.dll`; a static library on macOS) |
| `ui/MediaLake.App` | Windows: WinUI 3 app (search window, tray, settings) |
| `ui/MediaLake.Mac` | macOS: SwiftUI + AppKit app (planned) |
| `installer/` | `medialake.iss` (Inno Setup); `macos/` pkg sources (planned) |
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

## Build (macOS, planned)

See `docs/plan2.md`. Prerequisites: Rust, Xcode, CMake (`brew install cmake`), and the sibling `../bierpc` checkout.
None of the macOS scripts has been run on a Mac yet. `setup-vendor.sh` is a direct port of the Windows one;
`build-release.sh` and `make-icon.sh` are written for the planned layout and need `ui/MediaLake.Mac` and
`installer/macos` first.

```bash
scripts/macos/setup-vendor.sh                # once: llama-cpp-rs + llama.cpp b11535 into vendor/
scripts/macos/build-release.sh --installer   # builds server (Metal), core library, UI; stages dist/app/MediaLake.app; builds the .pkg
```

## Remote server

```
medialake-server --model-dir D:\models --quality heavy serve --bind 0.0.0.0:47900 --public-addr myserver.example.com:47900
medialake-server pair            # prints a one-time pairing code (10 min)
medialake-server clients list | revoke <id>
```

Paste the code in *Settings > Model > On a remote server*. The connection is TLS with the server's
self-signed certificate pinned by SHA-256 fingerprint (contained in the code); the client's long-term secret is
stored DPAPI-protected (macOS: in the Keychain). File contents are sent to the remote server for embedding.

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

Conflicts keep both versions (*server wins*: the local one becomes `name (conflict, <device>, <date>).ext`
and is shared; *ask*: it waits for `resolve … mine|server|both`). History lives on the host
(`medialake-server sync versions|snapshots|restore`). Sync is off by default in the app (`[sync] enabled`),
and `search_enabled = false` turns the app into a pure sync client / host.

Tests that need no model: `cargo test -p medialake-proto -p medialake-sync`; soak tests with real
transfers are `--ignored` in `crates/medialake-sync/tests/network.rs`.

## Data locations

`%LOCALAPPDATA%\MediaLake\` : `config.toml`, `index.db` (+ `vectors.usearch`), `models\`, `logs\`
(`client.log`, `server.log`, `ui.log`). Override with `MEDIALAKE_HOME` / `MEDIALAKE_MODEL_DIR`. On macOS the same files live in
`~/Library/Application Support/MediaLake/`.
