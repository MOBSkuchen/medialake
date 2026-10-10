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

## UI
Client UI uses the native toolkit of each platform: WinUI on Windows, SwiftUI/AppKit on macOS (planned, see `plan2.md`).
Both hosts load the same rust client; only the UI differs.
The app is opened by using a keyboard shortcut (defined during installation).

### Base app
It opens a window, which is centered in the middle of the current monitor. The user may type into it. 
Below, files will appear based on similarity to the text. A file can be clicked on and should also have a "Open in Explorer" option on the right ("Show in Finder" on macOS).

Very much like Spotlight on Mac - but only for files.

At the top left is a large, fat text "MediaLake" and at the right is a settings button.

### Settings menu
Contains 3 parts:
1. General: Only keyboard shortcut currently
2. Model: Options for local and also remote. Remote prompts for an input code. More on that later. Also add a dropdown for the model to use and a confidence threshold slider.
3. Index: Select (also recursively) and deselect directories to index. Also add 2 sliders for index timeout after modification and for modification size (in bytes)

## Installer
The installer program installs the client+server combo, as well as downloading the default model and prompting the user for a keyboard shortcut.
On Windows this is an Inno Setup installer; on macOS a `.pkg`, where the shortcut is asked on first start instead (a pkg cannot ask questions).