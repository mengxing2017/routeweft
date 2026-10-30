import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('frontend_release', Path(__file__).parents[1] / 'frontend-release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def archive(path, metadata, missing=None):
    files = {'package/package.json': json.dumps(metadata), 'package/dist/index.js': 'export {};',
             'package/dist/index.d.ts': 'export {};', 'package/README.md': 'Usage',
             'package/THIRD_PARTY_NOTICES.md': 'Notice', 'package/LICENSE.osrm-frontend': 'License'}
    with tarfile.open(path, 'w:gz') as packed:
        for name, content in files.items():
            if name == missing:
                continue
            data = content.encode()
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            packed.addfile(entry, io.BytesIO(data))


class FrontendReleaseTests(unittest.TestCase):
    def fixture(self, root):
        frontend = Path(root)
        for name, version in [('routing-client', '0.2.0'), ('maplibre', '0.1.0')]:
            directory = frontend / 'packages' / name
            directory.mkdir(parents=True)
            metadata = {'name': '@routeweft/' + name, 'version': version}
            if name == 'maplibre':
                metadata['dependencies'] = {'@routeweft/routing-client': '0.2.0'}
            (directory / 'package.json').write_text(json.dumps(metadata))
            (directory / 'dist').mkdir()
            (directory / 'dist/stale.js').write_text('stale')
        return frontend

    def test_registry_auth_is_scoped_and_token_not_written(self):
        registry, config = release.registry_config('https://packages.example.test/repository/npm', 'secret')
        self.assertEqual(registry, 'https://packages.example.test/repository/npm/')
        self.assertIn('//packages.example.test/repository/npm/:_authToken=${ROUTEWEFT_NPM_TOKEN}', config)
        self.assertNotIn('secret', config)
        self.assertNotIn('_authToken', release.registry_config('https://registry.npmjs.org', '', False)[1])

    def test_invalid_configuration(self):
        for url in ['', 'http://packages.example.test', 'https://user:secret@packages.example.test',
                    'https://packages.example.test/?q=x', 'https://packages.example.test/\nregistry=x']:
            with self.assertRaises(ValueError):
                release.registry_config(url, 'secret')
        with self.assertRaises(ValueError):
            release.registry_config('https://packages.example.test', '')

    def test_all_archives_checked_before_upload_and_same_archives_uploaded_in_order(self):
        for execute in [False, True]:
            with self.subTest(execute=execute), tempfile.TemporaryDirectory() as source:
                frontend = self.fixture(source)
                calls, configs = [], []

                def run(command, cwd, env):
                    calls.append(command)
                    config = Path(env['NPM_CONFIG_USERCONFIG'])
                    configs.append(config)
                    self.assertEqual(config.stat().st_mode & 0o777, 0o600)
                    self.assertNotIn('npm_config_registry', env)
                    self.assertEqual(env['ROUTEWEFT_NPM_TOKEN'], 'secret')
                    if command[:2] == ['pnpm', 'install']:
                        self.assertFalse((cwd / 'packages/routing-client/dist').exists())
                    if command[1] == 'pack':
                        metadata = json.loads((cwd / 'package.json').read_text())
                        filename = cwd.name + '-' + metadata['version'] + '.tgz'
                        archive(cwd / filename, metadata)
                        return json.dumps([{'filename': filename}]).encode()
                    return b''

                with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                    'NPM_REGISTRY': 'https://packages.example.test/npm/',
                    'NPM_TOKEN': 'secret', 'npm_config_registry': 'wrong',
                }):
                    release.release(execute)
                publishes = [call for call in calls if call[1] == 'publish']
                self.assertEqual(len(publishes), 4 if execute else 2)
                self.assertTrue(all('--dry-run' in call for call in publishes[:2]))
                if execute:
                    self.assertEqual(publishes[0][:-1], publishes[2])
                    self.assertEqual(publishes[1][:-1], publishes[3])
                self.assertIn('routing-client-0.2.0', publishes[0][2])
                self.assertIn('maplibre-0.1.0', publishes[1][2])
                self.assertTrue(all(not config.exists() for config in configs))

    def test_failed_tests_prevent_publishing(self):
        with tempfile.TemporaryDirectory() as source:
            frontend = self.fixture(source)
            calls = []
            def run(command, cwd, env):
                calls.append(command)
                if command[:2] == ['pnpm', 'test']:
                    raise ValueError('test failed')
                return b''
            with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                'NPM_REGISTRY': 'https://packages.example.test', 'NPM_TOKEN': 'secret',
            }):
                with self.assertRaisesRegex(ValueError, 'test failed'):
                    release.release(True)
            self.assertFalse(any(call[1] in ('pack', 'publish') for call in calls))

    def test_second_preview_failure_prevents_all_uploads(self):
        with tempfile.TemporaryDirectory() as source:
            frontend = self.fixture(source)
            calls = []
            def run(command, cwd, env):
                calls.append(command)
                if command[1] == 'pack':
                    metadata = json.loads((cwd / 'package.json').read_text())
                    archive(cwd / 'package.tgz', metadata)
                    return b'[{"filename":"package.tgz"}]'
                if command[1] == 'publish' and 'maplibre' in command[2]:
                    raise ValueError('adapter preview failed')
                return b''
            with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                'NPM_REGISTRY': 'https://packages.example.test', 'NPM_TOKEN': 'secret',
            }):
                with self.assertRaisesRegex(ValueError, 'adapter preview failed'):
                    release.release(True)
            self.assertTrue(all('--dry-run' in call for call in calls if call[1] == 'publish'))

    def test_prerelease_defaults_to_next_and_single_package_selection(self):
        with tempfile.TemporaryDirectory() as source:
            frontend = self.fixture(source)
            path = frontend / 'packages/maplibre/package.json'
            metadata = json.loads(path.read_text())
            metadata['version'] = '0.2.0-alpha.1'
            path.write_text(json.dumps(metadata))
            calls = []
            def run(command, cwd, env):
                calls.append(command)
                if command[1] == 'pack':
                    archive(cwd / 'package.tgz', metadata)
                    return b'[{"filename":"package.tgz"}]'
                return b''
            with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                'NPM_REGISTRY': 'https://packages.example.test', 'NPM_TOKEN': '',
            }):
                release.release(packages=['maplibre'])
                publishes = [call for call in calls if call[1] == 'publish']
                self.assertEqual(len(publishes), 1)
                self.assertEqual(publishes[0][publishes[0].index('--tag') + 1], 'next')
                calls.clear()
                with self.assertRaisesRegex(ValueError, 'non-latest'):
                    release.release(packages=['maplibre'], tag='latest')
                self.assertEqual(calls, [])

    def test_versions_and_cross_package_dependency_are_validated(self):
        with tempfile.TemporaryDirectory() as source:
            frontend = self.fixture(source)
            path = frontend / 'packages/routing-client/package.json'
            metadata = json.loads(path.read_text())
            for version in ['latest', '1.0', '01.0.0', '1.0.0-01']:
                metadata['version'] = version
                path.write_text(json.dumps(metadata))
                with self.assertRaises(ValueError):
                    release.package_metadata(frontend)
            metadata['version'] = '0.2.1'
            path.write_text(json.dumps(metadata))
            with self.assertRaisesRegex(ValueError, 'exact routing-client'):
                release.package_metadata(frontend)
            for version in ['1.0.0', '1.0.0-alpha.1', '0.2.1+build.7']:
                self.assertIsNotNone(release.SEMVER.fullmatch(version))

    def test_archive_rejects_missing_types_and_local_dependencies(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'package.tgz'
            metadata = {'name': '@routeweft/maplibre', 'version': '0.1.0'}
            archive(path, metadata, missing='package/dist/index.d.ts')
            with self.assertRaisesRegex(ValueError, 'missing'):
                release.validate_archive(path, metadata)
            metadata['dependencies'] = {'@routeweft/routing-client': 'workspace:*'}
            archive(path, metadata)
            with self.assertRaisesRegex(ValueError, 'local-only'):
                release.validate_archive(path, metadata)

    def test_failure_diagnostics_redact_token_and_urls(self):
        process = type('Result', (), {'returncode': 1, 'stdout': b'failed https://private.example/npm secret-value'})()
        with patch.object(release.subprocess, 'run', return_value=process):
            with self.assertRaises(ValueError) as caught:
                release.run(['npm', 'publish'], Path('.'), {'NPM_TOKEN': 'secret-value'})
        self.assertNotIn('private.example', str(caught.exception))
        self.assertNotIn('secret-value', str(caught.exception))
        self.assertIn('failed', str(caught.exception))


if __name__ == '__main__':
    unittest.main()
