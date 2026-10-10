# SearchNow

Local, semantic search for your files: text, code, PDF/Office documents, images, audio and
(experimental) video are embedded **by content** with EmbeddingGemma 2 (llama.cpp) and found with natural-language
queries from a Spotlight-like window.

## Layout

| Path | What |
|---|---|
| `crates/searchnow-proto` | bierpc message types, model manifest, pairing-code codec, TLS pinning |
| `crates/searchnow-llama` | thin wrapper over raw llama.cpp / mtmd (pinned b11535 via `vendor/`) |
| `crates/searchnow-server` | `searchnow-server.exe`: embedding server (`--local` child, `serve`, `pair`, `clients`, `bench`) |
| `crates/searchnow-core` | client logic + `searchnow-cli` (diagnostics) |
| `crates/searchnow-ffi` | `searchnow_core_ffi.dll` C ABI for the UI |
| `ui/` | WinUI 3 app (search window, tray, settings) |
| `installer/` | Inno Setup |
| `scripts/` | `dev-env.sh`, `setup-vendor.ps1`, `build-release.ps1`, `make-icon.ps1` |

`bierpc` / `bier_derive` are path dependencies on the sibling repo `../rpc` (it needs the two uncommitted
fixes described in `poc/README.md`).

## Build

Prerequisites (per-user installs are fine): Rust, .NET 10 SDK, VS 2022 Build Tools (C++), CMake, libclang
(portable LLVM; `LIBCLANG_PATH`), optionally the Vulkan SDK and Inno Setup 6. `scripts/dev-env.sh` shows the
paths used on the development machine.

```powershell
scripts\setup-vendor.ps1                 # once: llama-cpp-rs + llama.cpp b11535 into vendor/
scripts\build-release.ps1 -Installer     # builds server (CPU + Vulkan), core DLL, UI; stages dist\app; builds the installer
```

Dev loop:

```bash
source scripts/dev-env.sh
cargo build --release                                   # server, cli, ffi dll
cargo test --release --workspace --lib
SEARCHNOW_MODEL_DIR=<dir with gguf> cargo test --release -p searchnow-core --test e2e -- --ignored --test-threads=1
cd ui/SearchNow.App && dotnet build -c Release -p:Platform=x64
```

## Remote server

```
searchnow-server --model-dir D:\models --quality heavy serve --bind 0.0.0.0:47900 --public-addr myserver.example.com:47900
searchnow-server pair            # prints a one-time pairing code (10 min)
searchnow-server clients list | revoke <id>
```

Paste the code in *Settings > Model > On a remote server*. The connection is TLS with the server's
self-signed certificate pinned by SHA-256 fingerprint (contained in the code); the client's long-term secret is
stored DPAPI-protected. File contents are sent to the remote server for embedding.

## Data locations

`%LOCALAPPDATA%\SearchNow\` : `config.toml`, `index.db` (+ `vectors.usearch`), `models\`, `logs\`
(`client.log`, `server.log`, `ui.log`). Override with `SEARCHNOW_HOME` / `SEARCHNOW_MODEL_DIR`.
