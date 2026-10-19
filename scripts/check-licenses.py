#!/usr/bin/env python3
"""Validate the locked backend dependency notice inventory (not a legal audit)."""
import json
import tomllib
from pathlib import Path
root = Path(__file__).resolve().parents[1]
lock = tomllib.loads((root / 'backend/Cargo.lock').read_text())
expected = {(p['name'], p['version']) for p in lock['package'] if 'source' in p}
entries = json.loads((root / 'docs/dependency-licenses.json').read_text())
actual = {(p['name'], p['version']) for p in entries}
if expected != actual or len(actual) != len(entries):
    raise SystemExit(f'notice inventory mismatch: missing={expected-actual}, extra={actual-expected}')
for entry in entries:
    if not entry['notices']:
        raise SystemExit(f'missing license text: {entry["name"]}')
    for name in entry['notices']:
        path = (root / name).resolve()
        if not path.is_relative_to(root / 'LICENSES') or not path.is_file() or not path.read_bytes().strip():
            raise SystemExit(f'invalid license material: {name}')
print(f'{len(actual)} locked backend dependency notice entries verified; project license remains undecided.')
