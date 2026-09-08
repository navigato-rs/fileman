# Rust-only HTTPS evaluation

Experimental shared client for Fileman and Starcom. Neither application calls it
in normal builds. `tls-evaluation` only exposes a read-only probe example; it does
not enable reporting or updates. The upstream RustCrypto provider explicitly
warns against production use. Passing these tests is not a cryptographic audit.

```sh
cargo test --locked -p navigato-http
cargo run --locked -p navigato-http --example probe -- https://api.github.com/
# Check the application dependency graph as well:
cargo check --locked --features tls-evaluation --example tls-probe
python scripts/check-dependencies.py --package navigato-http
```

The probe prints only HTTP status and byte count. Non-2xx status still proves an
HTTP response was received over authenticated TLS, not application success. It
never reads Sentry credentials, report storage, SSH configuration or user content.

## Selected stack

Ureq 3.3.0 (`rustls-no-provider`) and Rustls 0.23.44, with RustCrypto's provider
pinned to `70f76c039e587192688af18a80d5d6435dedaf22`. No process-global provider,
Ring, AWS-LC, OpenSSL, native-tls or Graviola. Keep the pin because Ureq explicitly
does not give its custom-provider API a semver compatibility guarantee.

TLS 1.3 only: AES-128-GCM, ChaCha20-Poly1305 and AES-256-GCM; X25519 and P-256
key exchange. RSA and ECDSA server authentication are tested. No TLS 1.2, QUIC,
0-RTT, client certificates or client private-key loading. Server private keys in
loopback tests are fixture-only and unreachable through the client API.

The small adapter addresses two gaps in the pinned provider: all-zero X25519
secrets are rejected using Dalek's contributory check, and AES-GCM suites have
Rustls's documented 2^24-record confidentiality bound. Key exchange uses fallible
OS randomness instead of upstream's `UnwrapErr`. Rustls still owns certificate
validation, record processing and key updates; no primitives or TLS state machine
are forked. These guards need upstream review before release adoption.

## Trust and request bounds

OS certificate stores are read by `rustls-native-certs`; Rustls/WebPKI performs
the verification. OS socket, entropy and certificate-store APIs are allowed;
outsourcing TLS/crypto implementation is not. Root-loading errors fail closed.
Explicit DER roots replace, rather than extend, the OS store. This does not
reproduce all native trust policy, revocation or enterprise verifier behavior.

Requests require HTTPS, reject URL credentials/fragments/control characters,
never follow redirects and do not read proxy configuration. There are no
cookies, credentials, idle pooled sockets, automatic retries or periodic workers.
A POST is limited to 64 KiB. Headers are limited to 16 KiB; responses default to
1 MiB (maximum 8 MiB). Exact limits are accepted; oversized/truncated bodies fail.
All requests have a finite deadline (15 seconds by default, at most 60).

Call from a worker. Cancellation, proxy policy, reporting consent and retry UX
remain application integration work. OS DNS can outlive the caller's timeout in
Ureq's resolver thread; no claim is made that an OS call can be interrupted.
Do not collect Ureq/Rustls debug or wire logs into diagnostics.

## Acceptance gates

CI tests the client with C/C++ compilers disabled on Linux, macOS and Windows.
Tests cover three ciphers, both groups (including HelloRetryRequest), RSA/ECDSA
chains, bad hostnames, expired/future certificates, wrong EKU, invalid signatures,
non-CA intermediates, missing chains, wrong roots, low-order shares, altered
records, binary POSTs, redirects, bounded headers/bodies and stalled handshakes.
It inspects the effective target graph, not dormant Cargo.lock entries.

Before production: upstream adapter review, fuzz/differential testing against an
independent TLS implementation, actual ingestion-host interoperability, proxy
and cancellation behavior, oldest-supported CPUs and additional architectures,
and a complete dependency/advisory review. No benchmark or battery claim yet.
Read-only live probes against public GitHub/Sentry endpoints are manual, separate
from deterministic PR tests. No Sentry event is submitted by this evaluation.
