# Changelog

## v0.4.0 (5 Sep 2026)

### Features
- Pure-Rust SSH stack, replacing libssh2 — no OpenSSL, no async runtime
- ssh-agent authentication on Windows
- Dropped SSH connections re-establish themselves, keeping the current listing on screen
- Right-click context menu: Open, Open with default app, Reveal in file manager, Properties, Copy path, Rename, Move to Trash
- Reveal a file in the desktop file manager (Ctrl+Shift+O)
- Draggable divider between the panels; double-click to recenter
- Extend the selection with Shift+Up / Shift+Down
- Metadata preview for video files (duration and resolution)
- Copy files out of an archive onto a remote host
- macOS builds are signed and notarized, so they open without a Gatekeeper warning

### Fixes
- Refreshing a remote folder keeps its contents on screen instead of blanking, and navigating back after a reconnect shows fresh data
- Preview files inside archives on remote hosts
- The cursor stays put after deleting an item, including on remotes
- Copying onto an existing local file warns before overwriting
- The activity spinner reflects real work — no more phantom spinning after browsing an archive
- The progress dialog names the operation (Copying, Moving, …)
- Text viewer no longer wraps long lines by default; the help screen scrolls to fit
- Remote errors are shown in plain words, not wire codes
- Wheel scrolling settles instead of bouncing back to the selection
- Dialogs are no longer dimmed by their own overlay
- Remote directory copies past 64 MB no longer corrupt
- Remote copies no longer fail when the command only writes a warning

## v0.3.4 (30 Aug 2026)

### Features
- In-app settings
- Breadcrumb path and per-panel status line
- Removable drives in Go To
- Stream remote media to the system player
- SSH host-key verification

### Fixes
- GPU init on Windows
- Remote copy, SFTP disconnects, and large zip listing
- Image preview cache and oversized-decode OOM
- File operations no longer clobber existing files

## v0.3.3 (26 Apr 2026)

### Features
- Improved "Go To" menu with grouped remote and local destinations
- Automatic whitespace display on selection only
- Editor mode now covers the whole window
- Archive navigation on remotes

### Fixes
- Newlines in editor mode
- Leak in egui rendering

## v0.3.2 (14 Apr 2026)

### Features
- On access denied, offer to retry with elevated privileges
- Last-modified tab
- Prompt to elevate permissions

### Fixes
- Better IO and remote error handling
- Update to egui-0.34

## 0.3.1 (4 Apr 2026)

### Features
- Remote search (Alt+F7 on a remote panel) using `find` / `grep` over SSH; results stream back and behave like a normal search folder
- Space key computes folder size on remote panels (via `du` over SSH)

### Fixes
- Fix `du` byte count: `exit_status()` was read before `wait_close()`, making the result unreliable
- Refresh stale cached directories automatically
- Better surface IO errors
- Fix quick access on Windows

## 0.3.0 (25 Mar 2026)

### Features
- Remote browsing via built-in SFTP
- New syntax: Makefile, Dockerfile, CSV, SVG
- Defaulting to Ctrl+? schema on Apple devices
- Quick access menu with Ctrl+G
- Cache contents of parent folders

### Fixes
- Fix archive navigation
- Avoid terminal in Windows

## 0.2.0 (19 Mar 2026)

### Features
- Support GLES for older systems (separate build)
- Tab support (same keys as browsers)
- Self-update: `fileman --update` checks GitHub releases and replaces the binary in-place (compile feature `self-update`, enabled for tarball/zip/AppImage/MSI, disabled for deb/rpm)
- Introspection about async tasks displayed in F1 help screen
- Multi-stage JPEG loading for instant views
- New image formats: TGA, HDR, and DDS
- New syntax: RON

## 0.1.0 (13 Mar 2026)

Initial release.

### Features
- Two-panel file manager with keyboard-driven navigation
- Archive browsing: zip, tar, tar.gz, tar.bz2 (read-only, inline navigation)
- File operations: copy (F5), move (F6), rename (Shift+F6), delete (F8), mkdir (F7), new file (Shift+F4)
- Multiple selection with Insert key
- Integrated text editor (F4) with syntax highlighting
- File preview (F3): text, hex, images (JPEG, PNG, BMP, GIF, WebP), EXIF metadata
- Animated GIF playback in preview
- Search by name or content (Alt+F7, Shift+Alt+F7) with wildcard support
- File properties dialog (Alt+Enter) on Unix
- Open files with default system application (Shift+Enter)
- Symlink display with target paths
- Directory size calculation (Space)
- Navigation history (Alt+Left, Alt+Right)
- Configurable sort by name, size, or date
- Dark/light themes with external theme support (F9, F10)
- Help screen (F1)
- Replay-based testing framework with snapshot assertions
