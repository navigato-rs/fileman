"""Store validated blobs; the connected publisher creates trees and review refs."""
from pathlib import Path
import base64
import json
import os
import subprocess
import sys
import urllib.request
root = Path(sys.argv[1])
def git(*args):
    return subprocess.check_output(['git', '-C', str(root), *args])
tree = git('write-tree').decode().strip()
assert tree == '08d38337dc8aaf37a1c10b79d735c8bebd544ea5'
assert not git('diff', '--name-only')
entries = []
for path in git('diff', '--cached', '--name-only').decode().splitlines():
    mode, expected, *_ = git('ls-files', '--stage', '--', path).decode().split()
    data = git('show', ':' + path)
    req = urllib.request.Request('https://api.github.com/repos/navigato-rs/fileman/git/blobs',
        json.dumps({'content': base64.b64encode(data).decode(), 'encoding': 'base64'}).encode(),
        headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'], 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=30) as res:
        sha = json.load(res)['sha']
    assert sha == expected
    entries.append({'path': path, 'mode': mode, 'type': 'blob', 'sha': sha})
output = root.parent / 'export'
output.mkdir(exist_ok=True)
(output / 'publication.json').write_text(json.dumps({'tree': tree, 'parent': git('rev-parse','HEAD').decode().strip(), 'entries': entries}))
