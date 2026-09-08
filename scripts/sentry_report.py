#!/usr/bin/env python3
"""Validate a reviewed Navigato report and optionally import it into hosted Sentry.

No network access without --send. Run locally for private reports, never in public CI.
Only the environment supplies the destination; a report cannot choose its recipient.
"""
import argparse
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid

MAX_BYTES = 8192
KINDS = {"panic", "gpu_initialization", "surface_creation", "frame_wait", "desktop_exit"}
TOKEN = r"[a-zA-Z0-9._+\-]{1,80}"
HEX_ID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}(?:-[0-9a-fA-F]{1,8})?"


def require(condition):
    if not condition:
        raise ValueError("Invalid or unsupported diagnostic report")


def fields(value, required, optional=()):
    require(type(value) is dict and set(required) <= value.keys()
            and value.keys() <= set(required) | set(optional))


def text(value, pattern):
    return type(value) is str and re.fullmatch(pattern, value) is not None


def integer(value, low, high):
    return type(value) is int and low <= value <= high


def event_from_report(report, app):
    fields(report, {"schema", "app", "version", "revision", "os", "arch", "payload"}, {"sentry_consent"})
    require(type(report.get("sentry_consent", False)) is bool)
    require(type(report["schema"]) is int and report["schema"] == 2)
    require(report["app"] == app and app in {"fileman", "starcom"})
    require(text(report["version"], TOKEN) and text(report["revision"], TOKEN))
    require(report["os"] in {"linux", "macos", "windows"})
    require(report["arch"] in {"x86", "x86_64", "arm", "aarch64", "riscv64"})
    payload = report["payload"]
    fields(payload, {"type", "kind", "site", "trace"})
    require(payload["type"] == "failure" and payload["kind"] in KINDS)
    site = payload["site"]
    if site is not None:
        fields(site, {"file", "line", "column"})
        require(text(site["file"], r"src/[a-zA-Z0-9/_\-.]{1,153}\.rs")
                and ".." not in site["file"])
        require(integer(site["line"], 0, 2**32 - 1) and integer(site["column"], 0, 2**32 - 1))
    trace = payload["trace"]
    fields(trace, {"debug_id", "code_id", "image_size", "image_vmaddr", "offsets"})
    require(text(trace["debug_id"], HEX_ID))
    require(trace["code_id"] is None or text(trace["code_id"], r"[0-9a-fA-F]{1,128}"))
    require(integer(trace["image_size"], 1, 2**40))
    require(integer(trace["image_vmaddr"], 0, 2**64 - 1))
    offsets = trace["offsets"]
    require(type(offsets) is list and 0 < len(offsets) <= 48)
    require(all(integer(offset, 0, trace["image_size"] - 1) for offset in offsets))
    # Reconstruct a closed event rather than forwarding fields from untrusted JSON.
    image = {
        "type": {"linux": "elf", "macos": "macho", "windows": "pe"}[report["os"]],
        "debug_id": trace["debug_id"], "code_file": app,
        "image_addr": "0x0", "image_size": trace["image_size"],
        "image_vmaddr": hex(trace["image_vmaddr"]), "arch": report["arch"],
    }
    if trace["code_id"] is not None:
        image["code_id"] = trace["code_id"]
    if report["os"] == "windows":
        image["debug_file"] = app + ".pdb"
    return {
        "event_id": uuid.uuid4().hex, "platform": "native", "level": "error",
        "release": f"{app}@{report['version']}+{report['revision']}",
        "environment": "reviewed-report",
        "exception": {"values": [{
            "type": payload["kind"], "value": "User-reviewed application failure",
            # These are sampled return addresses, not a faulting CPU context.
            "stacktrace": {"instruction_addr_adjustment": "all", "frames": [
                {"instruction_addr": hex(offset), "addr_mode": "rel:0", "in_app": True}
                for offset in offsets
            ]},
        }]},
        "debug_meta": {"images": [image]},
        "tags": {"app": app, "os": report["os"], "arch": report["arch"],
                 "capture_time": "unknown", "capture": "executable-only"},
    }


def destination(dsn):
    try:
        url = urllib.parse.urlsplit(dsn)
        require(url.scheme == "https" and url.port in (None, 443)
                and url.password is None and not url.query and not url.fragment)
        require(text(url.username, r"[a-fA-F0-9]{32,64}")
                and text(url.hostname, r"o[0-9]+\.ingest\.(?:[a-z]{2}\.)?sentry\.io")
                and text(url.path, r"/[0-9]+"))
        return f"https://{url.hostname}/api{url.path}/envelope/", url.username
    except (ValueError, TypeError):
        raise ValueError("SENTRY_DSN must identify a hosted HTTPS Sentry project") from None


class NoRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Sentry redirect refused; no report was forwarded")


def send_report(report, app, dsn):
    require(report.get("sentry_consent") is True)
    event = event_from_report(report, app)
    return _send_event(event, dsn)


def _send_event(event, dsn):
    url, key = destination(dsn)
    encoded = json.dumps(event, separators=(",", ":")).encode()
    envelope = (json.dumps({"event_id": event["event_id"]}).encode() + b"\n"
                + json.dumps({"type": "event", "length": len(encoded)}).encode()
                + b"\n" + encoded + b"\n")
    request = urllib.request.Request(url, envelope, headers={
        "Content-Type": "application/x-sentry-envelope",
        "X-Sentry-Auth": f"Sentry sentry_version=7, sentry_key={key}, sentry_client=navigato-import/1",
    })
    # Use Python's verified HTTPS transport only in this maintainer tool, not the app.
    # No proxy discovery, redirects, authentication token, retries, or attachments.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirects())
    try:
        with opener.open(request, timeout=15) as response:
            require(200 <= response.status < 300)
            response.read(MAX_BYTES)
    except urllib.error.HTTPError as error:
        raise ValueError(f"Sentry rejected the report (HTTP {error.code}); not retried") from None
    except (OSError, urllib.error.URLError):
        raise ValueError("Sentry delivery failed; outcome may be uncertain, not retried") from None
    return event["event_id"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--app", choices=["fileman", "starcom"], required=True)
    parser.add_argument("--send", action="store_true", help="send this reviewed report to SENTRY_DSN")
    args = parser.parse_args()
    with args.report.open("rb") as file:
        data = file.read(MAX_BYTES + 1)
    require(len(data) <= MAX_BYTES)
    report = json.loads(data)
    if args.send:
        event_id = send_report(report, args.app, os.environ.get("SENTRY_DSN", ""))
        print(f"Sentry accepted event {event_id}; inspect symbolication in the project.")
    else:
        print(json.dumps(event_from_report(report, args.app), indent=2))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, TypeError, KeyError, RecursionError, OSError):
        # Exception text may contain input or an endpoint. Do not dump it in CI.
        print("Report validation or delivery failed; no automatic retry.", file=sys.stderr)
        sys.exit(1)
