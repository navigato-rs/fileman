# Crash diagnostics

Sentry account setup alone does not capture or deliver application crashes.
This increment provides optional local backtraces, reviewed maintainer import,
and debug-file uploads. It adds no HTTP/TLS client to Fileman or Starcom.

## Configuration

`SENTRY_AUTH_TOKEN` stays in Actions secrets. `SENTRY_ORG` and `SENTRY_PROJECT`
may be secrets or variables; secrets take precedence. Scope project/DSN values
per repository when the apps use separate Sentry projects. Releases skip Sentry
only when all three symbol settings are absent; incomplete configuration fails.
`SENTRY_DSN` is a project ingestion destination, not a read/admin credential.
Never embed the auth token or pass it to application builds.

No automatic tracing, logs, profiling, session recording or usage upload is enabled.
Keep project access private and configure retention/IP handling in Sentry before
importing user reports. Clearing local reports does not retract prior imports.

## Validate after merging

Run **Crash diagnostics** manually on Fileman's `main`. Normal PR/main runs perform
only local checks. The manual main-branch run additionally uploads each platform's
probe symbols and sends one synthetic event using the configured credentials.
The step summary records event IDs. Open each event in Sentry and confirm
`crash_probe::synthetic_panic` resolves to `crash_probe.rs` with a line number.
A 2xx ingestion response alone is not proof of symbolication.

Linux checks source-line resolution against the extracted debug file; macOS checks
the dSYM locally. Windows validates address capture and the presence of the PDB;
server-side PDB symbolication remains part of the manual Sentry acceptance test.
These are optimized/stripped probe binaries, not a claim that packaged application
crashes on every OS have already been exercised.

## User reports

In Help/About, enable **Include application backtraces locally** before reproducing
a failure. Review the report and explicitly authorize Sentry processing before
copying/emailing it. Default or older category-only reports contain no backtrace.

For a report whose sender authorized Sentry processing, on the maintainer's machine:

```sh
# Validate and inspect the exact reconstructed event; no network access.
python3 scripts/sentry_report.py report.json --app fileman
# Send once to the destination in SENTRY_DSN, after review.
python3 scripts/sentry_report.py report.json --app fileman --send
```

Use `--app starcom` for Starcom. The importer rejects unknown/content fields, usage
reports, missing permission, wrong app, oversized input and non-Sentry destinations.
There are no automatic retries: uncertain delivery must not quietly duplicate a
report. Private reports must never be pasted into public Actions inputs/artifacts
or the organization issue-triage workflow. Do not attach raw logs or memory dumps.

## Release symbols

The release workflow overrides Cargo's debug/strip settings only for official
builds; ordinary `cargo install --release` retains its previous size defaults.
Linux extracts `.debug` before stripping and packaging. macOS retains a dSYM and
strips/copies the executable into its bundle before signing. Windows retains the
PDB. Each Fileman feature/backend variant is handled before a later build can
replace its executable. Matching is by binary debug ID, not just the Git SHA.

The shared action uploads only its dedicated symbol directory, waits for debug-file
processing, and fails the release on a configured upload failure. It uses a
hash-checked Sentry CLI 3.7.0. No paid live upload is performed by a PR check.

## Remaining coverage

Rust panic capture is best effort and includes caught/worker panics. Fatal-error
stacks show the reporting call path, not a recovered GPU-driver crash. Native
signals, access violations, process aborts and OOM kills are not captured. A
compatible, reviewed application HTTPS transport and a separate automatic-upload
consent contract are still required before in-app delivery can be enabled.
