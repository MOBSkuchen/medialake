# MediaLake – Detailed Plan (v1)

Builds on `abstract.md`. This plan covers the Windows app; the macOS port is planned in `plan2.md`. Decisions made so far:

| Topic | Decision |
|---|---|
| UI | C# WinUI 3 host exe that loads the Rust client core as a DLL **in the same process** (the always-running app is one process) |
| Local server | `medialake-server.exe` runs as a **child process** of the client and is reached over bierpc on localhost, the same code path as remote mode |
| Content | EmbeddingGemma 2 is multimodal (text, image, audio, video in one vector space), so files are embedded by **content** |
| Model options | A model dropdown plus a quality setting **Heavy / Normal / Light** (quantisation). Embeddings always use the **full 768 dimensions** |
| Modification settings | Two sliders: **min change size to re-embed** and **max file size to index** (plus the debounce timeout) |
| Transport | bierpc as-is: rustls TLS for remote, plain TCP + token on localhost, persistent calls for large payloads |
| Remote pairing | One code containing: server address + one-time access token + shared secret + TLS cert fingerprint (pinned) |
| Video | **Experimental**, off by default; toggle in Settings → Model |
| Installer | Inno Setup |

---

## 1. Architecture

```
┌──────────────── MediaLake.exe (always running, tray) ────────────────┐
│                                                                       │
│  C# / WinUI 3 (UI only)          Rust: medialake_core.dll (cdylib)    │
│  ┌────────────────────┐  C ABI   ┌──────────────────────────────────┐ │
│  │ Search window      │ ───────► │ config · indexer · file watcher  │ │
│  │ Settings window    │ ◄─────── │ extractors · vector store        │ │
│  │ Tray icon, hotkey  │ callbacks│ search · server supervisor       │ │
│  └────────────────────┘          │ embedding client (bierpc)        │ │
│                                  └───────────────┬──────────────────┘ │
└──────────────────────────────────────────────────┼────────────────────┘
                       local: 127.0.0.1, plain TCP │ remote: TLS (rustls)
                       + per-launch token          │ + paired credentials
            ┌──────────────────────────────────────┴───────┐
            ▼                                              ▼
 medialake-server.exe (child process)          medialake-server.exe (other machine)
 llama.cpp + EmbeddingGemma 2                  llama.cpp + EmbeddingGemma 2
```

Key points:
- **The client owns all user data.** The index, file list and settings live on the client machine. The server only turns inputs (text, image bytes, audio, video frames) into vectors and keeps no state about files.
- **One embedding client.** Local and remote mode both use the same bierpc client (`ServerConnection`); they differ only in target, TLS and credentials.
- **The server supervisor** (in medialake-core) manages the local server process:
  1. Spawns `medialake-server.exe --local --port 0` with `CREATE_NO_WINDOW`, in a **Job Object** so the child dies when the client dies.
  2. Reads the chosen port from the child's stdout, and passes a random per-launch auth token via stdin.
  3. Health-checks the child and restarts it with backoff if it crashes. A llama.cpp or GPU-driver crash therefore never takes down the UI.
  4. Optionally stops the child after N idle minutes to free the model's RAM, and respawns it on demand. This is internal and not shown in the UI.
- **Changing model or quality means a full reindex.** The index stores the model id and quality, and refuses to mix them.

## 2. Repository Layout

```
medialake/
├─ Cargo.toml                    # workspace
├─ crates/
│  ├─ medialake-proto/           # shared bierpc message types (bier_derive), pairing-code codec
│  ├─ medialake-server/          # bin: embedding server (local child or remote), CLI incl. `pair`
│  ├─ medialake-core/            # client logic: config, index, watcher, extractors, store, search, supervisor
│  └─ medialake-ffi/             # cdylib "medialake_core.dll": C ABI over medialake-core
├─ ui/
│  └─ MediaLake.App/             # C# .NET 8, WinUI 3 (Windows App SDK), unpackaged, self-contained
├─ installer/
│  └─ medialake.iss              # Inno Setup script
└─ docs/
```

`bierpc` and `bier_derive` are **path dependencies on the local repo** `../rpc` (`C:\Users\jaspe\RustroverProjects\rpc`).

## 3. Components

### 3.1 medialake-server (embedding)
- **Runtime:** a small **`medialake-llama` crate of our own**: a thin safe wrapper over the **raw `llama-cpp-sys-2`** C API, with llama.cpp **pinned via `[patch]`** to a known-good tag (PoC: **b11535**). Spike 0a showed why:
  - The crates.io and git versions of the crate lag weeks behind llama.cpp; neither knows the `gemma-embedding2` architecture.
  - The high-level `llama-cpp-2` crate's `eval_chunks` decodes text and media as separate batches. Mean pooling then covers only the last batch, and **every image ends up with the same vector**.

  The working recipe (`poc/llama-rs/`):
  1. `mtmd_tokenize`, then `mtmd_encode_chunk` for media.
  2. **One `llama_batch_ext`** mixing text tokens and media embedding rows (positions 0..n).
  3. One `llama_process`, then `llama_get_embeddings_seq`.
  4. L2-normalise ourselves.

  Bumping llama.cpp is a deliberate step: update the tag, rerun `poc/llama-rs` as a regression test, and fix wrapper drift. Building needs CMake + libclang.
- **GPU:** the build uses the **Vulkan** backend, which is vendor-neutral, with CPU fallback via llama.cpp's dynamic backend loading. The PoC showed GPU is ~10–15× faster for media: an image takes ~70 ms on GPU vs ~1 s on CPU. Vulkan compiles shaders on first use (up to ~20 s), so the server runs a **warm-up embedding at start**. CPU fallback is built with `GGML_BACKEND_DL` + `GGML_CPU_ALL_VARIANTS` (per-CPU tuned backends, as in the official release). The generic build in spike 0a was ~2× slower on images.
- **Model:** EmbeddingGemma 2 as GGUF, with images, audio and video through `mtmd` and the projector (`mmproj`) file. The text model and the projector are separate files; loading the projector is what enables media.
- **Quality mapping:** **never F16.** The model card warns that EmbeddingGemma 2 gives NaN or degraded output in F16; BF16 and F32 are safe.

  | Quality | Text model | Projector | Download |
  |---|---|---|---|
  | Heavy | BF16 (ggml-org) | BF16 | ~1.5 GB |
  | Normal (default, PoC-tested) | Q8_0 (ggml-org) | Q8_0 | ~865 MB |
  | Light | UD-Q4_K_XL (unsloth) | Q8_0 | ~730 MB |

  Output is always the full 768-dim vector, L2-normalised by llama.cpp with mean pooling.
- **Media marker:** with libmtmd, use `mtmd_default_marker()` in the prompt. (llama-server randomises its marker per process.)
- **bierpc API** (`RpcServerHandler`, types in medialake-proto):
  - `Hello { proto_version, auth }`: returns `ServerInfo { model_id, quality, dims, modalities }`.
  - `EmbedText { auth, texts: Vec<String>, kind: Query | Document }`. Applies the model's task prefixes (confirmed by the model card and the PoC):
    - queries: `task: search result | query: {q}`
    - documents: `title: {file name} | text: {chunk}`
    - media: no prefix
  - `EmbedImage { auth, bytes, mime }`, `EmbedAudio { auth, samples, sample_rate }`, `EmbedVideo { auth, frames, audio }`. Small payloads go as single calls; large ones go through a **persistent call** whose session streams the payload over the raw connection (see §4).
  - `Pair { one_time_token }`: remote only, see §6.
- **Batching:** an internal queue batches requests so indexing throughput is good, and query embeddings get **priority** over indexing work so typing stays responsive.
- **CLI:**
  - `medialake-server --local --port 0` (child mode, binds 127.0.0.1 only)
  - `medialake-server serve --bind 0.0.0.0:PORT --model … --quality normal` (remote mode, TLS)
  - `medialake-server pair` prints a new pairing code
  - `medialake-server clients list|revoke <id>`

### 3.2 medialake-core (client logic)
- **Config:** TOML at `%LOCALAPPDATA%\MediaLake\config.toml`. Holds the hotkey, model + quality, local/remote mode (+ remote credentials, DPAPI-encrypted), threshold, indexed directories (path + recursive flag), the debounce timeout, min change size and max file size.
- **Indexed directories:** each entry is `{ path, recursive: bool }`. Exclusions are a default ignore list (`.git`, `node_modules`, `target`, hidden/system files, temp files) and can be edited later.
- **Watcher:** the `notify` crate (ReadDirectoryChangesW) on every indexed root.
  - Events are **debounced per file** using the "index timeout after modification" setting, so a file that is still being written gets re-embedded only once.
  - Renames and moves only update paths in the store and re-embed the cheap filename/path vector. The content is not re-embedded.
  - Deletes remove the file's vectors.
- **Re-embed rules:**
  - **Max file size:** files above this are indexed by **name/path only**.
  - **Min change size:** a modified file is re-embedded only if its estimated change is at least N bytes. The change is always measured against the state at the **last embed**, not the last scan:
    - Text-like files: the store keeps a hash per chunk from the last embed. Change = bytes in chunks whose hash differs + |size delta|. Only the changed chunks are re-embedded.
    - Media files: the store keeps a **decoded-content hash** (pixels or audio stream, without EXIF/ID3 metadata) next to the raw file hash.
      - If the content hash differs, change = full file size, so the file is always re-embedded. Replacing a photo with a different one of similar size is therefore never missed.
      - If only the metadata changed, only the filename/path vector is refreshed.
    - Last-seen `(size, mtime)` is stored separately from the last-embedded hashes. Reconciliation doesn't keep re-checking a file, and small edits **accumulate** naturally until they pass N, so the index can't drift away from the content indefinitely.
- **Startup reconciliation:** while the app is closed the watcher misses changes. On start, walk the roots and compare `(size, mtime)`, plus a content hash if those differ, against the store. Queue only the files that differ, applying the rules above.
- **Index queue:** a background worker pool, throttled, at low thread priority. It reports progress to the UI (n/total, current file).
- **Server connections:** bierpc runs **one call at a time per connection**, so the client keeps two connections: one for **queries** (low latency) and one for **indexing** (bulk). This way a search never waits behind a bulk indexing batch.
- **Extractors:** each file type maps to a modality:
  - Text and code (by extension and UTF-8 sniff) → text chunks of about 1–2k tokens with overlap. The model supports 8k context, but smaller chunks give more precise hits.
  - PDF (`pdf-extract`/pdfium) and Office files (docx/xlsx/pptx via zip + XML) → text chunks.
  - Images (png/jpg/webp/heic via `image`) → downscaled (e.g. ≤ 768 px) and re-encoded on the client, which keeps payloads small → one image embedding (~280 tokens).
  - Audio (via `symphonia`) → decoded to **mono 16 kHz** PCM, as the model requires → embedded in segments of ≤ 60 s (25 tokens/s). llama.cpp still labels its audio input "experimental", but the PoC results were correct.
  - Video (**experimental, off by default**): the **client extracts ≤ 8 frames** and sends them as images in one input (140 tokens/frame) so cost stays bounded. Frame extraction uses Windows Media Foundation, so nothing has to be bundled. llama.cpp's own video path needs an installed ffmpeg, and the Rust sys crate builds without it. PoC (llama-server, 4 s clip): ~1.1 s on GPU, ~15 s on CPU. While it's off, video files are indexed by name/path only. Turning it on queues all known video files for content embedding; turning it off removes their content vectors.
  - Every file also gets one **filename + path** text embedding, so files whose content can't be extracted are still findable by name.
- **Vector store:** SQLite (`rusqlite`) for metadata, plus a vector index:
  - Tables: `files(id, path, size, mtime, hash, modality, indexed_at)` and `chunks(id, file_id, kind, offset/segment, chunk_hash, vector_id)`.
  - Vectors live in an HNSW index (`usearch`) persisted next to the database. Vectors are **768-dim**, stored as f16. Storage-only quantisation of finished unit vectors is fine; the F16 warning applies to inference. That's about 1.5 KB per vector, so 1M chunks ≈ 1.5 GB.
  - A `meta` table records `model_id` and `quality`. If either changes, the client prompts for a reindex.
- **Search:**
  1. Embed the query as `Query`.
  2. Run an ANN top-k (k ≈ 200 chunks).
  3. Group the hits by file and score each file as its max chunk similarity, with a small bonus for filename matches.
  4. **Calibrate scores.** The PoC showed raw cosines are compressed: unrelated items score 0.50–0.66 and matches score 0.65–0.76. A raw threshold is therefore meaningless to users. Raw scores are mapped to a 0–1 confidence with a calibrated linear/sigmoid mapping (roughly 0.60 → 0 and 0.80 → 1, tuned on a larger test set). Cross-modal scores sat in the same range as text→text, so per-modality offsets start at 0 and are only added if a larger evaluation shows bias.
  5. Drop files below the **confidence threshold** (applied to the normalised score).
  6. Return the top N (about 20).

  Results carry the path, score, modality, best-matching snippet (text) or segment (audio/video), and mtime.

### 3.3 medialake-ffi (C ABI)
- A plain `extern "C"` surface; the C# `DllImport`s are generated by `csbindgen`.
- **Simple, coarse API.** Complex data crosses the boundary as UTF-8 JSON, so the ABI stays small and stable:
  - `sn_init(config_dir) -> handle`, `sn_shutdown(handle)`. Init starts the server child (local mode) or connects to the remote server.
  - `sn_search(handle, query, request_id) -> json`. Called from a C# background thread; a newer `request_id` cancels stale searches.
  - `sn_get_settings / sn_set_settings(json)`
  - `sn_add_dir(path, recursive) / sn_remove_dir(path)`
  - `sn_pair_remote(code) -> result json`, `sn_use_local()`
  - `sn_reindex_all()`
  - `sn_set_status_callback(fn ptr)`: indexing progress, server state (starting/ready/crashed/restarting), and errors, pushed to C#.
  - `sn_free_string(ptr)`
- Rust owns a tokio runtime inside the DLL. Panics are caught at the boundary (`catch_unwind`) and returned as errors.

### 3.4 MediaLake.App (C# WinUI 3)
- **Deployment:** unpackaged, with `WindowsAppSDKSelfContained=true` and a self-contained .NET publish. Inno Setup only copies files; no runtime installers are needed. Spike 0b: .NET 10 + Windows App SDK 2.5.1 build with the plain `dotnet` CLI. The output is **~228 MB**, so try trimming (and possibly ReadyToRun) in phase 9.
- **Lifetime:** starts at login (Run key set by the installer), lives in the **tray**, and keeps the core loaded so the index is warm. The window is hidden and shown, never recreated. It's single-instance: a second launch signals the running instance to show its window (named mutex + message).
- **Global hotkey:** `RegisterHotKey` via Win32 interop. Pressing it shows the window centred on the **monitor containing the cursor/foreground window** and focuses the search box. Esc or losing focus hides the window.
- **Search window** (borderless, rounded, Mica/Acrylic backdrop, Spotlight-like):
  - Top left: a large bold "MediaLake" title. Top right: a settings button (gear).
  - Search box: typing is debounced (~150 ms) and runs the search on a background thread.
  - Result list: shell icon or thumbnail, file name, shortened path, modality badge, and an optional snippet. On the right of each row is an **"Open in Explorer"** button, which runs `explorer.exe /select,"path"`.
  - Click or Enter opens the file with its default app (`ShellExecute`).
  - Keyboard: Up/Down to move, Enter to open, Ctrl+Enter to open in Explorer, Esc to hide.
  - A status line shows "Indexing 1,204 / 5,880…" or "Model starting…" when relevant.
- **Settings window** (NavigationView with 3 pages):
  1. **General:** a hotkey recorder control. It validates the shortcut by trying `RegisterHotKey`.
  2. **Model:**
     - Local/Remote toggle. Remote shows a text box for the pairing code and a "Connect" button with the connection status.
     - Model dropdown (EmbeddingGemma 2 for now).
     - Quality: Heavy / Normal / Light. Picking a quality that isn't downloaded yet downloads it. Changing it asks to confirm a reindex. In remote mode the quality is fixed by the server, so the control is shown read-only.
     - Confidence threshold slider (0–1, normalised confidence).
     - **Experimental** section: "Embed video content" toggle (off by default), with a note that it's slow and CPU-heavy.
  3. **Index:**
     - List of directories, each with a "recursive" checkbox, plus Add (folder picker) and Remove.
     - Slider: index timeout after modification (e.g. 0.5–60 s).
     - Slider: min change size to re-embed (bytes, log scale, e.g. 0 B–1 MB).
     - Slider: max file size to index (bytes, log scale, e.g. 1 MB–4 GB).
     - "Reindex all" button and index stats.

## 4. Protocol (medialake-proto, bierpc)

Based on the current code in `rpc/` (latest commits add rustls TLS and I/O timeouts):
- **Types:** `bier_derive` `Serialize`/`Deserialize` on shared `Action` / `Response` enums. `Hello` checks `proto_version`, and mismatches are rejected cleanly.
- **Auth:** the `RpcServerHandler::handle` gets no per-connection context, so **every action carries an `auth` field**.
  - Locally this is the per-launch token.
  - Remotely it's `client_id` + the shared secret, which is safe because the channel is TLS.
- **Large payloads:** the default `max_message_bytes` is 1 MB.
  - Single calls are raised to a sane limit (e.g. 16 MB) via `ClientConfig`/`ServerConfig`.
  - Video and large audio use `call_persistent`: the session writes length-prefixed chunks directly to the stream, and the server's `PersistentRpcServerHandler` reads them and returns the embeddings.
- **Timeouts:** connect/handshake/idle/request timeouts come from bierpc's config. The client reconnects when a connection is `poisoned`.
- **bierpc: `RpcServer::local_addr()`** ✅ added (uncommitted in `../rpc`). It reports the OS-assigned port when binding to port 0, which the supervisor needs for `--port 0`.
- **bier_derive fix** ✅ (uncommitted in `../rpc`): named fields of enum variants now generate `<#ty>::deserialize`, so generic field types such as `texts: Vec<String>` compile.
- **Not needed:** a bulk bytes fast path. A 2 MB `Vec<u8>` takes 22 ms in a single call, and large payloads stream at more than 100 MB/s via persistent calls (spike 0c).

## 5. Model Management
- Models are stored in `%LOCALAPPDATA%\MediaLake\models\` (GGUF per quality + mmproj).
- A built-in manifest lists `id, quality, url, sha256, size`.
- The installer downloads the default (Normal); other qualities are downloaded on demand from Settings, with progress and a SHA-256 check.

## 6. Remote Server & Pairing
- **TLS identity:** on first run, the server generates a self-signed certificate (`rcgen`) and stores it.
- **Generating a code:** `medialake-server pair` creates **one code** containing:
  - **address** (`host:port`; `--public-addr` overrides auto-detection)
  - **one-time access token** (16 bytes), valid until first use or a 10-min TTL
  - **shared secret** (16–32 bytes), which becomes this client's long-term credential
  - **cert fingerprint** (SHA-256 of the server's self-signed cert). The client pins it, so a fake server can't impersonate the real one.
  - a version byte and a checksum

  These are encoded as base32 in groups (`SN1-XXXX-XXXX-…`), about 100 characters, meant to be **pasted**. The codec lives in `medialake-proto`.
- **Using the code:**
  1. The user pastes it in Settings → Model → Remote.
  2. The client resolves the host itself (`tokio::net::lookup_host`), since `RpcClient` takes a `SocketAddr`. It then connects over TLS, pinning the fingerprint with a custom rustls verifier.
  3. It sends `Pair { one_time_token }`. The server burns the token and stores `client_id → secret`.
  4. Later calls authenticate with the secret.
- The client stores its credentials DPAPI-encrypted in its config.
- `medialake-server clients list|revoke` manages paired clients.

## 7. Installer (Inno Setup)
- Bundles the published `MediaLake.App` (self-contained, including `medialake_core.dll` and `medialake-server.exe`).
- **Custom wizard page:** a hotkey choice (text field with validation, defaulting to `Ctrl+Alt+Space`), written into `config.toml`.
- **Model download:** uses Inno's built-in download support (`CreateDownloadPage`) with progress. It downloads the Normal quality GGUF + projector and checks the SHA-256.
- **Options:** start-at-login (HKCU Run key), a Start-menu shortcut, and an uninstaller. On uninstall, ask whether to also delete the index and models.
- Per-user install (`%LOCALAPPDATA%\Programs\MediaLake`), so no admin rights are needed.

## 8. Phases / Milestones

| # | Phase | Deliverable / done when |
|---|---|---|
| 0 | **Spikes (risk first)** ✅ **Done** (`poc/README.md`) | (a) llama.cpp b11535 embeds text, image, audio and video correctly, both via llama-server and from Rust (raw sys API). (b) A C# WinUI 3 unpackaged app calls the Rust cdylib, including callbacks from Rust threads. (c) bierpc: typed calls, auth, a 64 MB persistent stream, parallel connections, TLS with a pinned cert. The calibration set (~50 items) moves to phase 2, where real indexing exists. |
| 1 | Workspace + server ✅ | `medialake-server` embeds text, images and audio over bierpc; benchmarks (ms per item per quality, CPU vs GPU) |
| 2 | Core: supervisor + store + indexer ✅ | Spawn and supervise the child server; index a directory (text + images), persist, reconcile on restart; CLI `search "query"` works end-to-end |
| 3 | Watcher + re-embed rules ✅ | Debounced live updates, min-change / max-size rules, rename/move/delete handling |
| 4 | FFI + search window ✅ | Hotkey → window on the current monitor → live results → open / open in Explorer; tray icon |
| 5 | Settings ✅ | All 3 pages wired to config; adding/removing dirs triggers (un)indexing; quality switch + download |
| 6 | Remaining modalities ✅ | PDF/Office, audio; video behind the experimental toggle |
| 7 | Remote mode ✅ | TLS serve mode, pairing code, cert pinning, credentials, settings UI |
| 8 | Installer ✅ | Inno Setup script, model download, hotkey page, autostart; clean-machine test |
| 9 | Polish ✅ | Thumbnails, snippets, performance, error states, logging (`tracing` → rolling file in both processes) |

## 9. Open Questions / Risks
1. **llama.cpp multimodal support:** ✅ verified by the PoC. Remaining risks:
   - The `llama-cpp-2` crate's mtmd bindings may lag behind llama.cpp. Fallback: our own bindgen over `mtmd.h`/`llama.h`.
   - Audio is still labelled experimental upstream.
   - CPU-only machines are slow for media (~1 s per image), so the initial index of a big photo library can take hours. Mitigations: low-priority background indexing with a visible progress/ETA.

   ONNX weights (`onnx-community/embeddinggemma-2-ONNX`) remain the fallback.
2. **Maintaining the llama.cpp pin:** we carry a `[patch]` of `llama-cpp-rs` (one stubbed wrapper function) until upstream catches up. Alternative: drop `llama-cpp-sys-2` and run bindgen ourselves over the pinned `llama.h`/`mtmd.h`. Decide in phase 1.
3. **Remote mode and privacy:** file contents are sent to the remote server for embedding. The channel is TLS-protected, but the UI should state this clearly.

## 10. Implementation status (phases 1-9) and deviations from the plan

All nine phases are implemented; see `README.md` for build/run instructions and `docs/calibration.md` for the score calibration.

| Area | What exists |
|---|---|
| `medialake-llama` | Safe wrapper over raw `llama-cpp-sys-2` (single-batch embedding of text / images / audio / multi-frame video) |
| `medialake-server` | `--local` child mode (token on stdin, `READY <port>`, exits when stdin closes), `serve` (TLS, pinned cert), `pair`, `clients list/revoke`, `bench`; query-priority job queue; warm-up |
| `medialake-core` | config, SQLite store + usearch HNSW (rebuilt from SQLite when stale), indexer (dedup queue, per-file serialisation, re-embed rules), `notify` watcher with debounce/rename handling, extractors (text CDC chunks, PDF, docx/pptx/xlsx/odt, images, audio via symphonia, video frames via Media Foundation), supervisor (Job Object, restart with backoff, idle stop, CPU fallback exe), model manager (resumable, SHA-256 verified), remote pairing + DPAPI credentials, search ranking |
| `medialake-ffi` | `sn_init / sn_call(method, json) / sn_set_event_callback / sn_shutdown` (+ csbindgen bindings) |
| `ui/MediaLake.App` | WinUI 3 search window (hotkey, tray, single instance, thumbnails, snippets), Settings (General / Model / Index) |
| `installer/medialake.iss` | Per-user Inno Setup: hotkey page, model download with SHA-256, autostart task, uninstall question |

Deviations and findings:

* **Vector index:** `usearch` is pinned to `=2.21.4`. Newer releases pull in `numkong`, which fails to link on MSVC (`__imp_nk_*` unresolved).
* **Persistent calls are not used for embedding payloads.** Everything (images, 60 s audio segments, 8 video frames) fits a single message with `max_message_bytes = 32 MB`; spike 0c showed single calls are fast enough.
* **Text chunking** is content-defined on line boundaries without overlap, so that an edit only changes nearby chunks. The *min change* rule measures the change at **line granularity** against the last embed (per-chunk line signatures are stored), which makes a byte threshold meaningful.
* **Video** uses Media Foundation (`IMFSourceReader`, 8 evenly spaced frames, RGB32 -> JPEG), as planned.
* **GPU:** Vulkan build via `--features vulkan` (needs the Vulkan SDK, the VS developer environment and a short `CARGO_TARGET_DIR`; `scripts/windows/build-release.ps1` does all of it). Measured on an RTX 3060 Ti vs CPU (normal quality): ~1000-token text 50 ms vs 1.7 s, image 60-90 ms vs 1.5-2.6 s, 4-frame video 0.28 s vs 8.3 s. The release ships `medialake-server.exe` (Vulkan) and `medialake-server-cpu.exe`; the supervisor falls back to the CPU build when the first cannot start (no Vulkan runtime).
* **Server selection** (local vs remote) is switchable at runtime: `Core::pair_remote` / `use_local`; the index is rebuilt when the server's quality differs from the stored one.
* **Calibration** (24 documents, 12 queries): `score_floor 0.58`, `score_ceiling 0.74`, default threshold `0.4`, plus a `relative_cut` of 0.06 raw cosine below the best hit.
* **Packaging:** `dotnet publish` omits the compiled XAML for unpackaged WinUI apps, so the release script stages the self-contained *build* output (~300 MB staged, 76 MB installer). Satellite resource folders other than en-US/de-DE are removed.
* **Not done / ideas:** code signing, ARM64, auto-update, model download resume UI after app restart, per-modality score offsets (data so far shows audio matches score ~0.07 lower than text), OCR for scanned PDFs.

* **Model download moved from the installer into the app** (first run): the search window offers Heavy/Normal/Light with size hints and a progress bar; the installer no longer needs network access.
