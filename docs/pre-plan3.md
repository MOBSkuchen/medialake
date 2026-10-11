# File sync
MediaLake is now no longer a file searching tool, but also a real-time collaboration, synchronization and backup platform.

## Design (abstract)
MediaLake listens for updates on its indexed files - as usual - but now also shares files to its authenticated clients and receives updates from them.

A client comes to a MediaLake server and reports its state of the file system, the server then responds with (essentially) a diff of that.
This diff includes only what files changed and a bit of MetaData (new size && mod time), the client will then ask for those files seperately.

When a client wants to update the remote state, it asks for the servers state, returns a diff (contrasted by its state) and if that is accepted by the server, 
it sends the server the new files.

Every MediaLake instance must therefore keep a fingerprint of its copy of files.

An implementation of a similar system can be found at https://github.com/MOBSkuchen/uplink.

### Merge conflicts
As a server may have multiple clients connected to it, it can happen that the state of a file on the server can vary from the client's state.
We want to give the user a selection for this behavior:
1. use server as ground truth
2. prompt user

### Backups
Allow the user to choose a backup strategy:
1. Time-based ⇒ create a backup of everything *every once in a while* and keep the last *N* copies of that
2. File-based ⇒ keep the last *N* edits of every file
3. None

## Frontend Implementation
The user facing design is platform specific.

### Windows
On Windows, we use the `Windows Cloud Files` api, which offers the best UX, as it has synchronization in mind.
Custom bindings are probably needed to make this work, tho and the documentation on it is thin to say the least.

### MacOS
macOS has a very similar Api called `File Provider`, but that system is not usable in the scope of this project, as it needs an Apple Developer Account.
Instead, we are basically forced to use a FUSE variation, like MacFuse. This also means, that we need to bundle MacFuse (or some other variation) into the installer.

## Notes
- File Sync is untethered from the SearchNow functionality.
  - Account for the possibility, that a user might only want to use one of the systems.
- Allow for hosting / using multiple distinct syncs
- Make sure to account for cross-platform capability (Windows+macOS) wherever possible!