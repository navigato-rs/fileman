#!/usr/bin/env python3
"""Reject native SSH/TLS/crypto implementations in Cargo's effective target tree."""
import argparse
import re
import subprocess

FORBIDDEN = {
    "ssh2", "libssh2-sys", "openssl", "openssl-sys", "openssl-src", "native-tls",
    "ring", "aws-lc-rs", "aws-lc-sys", "aws-lc-fips-sys", "mbedtls", "mbedtls-sys-auto",
    "graviola", "rustls-graviola",
}


def forbidden(tree):
    names = set()
    for line in tree.splitlines():
        if not line.strip():
            continue
        match = re.match(r"^([A-Za-z0-9_-]+) v[0-9]", line)
        if not match:
            raise ValueError("unexpected Cargo dependency-tree format")
        names.add(match[1])
    if not names:
        raise ValueError("empty Cargo dependency tree")
    return sorted(names & FORBIDDEN)


def tree_command(manifest, target, package=None):
    # cargo metadata can retain optional nodes activated only by weak features
    # (e.g. webpki's ring?/alloc). Let Cargo resolve actual build edges instead
    # of reimplementing feature resolution or treating Cargo.lock as a build.
    return ["cargo", "tree", "--locked", "--all-features", "--target", target,
            "--manifest-path", manifest, "--edges", "normal,build,dev",
            "--prefix", "none", "--format", "{p}",
            *(["--package", package] if package else ["--workspace"])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", default="Cargo.toml")
    parser.add_argument("--package", help="check only this package")
    parser.add_argument("--target", help="otherwise use rustc's host target")
    args = parser.parse_args()
    target = args.target
    if not target:
        version = subprocess.check_output(["rustc", "-vV"], text=True)
        target = next(line.split(": ", 1)[1] for line in version.splitlines() if line.startswith("host: "))
    tree = subprocess.check_output(tree_command(args.manifest_path, target, args.package), text=True)
    banned = forbidden(tree)
    if banned:
        raise RuntimeError("native crypto dependencies are forbidden: " + ", ".join(banned))
    print(f"Rust implementation policy passed for {target} ({args.package or 'workspace'}).")


if __name__ == "__main__":
    main()
