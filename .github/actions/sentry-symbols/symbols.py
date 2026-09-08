"""CI symbol handling. The application never invokes this script or Sentry CLI."""
import hashlib
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import urllib.request

VERSION = "3.7.0"
# Official getsentry/sentry-cli release asset SHA-256 digests, reviewed with this change.
CLI = {
    ("Linux", "x86_64"): ("sentry-cli-Linux-x86_64", "cec71d46a7cc394c94b6e75f1601985c710d457376c546ef3975567b3671563b"),
    ("Darwin", "arm64"): ("sentry-cli-Darwin-arm64", "c66564094fbe56ee3b359f7574541f858b8d1df0328a0a759da972fbf1886048"),
    ("Darwin", "x86_64"): ("sentry-cli-Darwin-x86_64", "fcd74786b4d95c6b7531662607897aadd5ab5d64c5d0468a6f4bd97ad04bedb8"),
    ("Windows", "AMD64"): ("sentry-cli-Windows-x86_64.exe", "8643986aec8d8cf8d69cd476d67427578e5dbbda378eba506d199681082abe5a"),
}


def cli():
    name, digest = CLI[(platform.system(), platform.machine())]
    target = Path(os.environ["RUNNER_TEMP"]) / name
    if not target.exists():
        url = f"https://github.com/getsentry/sentry-cli/releases/download/{VERSION}/{name}"
        with urllib.request.urlopen(url, timeout=60) as response:
            content = response.read(40 * 1024 * 1024)
        if hashlib.sha256(content).hexdigest() != digest:
            raise RuntimeError("Sentry CLI checksum mismatch")
        target.write_bytes(content)
        target.chmod(0o700)
    if hashlib.sha256(target.read_bytes()).hexdigest() != digest:
        raise RuntimeError("Sentry CLI checksum mismatch")
    return str(target)


def prepare():
    binary = Path(os.environ["SYMBOL_BINARY"])
    output = Path(os.environ["SYMBOL_OUTPUT"])
    output.mkdir(parents=True, exist_ok=False)
    if not binary.is_file():
        raise RuntimeError("Missing built executable")
    system = platform.system()
    if system == "Linux":
        debug = output / (binary.name + ".debug")
        subprocess.run(["objcopy", "--only-keep-debug", str(binary), str(debug)], check=True)
        subprocess.run(["objcopy", "--strip-debug", "--strip-unneeded", str(binary)], check=True)
        subprocess.run(["objcopy", "--add-gnu-debuglink=" + str(debug.resolve()), str(binary)], check=True)
    elif system == "Darwin":
        dsym = Path(str(binary) + ".dSYM")
        if not dsym.is_dir():
            subprocess.run(["dsymutil", str(binary), "-o", str(dsym)], check=True)
        shutil.copytree(dsym, output / dsym.name)
        subprocess.run(["strip", "-S", str(binary)], check=True)
        bundled = os.environ.get("SYMBOL_BUNDLE", "")
        if bundled:
            if not Path(bundled).is_file():
                raise RuntimeError("Missing bundled executable")
            shutil.copy2(binary, bundled)
    elif system == "Windows":
        pdb = binary.with_suffix(".pdb")
        if not pdb.is_file():
            raise RuntimeError("Missing PDB; compile with release debug information")
        shutil.copy2(pdb, output / pdb.name)
    else:
        raise RuntimeError("Unsupported symbol platform")
    # Keep the stripped executable as well: its ID must match its debug companion.
    shutil.copy2(binary, output / binary.name)


def upload():
    values = [os.environ.get(name, "") for name in ("SENTRY_AUTH_TOKEN", "SENTRY_ORG", "SENTRY_PROJECT")]
    if not any(values):
        print("Sentry upload disabled: no configuration (debug files retained locally).")
        return
    if not all(values) or any(not re.fullmatch(r"[a-zA-Z0-9_-]+", v) for v in values[1:]):
        raise RuntimeError("Incomplete Sentry symbol configuration")
    executable = cli()
    # No recursive checkout upload and no source bundles; only this variant's output.
    subprocess.run([executable, "debug-files", "upload", "--wait", os.environ["SYMBOL_OUTPUT"]],
                   check=True, timeout=300)


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in {"prepare", "upload", "install"}:
        raise SystemExit("Expected prepare, upload, or install")
    if sys.argv[1] == "install":
        print(cli())
    elif sys.argv[1] == "prepare":
        prepare()
    else:
        upload()
