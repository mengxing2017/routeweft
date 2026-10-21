#!/usr/bin/env python3
"""Private Cargo release staging. Never store registry URLs in public manifests."""
import argparse
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
ORDER = ('routeweft-domain', 'routeweft-dataset', 'routeweft-routing', 'routeweft-preprocess')
REPOSITORY = 'https://github.com/mengxing2017/routeweft'


def git(*args):
    return subprocess.check_output(['git', '-C', str(ROOT), *args])


def rules(path):
    if path is None:
        index = os.environ.get('CARGO_REGISTRIES_INTERNAL_INDEX', '')
        url = urlsplit(index.removeprefix('sparse+'))
        if url.scheme != 'https' or not url.hostname or url.username or url.password:
            raise ValueError('set CARGO_REGISTRIES_INTERNAL_INDEX to an HTTPS index')
        return [url.hostname.lower().encode()]
    values = [line.strip().lower().encode() for line in path.read_text().splitlines()
              if line.strip() and not line.lstrip().startswith('#')]
    if not values:
        raise ValueError('private address pattern file must not be empty')
    return values


def inspect_bytes(data, patterns, label):
    # Literal, case-insensitive matching; never print the matched private value.
    if any(pattern in data.lower() for pattern in patterns):
        raise ValueError(f'private address detected in {label}')


def scan_tree(path, patterns):
    for file in sorted(path.rglob('*')):
        inspect_bytes(str(file.relative_to(path)).encode(), patterns, 'file name')
        if file.is_symlink():
            raise ValueError('symlinks are not permitted in release staging')
        if file.is_file():
            inspect_bytes(file.read_bytes(), patterns, str(file.relative_to(path)))


def scan_archive(path, patterns, private_index=None):
    with tarfile.open(path, 'r:gz') as archive:
        for member in archive:
            inspect_bytes(member.name.encode(), patterns, 'archive entry name')
            if member.isfile():
                data = archive.extractfile(member).read()
                # Cargo embeds the selected index in generated manifests/lockfiles.
                # These archives remain private; exempt only that exact index there.
                if private_index and Path(member.name).name in ('Cargo.toml', 'Cargo.lock'):
                    for value in (private_index, private_index.removeprefix('sparse+')):
                        data = data.replace(value.encode(), b'<private-index>')
                inspect_bytes(data, patterns, 'archive entry')
            elif not member.isdir():
                raise ValueError('unsupported archive entry type')


def check(args):
    patterns = rules(args.patterns)
    if args.history:
        # Include commit/tag messages and all reachable historical blobs, not .git/config.
        objects = git('rev-list', '--objects', '--all').splitlines()
        for entry in objects:
            oid = entry.split(b' ', 1)[0].decode()
            kind = git('cat-file', '-t', oid).decode().strip()
            if kind in ('blob', 'commit', 'tag'):
                inspect_bytes(git('cat-file', kind, oid), patterns, f'Git object {oid}')
        for oid in git('for-each-ref', '--format=%(objectname)', 'refs/tags').splitlines():
            inspect_bytes(git('cat-file', '-p', oid.decode()), patterns, 'Git tag')
    if args.path:
        if args.path.is_dir():
            scan_tree(args.path, patterns)
        else:
            scan_archive(args.path, patterns)
    elif not args.history:
        for name in git('ls-files', '-z').split(b'\0'):
            if name:
                file = ROOT / os.fsdecode(name)
                if file.is_symlink():
                    raise ValueError('tracked symlink requires manual review')
                if file.is_file():
                    inspect_bytes(file.read_bytes(), patterns, 'tracked file')
    print('Private address check passed (literal patterns only).')


def prepare(args):
    patterns = rules(args.patterns)
    destination = args.output.resolve()
    if destination.is_relative_to(ROOT) or destination.exists():
        raise ValueError('output must be a new directory outside the source repository')
    license_data = args.license_file.read_bytes() if args.license_file else b''
    inspect_bytes(license_data, patterns, 'project license')
    commit = git('rev-parse', '--verify', args.ref + '^{commit}').decode().strip()
    archive_data = git('archive', commit, 'backend', 'LICENSES', 'THIRD_PARTY_NOTICES.md',
                       'docs/dependency-licenses.json', 'docs/data-sources.md')
    destination.mkdir(mode=0o700, parents=True)
    # Extract only regular files/directories; reject links and escaping paths.
    with tarfile.open(fileobj=io.BytesIO(archive_data)) as archive:
        for member in archive:
            target = (destination / member.name).resolve()
            if not target.is_relative_to(destination):
                raise ValueError('unsafe archive path')
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(archive.extractfile(member).read())
            else:
                raise ValueError('unsupported Git archive entry')
    scan_tree(destination, patterns)
    manifest = destination / 'backend/Cargo.toml'
    text = manifest.read_text()
    metadata = tomllib.loads(text)
    version = metadata['workspace']['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', version):
        raise ValueError('unsupported release version')
    if text.count('publish = false') != 1:
        raise ValueError('workspace publish protection changed; review transformation')
    text = text.replace('publish = false', 'publish = ["internal"]\nrepository = "' + REPOSITORY + '"')
    for name in ORDER:
        original = metadata['workspace']['dependencies'][name]
        expected = f'{name} = {{ path = "{original["path"]}" }}'
        replacement = f'{name} = {{ path = "{original["path"]}", version = "={version}", registry = "internal" }}'
        if text.count(expected) != 1:
            raise ValueError(f'dependency layout changed: {name}')
        text = text.replace(expected, replacement)
    manifest.write_text(text)
    for name in (*ORDER, 'routeweft-daemon'):
        crate = destination / 'backend/crates' / name
        manifest = crate / 'Cargo.toml'
        text = manifest.read_text()
        if text.count('publish.workspace = true') != 1:
            raise ValueError(f'publish layout changed: {name}')
        if name == 'routeweft-daemon':
            text = text.replace('publish.workspace = true', 'publish = false')
        else:
            text = text.replace('publish.workspace = true',
                                'publish.workspace = true\nrepository.workspace = true')
            if license_data:
                text = text.replace('repository.workspace = true',
                                    'repository.workspace = true\nlicense-file = "LICENSE"')
                (crate / 'LICENSE').write_bytes(license_data)
            shutil.copytree(destination / 'LICENSES', crate / 'LICENSES')
            shutil.copyfile(destination / 'THIRD_PARTY_NOTICES.md', crate / 'THIRD_PARTY_NOTICES.md')
            (crate / 'docs').mkdir(exist_ok=True)
            for notice in ('dependency-licenses.json', 'data-sources.md'):
                shutil.copyfile(destination / 'docs' / notice, crate / 'docs' / notice)
        manifest.write_text(text)
    # Lockfile may contain registry URLs after Cargo resolves internal dependencies.
    # Keep the entire staging directory private, including logs and archives.
    (destination / 'release-source.json').write_text(json.dumps({
        'commit': commit, 'version': version, 'packages': list(ORDER),
        'registry': 'internal',
    }, indent=2) + '\n')
    scan_tree(destination, patterns)
    print('Private release staging prepared. Source repository was not modified.')


def publish(args):
    stage = args.stage.resolve()
    if stage.is_relative_to(ROOT):
        raise ValueError('release staging must be outside the source repository')
    source = json.loads((stage / 'release-source.json').read_text())
    if source['packages'] != list(ORDER) or source['registry'] != 'internal':
        raise ValueError('unexpected release staging metadata')
    index = os.environ.get('CARGO_REGISTRIES_INTERNAL_INDEX', '')
    if not index.startswith(('https://', 'sparse+https://')):
        raise ValueError('CARGO_REGISTRIES_INTERNAL_INDEX must specify an HTTPS index')
    if not os.environ.get('CARGO_REGISTRIES_INTERNAL_TOKEN'):
        raise ValueError('CARGO_REGISTRIES_INTERNAL_TOKEN is required')
    patterns = rules(args.patterns)
    # Do not scan a resolved Cargo.lock: it intentionally contains private index data.
    for name in ORDER:
        scan_tree(stage / 'backend/crates' / name, patterns)
    # Run one package at a time. Caller waits for index visibility before proceeding.
    command = ['cargo', 'publish', '--registry', 'internal', '-p', args.package]
    env = os.environ.copy()
    env['CARGO_REGISTRIES_INTERNAL_CREDENTIAL_PROVIDER'] = 'cargo:token'
    # Capture Cargo output: errors may contain the private URL or credentials.
    result = subprocess.run([*command, '--dry-run'], cwd=stage / 'backend', env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode:
        raise ValueError('Cargo publish failed; output suppressed to protect private configuration')
    archive = stage / 'backend/target/package' / f'{args.package}-{source["version"]}.crate'
    if not archive.is_file():
        raise ValueError('Cargo did not produce the expected archive')
    scan_archive(archive, patterns, private_index=index)
    if args.execute:
        result = subprocess.run(command, cwd=stage / 'backend', env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            raise ValueError('Cargo upload failed; output suppressed')
    print('Publish completed.' if args.execute else 'Publish dry-run completed.')


def release(args):
    # Validate before preparing anything. Cargo uses the user's normal cache/config.
    rules(None)
    if not os.environ.get('CARGO_REGISTRIES_INTERNAL_TOKEN'):
        raise ValueError('CARGO_REGISTRIES_INTERNAL_TOKEN is required')
    with tempfile.TemporaryDirectory(prefix='routeweft-release-') as temporary:
        stage = Path(temporary) / 'stage'
        prepare(argparse.Namespace(patterns=None, output=stage, ref=args.ref,
                                   license_file=None))
        for package in ([args.package] if args.package else ORDER):
            print(f'{package}: {"publish" if args.execute else "dry-run"}', flush=True)
            publish(argparse.Namespace(stage=stage, patterns=None, package=package,
                                       execute=args.execute))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    checker = commands.add_parser('check')
    checker.add_argument('--patterns', type=Path)
    checker.add_argument('--path', type=Path)
    checker.add_argument('--history', action='store_true')
    checker.set_defaults(run=check)
    staging = commands.add_parser('prepare')
    staging.add_argument('--ref', default='HEAD')
    staging.add_argument('--output', type=Path, required=True)
    staging.add_argument('--patterns', type=Path)
    staging.add_argument('--license-file', type=Path)
    staging.set_defaults(run=prepare)
    publisher = commands.add_parser('publish')
    publisher.add_argument('--stage', type=Path, required=True)
    publisher.add_argument('--patterns', type=Path)
    publisher.add_argument('--package', choices=ORDER, required=True)
    publisher.add_argument('--execute', action='store_true', help='upload; default is dry-run')
    publisher.set_defaults(run=publish)
    automatic = commands.add_parser('release', help='prepare and publish all libraries automatically')
    automatic.add_argument('--ref', default='HEAD')
    automatic.add_argument('--package', choices=ORDER)
    automatic.add_argument('--execute', action='store_true', help='upload; default is dry-run')
    automatic.set_defaults(run=release)
    args = parser.parse_args()
    try:
        args.run(args)
    except (ValueError, OSError, KeyError, tarfile.TarError, subprocess.CalledProcessError):
        # Exceptions can embed paths or private configuration; avoid printing their contents.
        parser.exit(1, 'Release operation failed; verify inputs and private staging configuration.\n')


if __name__ == '__main__':
    main()
