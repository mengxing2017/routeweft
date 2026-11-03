#!/usr/bin/env python3
"""Maintain frontend/package.json as the single frontend release version source."""
import argparse
import json
from pathlib import Path
import re

FRONTEND = Path(__file__).resolve().parents[1] / 'frontend'
PACKAGES = ('routing-client', 'maplibre')
SEMVER = re.compile(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)'
                    r'(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)'
                    r'(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?'
                    r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?')


def version(frontend=FRONTEND):
    value = json.loads((frontend / 'package.json').read_text()).get('version', '')
    if not isinstance(value, str) or not SEMVER.fullmatch(value):
        raise ValueError('frontend/package.json must define an explicit semantic version')
    return value


def changes(frontend, requested=None):
    """Prepare all edits before writing; dependency resolutions remain unchanged."""
    desired = version(frontend) if requested is None else requested
    if not isinstance(desired, str) or not SEMVER.fullmatch(desired):
        raise ValueError('Use an explicit semantic version')
    documents = {}

    def load(relative):
        path = frontend / relative
        document = json.loads(path.read_text())
        documents[path] = document
        return document

    root = load('package.json')
    root['version'] = desired
    names = {f'@routeweft/{package}' for package in PACKAGES}
    for package in PACKAGES:
        data = load(f'packages/{package}/package.json')
        if data.get('name') != f'@routeweft/{package}':
            raise ValueError(f'{package}: unexpected package identity')
        data['version'] = desired
        for section in ('dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies'):
            for name in names & data.get(section, {}).keys():
                data[section][name] = desired
    lock = load('packages/routing-client/package-lock.json')
    if lock.get('name') != '@routeweft/routing-client' or '' not in lock.get('packages', {}):
        raise ValueError('Unexpected routing-client package-lock layout')
    lock['version'] = desired
    lock['packages']['']['version'] = desired
    return {path: json.dumps(data, ensure_ascii=False, indent=2) + '\n'
            for path, data in documents.items()
            if json.loads(path.read_text()) != data}


def check(frontend=FRONTEND):
    pending = changes(frontend)
    if pending:
        paths = ', '.join(str(p.relative_to(frontend)) for p in pending)
        raise ValueError(f'Frontend versions differ from frontend/package.json: {paths}; '
                         'run make frontend-version-sync')
    return version(frontend)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group()
    action.add_argument('--check', action='store_true', help='check without changing files (default)')
    action.add_argument('--sync', action='store_true', help='sync manifests and lock metadata from root version')
    action.add_argument('--set-version', help='set the root version and sync derived metadata')
    action.add_argument('--print', action='store_true', dest='print_version', help='print the root version')
    args = parser.parse_args()
    try:
        if args.print_version:
            print(version())
        elif args.sync or args.set_version is not None:
            pending = changes(FRONTEND, args.set_version)
            for path, content in pending.items():
                path.write_text(content)
            print(f'Frontend version {check()}; synchronized {len(pending)} files')
        else:
            print(f'Frontend version {check()}; all manifests and lock metadata agree')
    except (ValueError, OSError, KeyError) as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
