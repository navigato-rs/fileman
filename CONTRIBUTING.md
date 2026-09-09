# Contributing

## Workflow

Ensure `cargo fmt` is ran and `cargo clippy` is clean.

## Code Style Guide

- keep dependencies and amount of code low
- simple is good, don't overcomplicate or anticipate
- one `use` per crate, prefer importing modules instead of concrete types/functions
  - ok to include individual members if there is a few of them. As soon as it doesn't fit into a line - switch to using more complete paths, e.g. `fs::File` instead of `File`.
- don't rely on implicit references via `match`, always use explicit `ref` instead
- use enums instead of boolean arguments

## Repository Layout

- `src/main.rs` — app entry point, event loop, directory loading
- `src/archive.rs` — container plugins (zip, tar, tar.gz, tar.bz2)
- `src/core.rs` — shared types and utilities
- `src/app_state.rs` — application state
- `src/input.rs` — keyboard handling
- `src/ui/` — UI components (panel, preview, help)
- `src/image_decode.rs` — image decoding (including animated GIF)
- `src/ssh.rs` — SSH transport: blocking session driving sunset's sans-io runners
- `src/ssh/knownhosts.rs` — `known_hosts` parsing and host-key policy
- `src/ssh/agent.rs` — blocking ssh-agent client (Unix only)
- `src/sftp.rs` — remote file operations built on `src/ssh.rs`
- `src/replay.rs` — replay case data structures and assertion types
- `src/replay_runner.rs` — headless replay executor and assertion logic
- `themes/` — external theme files
- `etc/` — desktop entry, icon, reference snapshots
- `tests/cases/` — replay test cases (RON format)
- `tests/data/` — test fixture data
- `scripts/replay_runner.sh` — runs all replay cases with per-test cleanup
- `scripts/macos-app.sh` — local `FileMan.app` for `make install` (ad-hoc signed)
- `scripts/macos_package.sh` — signs, notarizes, and packages the macOS release

## Testing

Tests use a replay system that drives the application headlessly. Each test case
is a RON file in `tests/cases/` that specifies a starting directory, a sequence
of key events, and assertions to check after execution.

Run all replay cases:
```bash
scripts/replay_runner.sh
```

Run a single case:
```bash
cargo run --release -- --replay tests/cases/search.ron
```

Emit a screenshot while replaying:
```bash
cargo run --release -- --replay tests/cases/preview.ron --snapshot /tmp/replay.png
```

### Replay case format

```ron
(
  root: "tests/data/basic",       // starting directory (both panels)
  left: Some("path/to/left"),     // optional: override left panel root
  right: Some("path/to/right"),   // optional: override right panel root
  state_dump: Some("target/test-artifacts/dump.ron"),  // optional: write state to file
  keys: [
    (key: "Wait"),                // wait for async loading to finish
    (key: "Down"),                // bare key press
    (key: "F7", modifiers: ["Alt"]),  // key with modifiers
    (key: "text:hello"),          // inject text input
    (key: "select:source.txt"),   // move cursor to named entry
    (key: "replace:new_name"),    // set inline rename text
    (key: "wait:500"),            // wait for a fixed duration (ms)
  ],
  asserts: (
    // all assertion fields are optional
  ),
)
```

### Assertion types

There are three kinds of assertions, and they can be combined in a single test.

#### 1. Filesystem checks

Verify that files and directories exist (or match exactly) on disk after the
replay. Useful for testing operations like copy, move, delete, and mkdir.

```ron
asserts: (
  // Check directory tree on disk
  fs: Some((
    mode: Exact,    // or Contains
    entries: [
      (path: "out", kind: Dir),
      (path: "source.txt", kind: File),
    ],
  )),
  // Check file contents
  files: [
    (path: "out/copy.txt", contains: Some("expected text")),
    (path: "out/exact.txt", equals: Some("full content")),
  ],
),
```

`mode: Exact` fails if there are any entries on disk not listed in the assertion.
`mode: Contains` only checks that the listed entries are present.

#### 2. Screenshot comparison

Render the UI to a PNG and compare it against a reference image with configurable
tolerance. Useful for visual regression testing of the UI layout.

```ron
asserts: (
  snapshots: [
    (
      path: "target/test-artifacts/preview.png",
      expected: "etc/snapshots/tests/preview.png",
      max_channel_diff: 200,      // per-channel tolerance (0–255)
      max_pixel_fraction: 0.003,  // fraction of pixels allowed to differ
    ),
  ],
),
```

To update reference images after intentional UI changes:
```bash
cp target/test-artifacts/*.png etc/snapshots/tests/
```

#### 3. Panel state checks

Inspect the internal state of each panel: entry list, selected entry, mode, and
marked set. This is the fastest way to test navigation, search, selection, and
mode transitions without relying on pixel output.

```ron
asserts: (
  left_panel: Some((
    mode: Exact,                        // or Contains; applies to entries list
    entries: ["..", "out", "file.txt"],  // expected entry names
    selected: Some("file.txt"),         // cursor position
    browser_mode: Some("Fs"),           // Fs, Container, or Search
    panel_mode: Some("Browser"),        // Browser, Preview, Edit, or Help
    marked: ["out", "file.txt"],        // multi-selected entries
  )),
  right_panel: Some(( /* same fields */ )),
),
```

All fields inside a panel assert are optional — omit any you don't care about.

### State dumps

For debugging or developing new tests, request a full state dump:

```ron
(
  root: "tests/data/basic",
  state_dump: Some("target/test-artifacts/debug-state.ron"),
  keys: [ (key: "Wait") ],
  asserts: (),
)
```

This writes a RON file with both panels' entries, cursor positions, modes, and
sort settings. Inspect it to determine the right assertion values for a new test.

## SSH

Remote browsing runs on [sunset](https://github.com/navigato-rs/sunset), a
pure-Rust SSH implementation. Both layers used here are sans-io: `sunset::Runner`
is the SSH protocol and `SftpRunner` is SFTP on top of it, and neither does IO
of its own, so `src/ssh.rs` drives them straight from a blocking socket. There
is no executor and no background thread — an `ssh::Conn` is a `Mutex` around
the session, and whichever worker thread calls in does the pumping itself.
That is why nothing in the dependency tree is async.

Four properties of the runners shape that loop, and every one of them causes a
hang rather than an error when ignored:

- An event borrows the runner, so anything needing the runner again has to wait
  until that borrow ends.
- The SSH runner stops accepting socket input while a payload is waiting to be
  collected, so `Session::feed` returns instead of looping: only the caller can
  drain a channel, and spinning there would never let it.
- Channel data has to be moved out by hand. Nothing else will do it, and the
  peer stops sending once the window it gave us goes unacknowledged.
- `SftpRunner::want_buf` asks for exactly the bytes it wants next, so it is
  filled from the channel rather than read past.

Reads and writes are sized by `MAX_READ_LEN` and `MAX_WRITE_LEN`. The runner
refuses anything longer, since a bigger SFTP packet than servers are required
to accept gets the channel closed rather than answered.

A command reading its input to end-of-file needs to be told when that is, which
`ExecStream::finish_input` does via sunset's `send_eof`. Without it `tar xf -`
waits forever, so a streamed upload that hangs is usually a missing
`finish_input`.

A command's exit status arrives as a channel request around the channel's EOF,
so `Exec::done` waits for the close as well: stopping at EOF alone races the
status and usually misses it. `check_exec` then judges by that status, and
falls back to stderr only when a server sends none — going by stderr alone
fails a copy over any warning the command prints while still succeeding.

A command's output is held to a high-water mark and the rest left on the
channel, so the peer's window throttles it. Discarding the excess instead is
what a fixed cap did, and it corrupted any directory copy whose tar ran past
it. A capture that nothing drains until the command ends is still bounded, but
fails rather than returning a short answer.

Authentication offers ssh-agent keys first and then key files. `sunset::agent`
is the agent protocol and `src/ssh/agent.rs` is the transport under it — a Unix
socket, or the OpenSSH named pipe on Windows, which is the only part that
differs.

A connection dropped by a sleep or a network change is only discovered when
something is next asked of it. `Conn::with_retry` dials again and repeats the
request, so a listing after a drop simply works instead of erroring and needing
a reconnect and a renavigation. Only read-only requests take that path: a fatal
error means no reply was seen, so repeating a mutation could report "already
exists" for work that in fact succeeded. `Conn::with` is the plain version for
everything else, including anything holding a `HandleId`, which dies with the
session it came from.

Integration tests run against a real sshd on localhost; see Testing above and
the `sftp` job in CI.

## Releasing

Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds every
platform and uploads the artifacts to the GitHub release.

### macOS code signing and notarization

macOS refuses to open a downloaded app unless it is signed with a Developer ID
certificate *and* notarized by Apple. Without that, users get "FileMan is
damaged and can't be opened" or "Apple could not verify FileMan" and have to
dig through System Settings to launch it. Notarizing removes that step
entirely: the app opens on first double-click.

`scripts/macos_package.sh` performs the whole flow — sign, submit to the notary
service, staple the ticket into both the `.app` and the `.dmg`. It is driven by
repository secrets, and falls back to an ad-hoc signature (producing usable but
Gatekeeper-blocked artifacts) when they are absent, so forks still build.

Setting this up is a one-time job and needs a paid Apple Developer Program
membership (99 USD/year); an individual account is enough.

**1. Create a Developer ID Application certificate.** In Xcode: Settings →
Accounts → Manage Certificates → + → Developer ID Application. Then export it
from Keychain Access (My Certificates → right-click → Export) as a `.p12` with
a password. Note that this is *not* the "Apple Development" certificate — only
Developer ID certificates can sign software distributed outside the App Store.

**2. Create an App Store Connect API key.** App Store Connect → Users and
Access → Integrations → App Store Connect API → Team Keys → Generate API Key.
The "Developer" role is sufficient for notarization. Download the
`AuthKey_XXXXXXXX.p8` — Apple only offers the download once — and note the Key
ID and the Issuer ID shown on the same page.

**3. Add the repository secrets** under Settings → Secrets and variables →
Actions:

| Secret | Value |
|--------|-------|
| `MACOS_CERTIFICATE` | `base64 -i certificate.p12 \| pbcopy` |
| `MACOS_CERTIFICATE_PWD` | the password used when exporting the `.p12` |
| `APPLE_API_KEY` | `base64 -i AuthKey_XXXXXXXX.p8 \| pbcopy` |
| `APPLE_API_KEY_ID` | the Key ID, e.g. `XXXXXXXXXX` |
| `APPLE_API_ISSUER_ID` | the Issuer UUID |

`MACOS_SIGN_IDENTITY` is optional: the script picks the Developer ID identity
out of the imported certificate on its own, and only needs an explicit value if
the certificate happens to carry more than one.

An Apple ID works instead of an API key — set `APPLE_ID`, `APPLE_APP_PASSWORD`
(an [app-specific password](https://support.apple.com/en-us/102654), not the
account password), and `APPLE_TEAM_ID`. API keys are preferred: they are
scoped, revocable, and unaffected by the account's 2FA.

### Testing the packaging locally

On a Mac with the certificate in the login keychain, build the bundle and run
the script with the notary credentials in the environment:

```bash
cargo bundle --release --target aarch64-apple-darwin --features self-update
MACOS_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  APPLE_API_KEY_ID=... APPLE_API_ISSUER_ID=... APPLE_API_KEY="$(base64 -i AuthKey.p8)" \
  scripts/macos_package.sh
```

Leaving all of them unset exercises the ad-hoc path, which is enough to check
that the artifacts are laid out correctly. Notarization typically takes a few
minutes per submission; the script submits twice, once for the zip and once for
the dmg.

Note that the in-app self-update replaces the executable inside an installed
`FileMan.app` and re-signs it ad-hoc, which drops the Developer ID signature and
invalidates the stapled ticket. That is fine in practice — the installed app is
no longer quarantined, so Gatekeeper does not re-check it — but a user who wants
a fully notarized copy should reinstall from the dmg.
