#!/usr/bin/env python3
"""Build, test and publish the frontend library to an explicit private npm registry."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / 'frontend/packages/routing-client'


def registry_config(registry, token):
    url = urlsplit(registry)
    if (url.scheme != 'https' or not url.hostname or url.username or url.password
            or url.query or url.fragment or any(c.isspace() for c in registry)
            or any(c in registry for c in '\\${}')):
        raise ValueError('NPM_REGISTRY_INTERNAL must be an HTTPS registry URL without credentials')
    if not token or any(c.isspace() for c in token):
        raise ValueError('NPM_TOKEN_INTERNAL is required and must not contain whitespace')
    registry = registry.rstrip('/') + '/'
    auth_path = '//' + url.netloc + url.path.rstrip('/') + '/'
    return registry, (f'registry={registry}\n@routeweft:registry={registry}\n'
                      f'{auth_path}:_authToken=${{NPM_TOKEN_INTERNAL}}\n')


def run(command, cwd, env):
    # npm diagnostics can include private URLs or credentials. Do not echo them.
    result = subprocess.run(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    if result.returncode:
        raise ValueError(f'{command[1]} failed; npm output suppressed to protect private configuration')
    return result.stdout


def release(execute=False):
    registry, config = registry_config(os.environ.get('NPM_REGISTRY_INTERNAL', ''),
                                       os.environ.get('NPM_TOKEN_INTERNAL', ''))
    with tempfile.TemporaryDirectory(prefix='routeweft-npm-release-') as temporary:
        stage = Path(temporary)
        package = stage / 'package'
        # Publish the current working tree; generated output is always rebuilt.
        shutil.copytree(PACKAGE, package, ignore=shutil.ignore_patterns(
            'node_modules', 'dist', '*.tgz', '.npmrc', '.git'))
        npmrc = stage / 'npmrc'
        npmrc.write_text(config)
        npmrc.chmod(0o600)
        env = {key: value for key, value in os.environ.items()
               if not key.lower().startswith('npm_config_')}
        env.update(NPM_CONFIG_USERCONFIG=str(npmrc),
                   NPM_CONFIG_CACHE=str(stage / 'cache'))
        print('Installing locked dependencies and testing frontend package...', flush=True)
        run(['npm', 'ci', '--ignore-scripts', '--no-audit', '--no-fund'], package, env)
        run(['npm', 'test'], package, env)
        packed = json.loads(run(['npm', 'pack', '--ignore-scripts', '--json'], package, env))
        filename = packed[0]['filename']
        if Path(filename).name != filename or not filename.endswith('.tgz'):
            raise ValueError('Unexpected npm archive name')
        archive = package / filename
        command = ['npm', 'publish', str(archive), '--registry', registry, '--ignore-scripts']
        run([*command, '--dry-run'], stage, env)
        if execute:
            run(command, stage, env)
        print('Frontend package published.' if execute else
              'Frontend publish dry-run passed; nothing uploaded.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute', action='store_true', help='upload after checks; default is dry-run')
    args = parser.parse_args()
    try:
        release(args.execute)
    except (ValueError, OSError, KeyError, IndexError):
        parser.exit(1, 'Frontend release failed; check registry/token, dependencies, tests and package version.\n')


if __name__ == '__main__':
    main()
