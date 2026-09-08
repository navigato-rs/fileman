# Feedback and diagnostics

**No automatic uploads.** Feedback opens a browser draft. Private reporting opens
an email draft. Neither submits anything until you send it. GitHub issues are
public; email goes only to the displayed recipient and discloses your sender
address. A configured delivery alias is visible in the draft and compiled binary,
not an authentication secret. Check it before sending.

## Local reports

Release builds keep minimal failure reports locally by default, asking you to
review them before sharing. Usage collection is independently **off by default**.
Both controls are inside the existing Help/About screen, under **Feedback and
diagnostics**. Turning one off immediately clears its counters and local reports;
a write/delete failure is shown. Preferences persist between launches. Corrupt
preferences disable both categories. Debug, demo, replay and snapshot runs do not
record. When the state directory is unavailable or already owned by another app
instance, reporting is disabled for that instance.

Reports contain only a versioned schema, app/build, OS/architecture and either a
fixed failure category with an optional application-relative Rust source location,
or fixed feature flags and timing histograms. No installation/user identifier,
wall-clock timestamps or IP address is in a report. No paths, SSH aliases/addresses,
usernames, commands, text, clipboard data, screenshots, file contents, raw errors,
panic messages or log attachments are collected. Source locations are code paths
such as `src/main.rs`, never runtime file paths. Local report filenames contain a
time and process number for collision avoidance; those names are not exported.

Rust panic recording is best effort and includes caught/worker panics, not just
process crashes. It does not collect native crashes, minidumps or unclean-exit guesses. Fatal renderer/desktop failures use stable classifications.
The ordinary local panic/error logger is unchanged; it is never attached.

## Optional backtraces and Sentry

**Include application backtraces locally** is independently off by default, including
when upgrading older preferences. When enabled, failure reports add at most 48
executable-relative code offsets, its debug/build ID, image size and preferred load
address. They do not include actual ASLR load addresses, loaded-library names,
resolved compiler paths, thread names, variables, memory or panic payloads. Missing
executable metadata leaves a category-only report. Turning this option off clears
saved failure reports. Reports remain bounded to 8 KiB and are never sent by the app.

The report review has a separate, unchecked **Allow the maintainer to import this
report into private Sentry diagnostics** choice. That applies only to the copied or
emailed report. Private email alone does not grant this permission. The maintainer
importer refuses delivery without it and refuses usage reports. It sends one
validated event to the separately configured hosted Sentry project, with no retries,
redirects, source attachments or raw logs. Sentry sees the importer's network IP,
not an end-user IP recorded by the app. Project access and retention are controlled
by the maintainer's Sentry account; that account's policies also apply to imported
reports. The app's seven-day local expiry does not delete reports already imported.

## Optional statistics

The local summary contains feature flags in this order: remote, archive, editor,
search, jump route, upload. Each flag means an instrumented workflow was used or
attempted, not a click count or unique person. Timing histograms are ordered:
frame, local-directory worker, remote-directory worker, connection attempt.
Each row has seven millisecond buckets: <=1, <=5, <=16, <=50, <=200, <=1000, >1000.
Counters saturate instead of overflowing. A consent change invalidates in-flight
timers, including when collection is later re-enabled.

Fileman measures directory workers (not time until the UI becomes usable) and
redraw handling. Starcom measures renderer CPU duration (including submission
waits, not GPU execution time) and connection attempts through attachment. These
measurements are not directly comparable across applications. They are not active
use time, energy measurements, population counts, retention or crash rates.

A summary is saved at normal shutdown or when you press **Review usage summary**.
Inspect the exact serialized payload before copying or emailing it. Long reports
must be copied into the draft; they are never silently truncated to fit a URL.
There is no timer, heartbeat, retry loop, upload thread, network dependency or
blocking network flush on shutdown. Reporting may omit an observation under lock
contention rather than delay application work.

## Storage

Within your per-user state directory: `navigato/<app>/support`.
Linux uses absolute `XDG_STATE_HOME` or `~/.local/state`; macOS uses
`~/Library/Application Support`; Windows uses `LOCALAPPDATA`.
Only one process per app owns reporting at a time. The store keeps at most eight
reports, each at most 8 KiB. Reports older than seven days are pruned on launch and
writes, with no idle cleanup timer. Unix directories/files are private (0700/0600);
Windows uses the inherited per-user profile ACL. The store rejects symlink entries,
oversized/unknown schemas and invalid metadata. Delete a report from the UI or
remove the support directory while the app is closed. Previously sent email or
submitted issues are not deleted by clearing local reports.

## Release configuration

Set the organization/repository Actions secret `NAVIGATO_PRIVATE_REPORT_EMAIL` to
a dedicated single-recipient alias. Release builds embed it without storing its
value in source. Local packagers may supply the same environment variable while
compiling. An absent or invalid address disables the private-report link, never
falls back to an author's public email. Changing the alias requires a rebuild.
`GITHUB_SHA`, when supplied by CI, identifies the exact build.

## Symbol uploads

Official release workflows preserve line-level debug information for each build
variant and upload it when `SENTRY_AUTH_TOKEN`, `SENTRY_ORG` and `SENTRY_PROJECT` are
configured. Org/project values may be GitHub secrets or variables. Only release
symbols and the corresponding executable are uploaded, never a checkout-wide scan
or source bundle. The auth token is CI-only and is not embedded into the app.
`SENTRY_DSN` is used only by the explicit synthetic smoke workflow or a maintainer
import; it is not an application upload switch. See `SENTRY.md` for validation.

## Next delivery milestone

Automatic application uploads and product analytics delivery are **not enabled**. The current Sentry
transport/dependency proof does not satisfy Starcom's no-native-crypto policy.
Do not remove that policy or substitute an experimental TLS implementation merely
for telemetry. Before adding uploads: select and validate a compliant transport,
confirm the private backend and its retention/IP settings, add distinct automatic-upload
consent, enforce bounded delivery/retries, and verify symbolized packaged-release
reports. Local collection consent does not authorize a future automatic uploader.
