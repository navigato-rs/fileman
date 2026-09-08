# Navigato support

Small shared Help/About UI and a content-free, local report store for Fileman and
Starcom. This package is independent of Sunset. It has no network transport.

Applications provide compile-time `Info`, initialize once outside replay/demo,
retain the `Session` through teardown, and call typed `failure`, `feature` or
`timer` APIs. Runtime content cannot be passed to those recorders. `show` embeds
the controls in an existing egui surface. See [the privacy contract](../PRIVACY.md).

`cargo test -p navigato-support` checks encoding, recipient injection, schema
allowlisting, bounds, permissions, symlinks, locking, panic payload exclusion and
consent changes. `cargo clippy -p navigato-support --all-targets -- -D warnings`
checks the package without enabling any telemetry transport.

Optional backtraces are separately consented, bounded executable-relative offsets.
The Sentry crates are used for image metadata only: no SDK client/transport is
initialized. `scripts/sentry_report.py` is a maintainer-only import tool, not an
application dependency. Private-email consent does not authorize Sentry import.
See `SENTRY.md` and `PRIVACY.md` at the repository root.
