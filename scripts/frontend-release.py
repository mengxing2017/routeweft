#!/usr/bin/env python3
"""Test, pack and publish versioned frontend libraries to an explicit npm registry."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / 'frontend'
PACKAGES = ('routing-client', 'maplibre')
SEMVER = re.compile(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)'
                    r'(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)'
                    r'(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?'
                    r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?')


def load_release_env():
    spec = importlib.util.spec_from_file_location('cargo_release', ROOT / 'scripts/release.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.load_release_env()


def registry_config(registry, token, require_token=True):
    url = urlsplit(registry)
    if (url.scheme != 'https' or not url.hostname or url.username or url.password
            or url.query or url.fragment or any(c.isspace() for c in registry)
            or any(c in registry for c in '\\${}')):
        raise ValueError('Specify an HTTPS npm registry without credentials using --registry or NPM_REGISTRY')
    if (require_token and not token) or any(c.isspace() for c in token):
        raise ValueError('NPM_TOKEN is required for uploads and must not contain whitespace')
    registry = registry.rstrip('/') + '/'
    auth_path = '//' + url.netloc + url.path.rstrip('/') + '/'
    config = f'registry={registry}\n@routeweft:registry={registry}\n'
    if token:
        config += f'{auth_path}:_authToken=${{ROUTEWEFT_NPM_TOKEN}}\n'
    return registry, config


def run(command, cwd, env):
    result = subprocess.run(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    if result.returncode:
        output = result.stdout.decode(errors='replace')
        for key, value in env.items():
            if value and any(part in key.upper() for part in ('TOKEN', 'PASSWORD', 'SECRET')):
                output = output.replace(value, '<redacted>')
        output = re.sub(r'https?://[^\s\"\'<>]+', '<registry-url>', output)
        raise ValueError(f'{command[0]} {command[1]} failed:\n{output[-4000:]}')
    return result.stdout


def package_metadata(frontend):
    result = {}
    for package in PACKAGES:
        data = json.loads((frontend / 'packages' / package / 'package.json').read_text())
        if data.get('name') != f'@routeweft/{package}' or data.get('private'):
            raise ValueError(f'{package}: expected a publishable @routeweft package')
        if not SEMVER.fullmatch(data.get('version', '')):
            raise ValueError(f'{package}: version must be an explicit semantic version')
        if data.get('publishConfig'):
            raise ValueError(f'{package}: publish settings must be supplied by the release command')
        result[package] = data
    if result['maplibre']['dependencies'].get('@routeweft/routing-client') != result['routing-client']['version']:
        raise ValueError('maplibre must depend on the exact routing-client release version')
    return result


def validate_archive(archive, expected):
    with tarfile.open(archive, 'r:gz') as package:
        names = {item.name for item in package.getmembers() if item.isfile()}
        required = {'package/package.json', 'package/dist/index.js', 'package/dist/index.d.ts',
                    'package/README.md', 'package/THIRD_PARTY_NOTICES.md'}
        if not required <= names:
            raise ValueError(f'{expected["name"]}: archive is missing JS, types or documentation')
        metadata = json.load(package.extractfile('package/package.json'))
        if any(metadata.get(key) != expected[key] for key in ('name', 'version')):
            raise ValueError('Packed package identity differs from the source manifest')
        for section in ('dependencies', 'optionalDependencies', 'peerDependencies'):
            for version in metadata.get(section, {}).values():
                if version.startswith(('workspace:', 'file:', 'link:')):
                    raise ValueError('Archive contains a local-only dependency')
        if metadata.get('dependencies', {}) != expected.get('dependencies', {}):
            raise ValueError('Packed dependencies differ from the source manifest')
        if any('/node_modules/' in name or Path(name).name == '.npmrc'
               or Path(name).name.startswith('.env') for name in names):
            raise ValueError('Archive contains local configuration or installed dependencies')
        if metadata['name'] == '@routeweft/routing-client' and 'package/LICENSE.osrm-frontend' not in names:
            raise ValueError('routing-client archive is missing its upstream license')


def release(execute=False, registry=None, packages=None, access='restricted', tag=None):
    load_release_env()
    registry, config = registry_config(
        registry or os.environ.get('NPM_REGISTRY') or os.environ.get('NPM_REGISTRY_INTERNAL', ''),
        os.environ.get('NPM_TOKEN', os.environ.get('NPM_TOKEN_INTERNAL', '')), require_token=execute)
    metadata = package_metadata(FRONTEND)
    selected = tuple(p for p in PACKAGES if not packages or p in packages)
    if not selected or (packages and set(packages) - set(PACKAGES)):
        raise ValueError('Select routing-client or maplibre')
    if access not in ('public', 'restricted'):
        raise ValueError('access must be public or restricted')
    if tag is not None and (not re.fullmatch(r'[a-z][a-z0-9_-]*', tag)):
        raise ValueError('Use a non-version dist-tag such as latest, next or beta')
    if tag == 'latest' and any(SEMVER.fullmatch(metadata[p]['version']).group(4) for p in selected):
        raise ValueError('Prereleases must use a non-latest dist-tag')
    with tempfile.TemporaryDirectory(prefix='routeweft-npm-release-') as temporary:
        stage = Path(temporary)
        frontend = stage / 'frontend'
        shutil.copytree(FRONTEND, frontend, ignore=shutil.ignore_patterns(
            'node_modules', 'dist', 'example-dist', '*.tgz', '.npmrc', '.git', '.env*'))
        npmrc = stage / 'npmrc'
        npmrc.write_text(config)
        npmrc.chmod(0o600)
        env = {key: value for key, value in os.environ.items()
               if not key.lower().startswith('npm_config_')}
        env.update(NPM_CONFIG_USERCONFIG=str(npmrc), NPM_CONFIG_CACHE=str(stage / 'cache'),
                   ROUTEWEFT_NPM_TOKEN=os.environ.get('NPM_TOKEN', os.environ.get('NPM_TOKEN_INTERNAL', '')))
        print('Installing locked workspace dependencies...', flush=True)
        run(['pnpm', 'install', '--frozen-lockfile', '--ignore-scripts'], frontend, env)
        # Build the client before testing the adapter that imports its generated exports.
        for package in PACKAGES:
            directory = frontend / 'packages' / package
            print(f'Testing @routeweft/{package}@{metadata[package]["version"]}...', flush=True)
            run(['pnpm', 'run', 'build'], directory, env)
            run(['pnpm', 'test'], directory, env)
        commands = []
        for package in selected:
            directory = frontend / 'packages' / package
            packed = json.loads(run(['npm', 'pack', '--ignore-scripts', '--json'], directory, env))
            filename = packed[0]['filename']
            if Path(filename).name != filename or not filename.endswith('.tgz'):
                raise ValueError('Unexpected npm archive name')
            archive = directory / filename
            validate_archive(archive, metadata[package])
            effective_tag = tag or ('next' if SEMVER.fullmatch(metadata[package]['version']).group(4) else 'latest')
            command = ['npm', 'publish', str(archive), '--registry', registry,
                       '--access', access, '--tag', effective_tag, '--ignore-scripts']
            print(f'Preview {metadata[package]["name"]}@{metadata[package]["version"]} (tag={effective_tag}, access={access})', flush=True)
            run([*command, '--dry-run'], stage, env)
            commands.append(command)
        # Validate every selected artifact before uploading the first package.
        for package, command in zip(selected, commands) if execute else ():
            identity = f'{metadata[package]["name"]}@{metadata[package]["version"]}'
            print(f'Publishing {identity}...', flush=True)
            run(command, stage, env)
            print(f'Published {identity}.', flush=True)
        print('Frontend packages published.' if execute else
              'Frontend publish dry-run passed; nothing uploaded.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute', action='store_true', help='upload after checks; default is dry-run')
    parser.add_argument('--registry', help='explicit HTTPS npm registry; otherwise NPM_REGISTRY')
    parser.add_argument('--package', action='append', choices=PACKAGES, dest='packages', help='select packages; default: both in dependency order')
    parser.add_argument('--access', choices=('public', 'restricted'), default='restricted')
    parser.add_argument('--tag', help='default: latest for stable versions, next for prereleases')
    args = parser.parse_args()
    try:
        release(args.execute, args.registry, args.packages, args.access, args.tag)
    except (ValueError, OSError, KeyError, IndexError, tarfile.TarError) as error:
        parser.exit(1, f'Frontend release failed: {error}\n')


if __name__ == '__main__':
    main()
