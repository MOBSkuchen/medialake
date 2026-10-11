# MediaLake
## Abstract
Local file, data, media search based on embeddings.
## Architecture
- Client program in rust (medialake-client)
  - manages user data
  - handles searches
  - does embbeding (by asking the server) - uses filewatcher to watch for modifications

  
- Server program also in rust (medialake-server)
  - does the actual embedding
  - communicates with client via [bierpc](https://github.com/MOBSkuchen/bierpc)
  - owns the model by using llama.cpp


- The client packages the server in itself, but can also use a remote server


- Currently only supports EmbeddingGemma2 as a model

### File sync (`medialake-sync`, see `post-plan3.md`)
- Optional and independent of search: either can be switched off.
- Hub and spoke. A *host* (`medialake-server serve`, or the app) owns a plain folder and numbers every change;
  *members* mirror it and push their own changes. Same listener, certificate and pairing as embedding; pairing
  codes carry per-sync read-write / read-only grants.
- Conflicts never discard a version; backups (time snapshots or last N versions, content-addressed) live on the host.
- The engine is platform-free; a frontend only shows the result: a plain mirror folder (everywhere), or on macOS a
  mounted volume whose files load when opened (an NFSv3 server inside the app, mounted by the system; nothing to
  install). Windows Cloud Files is the planned equivalent.
- Searching a synced folder never downloads it: files that are not on this device are indexed by name and get their
  content embedding once something else has fetched them.

## UI
Client UI uses the native toolkit of each platform: WinUI on Windows, SwiftUI/AppKit on macOS (see `plan2.md`).
Both hosts load the same rust client; only the UI differs.
The app is opened from its menu bar / notification area icon, or by a keyboard shortcut if the user chose one
(optional: nobody has to). The shortcut is chosen in the welcome window, not by the installer.

### Base app
It opens a window, which is centered in the middle of the current monitor. The user may type into it. 
Below, files will appear based on similarity to the text. A file can be clicked on and should also have a "Open in Explorer" option on the right ("Show in Finder" on macOS).

Very much like Spotlight on Mac - but only for files.

At the top left is a large, fat text "MediaLake" and at the right is a settings button.

### Settings menu
Contains 4 parts:
1. General: keyboard shortcut, start at login, search on/off
2. Model: Options for local and also remote. Remote prompts for an input code. More on that later. Also add a dropdown for the model to use and a confidence threshold slider.
3. Index: Select (also recursively) and deselect directories to index. Also add 2 sliders for index timeout after modification and for modification size (in bytes)
4. Sync: joined folders (state, pause, conflict policy, "keep on this Mac" browser, history, leave), conflicts, sharing
   folders from this computer (invite a device, per-device access), bandwidth limits (each with an on/off switch).

## Installer
The installer program installs the client+server combo and nothing else; it asks no questions (a pkg cannot, and the
two platforms should behave alike). On Windows this is an Inno Setup installer; on macOS a `.pkg`.

### Welcome window
The app's first start (a fresh install: no `config.toml`, so `welcome_done = false`) opens "Welcome to MediaLake":
1. **Search**: model and quality, an optional keyboard shortcut, start at login. **Download** saves the choices,
   registers the shortcut and downloads the model in the background (the window moves on at once); **Skip for now**
   sets up no shortcut and downloads no model and leaves search off.
2. **Sync**: join a folder another computer shares (paste an invitation code), share a folder from this computer
   (macOS), or skip. *Back* returns to page 1.

Closing the window counts as done. Until search is set up the search window says so in its bottom-left corner
("Search is not set up yet. Set up search…") and the link opens the welcome window on page 1. The state comes from the
core (`status.search`: `ready`, `no_model`, `off`), so both platforms show the same thing.