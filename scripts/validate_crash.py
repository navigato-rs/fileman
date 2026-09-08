"""Run only the isolated synthetic probe, never a real application's profile."""
import argparse
import json
from pathlib import Path
import platform
import re
import subprocess

import sentry_report

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("symbols", type=Path)
parser.add_argument("destination", type=Path)
args = parser.parse_args()
result = subprocess.run([str(args.binary.resolve()), str(args.destination)],
                        capture_output=True, timeout=30)
assert result.returncode != 0, "probe did not panic"
reports = list(args.destination.glob("failure-*.json"))
assert len(reports) == 1, "expected one isolated diagnostic report"
text = reports[0].read_text()
assert "CANARY" not in text, "panic payload leaked into the report"
report = json.loads(text)
assert report["sentry_consent"] is False
trace = report["payload"]["trace"]
assert trace and trace["offsets"], "executable backtrace was not captured"
sentry_report.event_from_report(report, "fileman")
# Match Sentry's return-address adjustment when checking locally. AArch64
# instructions are four bytes; x86 needs only to point inside the call instruction.
adjustment = {"aarch64": 4, "arm": 2, "riscv64": 2}.get(report["arch"], 1)
addresses = [hex(max(0, offset - adjustment) + trace["image_vmaddr"]) for offset in trace["offsets"]]
if platform.system() == "Linux":
    debug = args.symbols / (args.binary.name + ".debug")
    resolved = subprocess.check_output(["addr2line", "-f", "-C", "-e", str(debug), *addresses], text=True)
    lines = resolved.splitlines()
    assert any("synthetic_panic" in function and re.search(r"crash_probe\.rs:[1-9][0-9]*", site)
               for function, site in zip(lines[::2], lines[1::2])), "panic frame has no source line"
elif platform.system() == "Darwin":
    dwarf = args.symbols / (args.binary.name + ".dSYM") / "Contents/Resources/DWARF" / args.binary.name
    resolved = subprocess.check_output(["atos", "-o", str(dwarf), *addresses], text=True)
    assert any("synthetic_panic" in line and re.search(r"crash_probe\.rs:[1-9][0-9]*", line)
               for line in resolved.splitlines()), "panic frame has no source line"
else:
    assert (args.symbols / args.binary.with_suffix(".pdb").name).stat().st_size > 0
print(f"Synthetic panic: {len(trace['offsets'])} executable frames; report validation passed.")
