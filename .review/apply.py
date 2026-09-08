"""Reconstruct an exact local-tested tree without changing remote refs."""
from pathlib import Path
import hashlib
import json
import subprocess
import sys
import zlib
root = Path(sys.argv[1])
folder = Path(__file__).parent
packed = b''.join((folder / f'part{i}').read_bytes() for i in range(4))
assert hashlib.sha256(packed).hexdigest() == 'b5fd07220f34615ab2fb6899c85c9bdd21670269cb88c1a88137297745718fcc'
plan = json.loads(zlib.decompress(packed))
def git(*args):
    return subprocess.check_output(['git', '-C', str(root), *args], text=True).strip()
assert git('rev-parse', 'HEAD') == plan['base']
assert not git('status', '--porcelain')
for flags in [('--check', '--index'), ('--index',)]:
    subprocess.run(['git', '-C', str(root), 'apply', *flags, '-'], input=plan['patch'], text=True, check=True)
assert git('write-tree') == plan['tree']
git('diff', '--cached', '--check')
print('Prepared exact tested tree:', plan['tree'])
