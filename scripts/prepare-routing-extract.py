#!/usr/bin/env python3
"""Create a provenance-bound restriction-complete extract from an explicit parent.

Requires an installed osmium executable and a same-snapshot, complete parent.
This never guesses member roles or downloads/replaces map data implicitly.
"""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import subprocess
import time


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--osmium', type=Path, required=True)
    p.add_argument('--parent', type=Path, required=True)
    p.add_argument('--parent-sha256', required=True)
    p.add_argument('--snapshot', type=int, required=True, help='Expected parent replication timestamp, Unix seconds')
    p.add_argument('--boundary', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True, help='New artifact directory; never overwritten')
    args = p.parse_args()
    tool = args.osmium.resolve(strict=True)
    parent = args.parent.resolve(strict=True)
    boundary = args.boundary.resolve(strict=True)
    if digest(parent) != args.parent_sha256:
        p.error('parent SHA-256 mismatch')
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {'complete': False, 'commands': [], 'parent_sha256': args.parent_sha256,
                'boundary_sha256': digest(boundary), 'snapshot': args.snapshot,
                'strategy': 'smart', 'relation_types': ['multipolygon', 'restriction'],
                'policy': 'preserve roles; no semantic repair'}

    def call(*argv):
        command = [str(tool), *map(str, argv)]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        manifest['commands'].append({'command': command, 'exit_code': result.returncode,
                                     'stdout': result.stdout, 'stderr': result.stderr})
        if result.returncode:
            raise RuntimeError(f'osmium {argv[0]} failed; see manifest commands')
        return result.stdout.strip()

    try:
        manifest['osmium_version'] = call('--version')
        timestamp = call('fileinfo', '-g', 'header.option.osmosis_replication_timestamp', parent)
        # Reject a parent with missing/unverifiable snapshot; do not label a
        # different snapshot as a correction of the fixed regression input.
        actual = int(datetime.fromisoformat(timestamp.replace('Z', '+00:00')).timestamp())
        if actual != args.snapshot:
            raise RuntimeError('parent snapshot mismatch')
        derived = args.output / 'routing.osm.pbf'
        call('extract', '-s', 'smart', '-S', 'types=multipolygon,restriction',
             '-p', boundary, '-o', derived, parent)
        restrictions = args.output / 'restrictions.osm.pbf'
        # tags-filter includes dependencies by default. Checking only restriction
        # dependencies avoids failing on unrelated, intentionally partial routes.
        call('tags-filter', derived, 'r/type=restriction', '-o', restrictions)
        call('check-refs', '-r', restrictions)
        if digest(parent) != args.parent_sha256 or digest(boundary) != manifest['boundary_sha256']:
            raise RuntimeError('source or boundary changed during extraction')
        manifest['source_sha256'] = digest(derived)
        manifest['restriction_subset_sha256'] = digest(restrictions)
        manifest['source_bytes'] = derived.stat().st_size
        manifest['complete'] = True
    except Exception as error:
        manifest['error'] = str(error)
        raise
    finally:
        manifest['finished_unix_seconds'] = time.time()
        (args.output / 'source-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
