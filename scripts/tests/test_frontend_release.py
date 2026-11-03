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
        (frontend / 'package.json').write_text(json.dumps({'name': '@routeweft/frontend', 'version': '0.2.0', 'private': True}))
        for name, version in [('routing-client', '0.2.0'), ('maplibre', '0.2.0')]:
            directory = frontend / 'packages' / name
            directory.mkdir(parents=True)
            metadata = {'name': '@routeweft/' + name, 'version': version}
            if name == 'maplibre':
                metadata['dependencies'] = {'@routeweft/routing-client': '0.2.0'}
            (directory / 'package.json').write_text(json.dumps(metadata))
            (directory / 'dist').mkdir()
            (directory / 'dist/stale.js').write_text('stale')
        (frontend / 'packages/routing-client/package-lock.json').write_text(json.dumps({
            'name': '@routeweft/routing-client', 'version': '0.2.0', 'lockfileVersion': 3,
            'packages': {'': {'version': '0.2.0'}}}))
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
                    content = config.read_text()
                    if command[0] == 'pnpm':
                        self.assertNotIn('ROUTEWEFT_NPM_TOKEN', env)
                        self.assertNotIn('NPM_TOKEN', env)
                        self.assertNotIn('packages.example.test', content)
                        self.assertIn('registry=https://registry.npmjs.org/', content)
                        self.assertNotIn('_authToken', content)
                    else:
                        self.assertEqual(env['ROUTEWEFT_NPM_TOKEN'], 'secret')
                        self.assertIn('registry=https://packages.example.test/npm/', content)
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
                self.assertIn('maplibre-0.2.0', publishes[1][2])
                self.assertTrue(all(not config.exists() for config in configs))

    def test_install_registry_and_read_token_are_independent_and_cli_overrides_env(self):
        for override in (None, 'https://cli.example.test/group'):
            with self.subTest(override=override), tempfile.TemporaryDirectory() as source:
                frontend = self.fixture(source)
                observed = []

                def run(command, cwd, env):
                    config = Path(env['NPM_CONFIG_USERCONFIG']).read_text()
                    if command[0] == 'pnpm':
                        expected = override or 'https://read.example.test/group'
                        self.assertIn(f'registry={expected}/', config)
                        self.assertIn('_authToken=${ROUTEWEFT_NPM_INSTALL_TOKEN}', config)
                        self.assertEqual(env['ROUTEWEFT_NPM_INSTALL_TOKEN'], 'read-secret')
                        self.assertNotIn('ROUTEWEFT_NPM_TOKEN', env)
                        self.assertNotIn('write.example.test', config)
                        observed.append('install')
                    if command[1] == 'publish':
                        self.assertEqual(command[command.index('--registry') + 1], 'https://write.example.test/hosted/')
                        self.assertIn('registry=https://write.example.test/hosted/', config)
                        self.assertIn('_authToken=${ROUTEWEFT_NPM_TOKEN}', config)
                        self.assertEqual(env['ROUTEWEFT_NPM_TOKEN'], 'write-secret')
                        observed.append('publish')
                    if command[1] == 'pack':
                        metadata = json.loads((cwd / 'package.json').read_text())
                        archive(cwd / 'package.tgz', metadata)
                        return b'[{"filename":"package.tgz"}]'
                    self.assertNotIn('read-secret', config)
                    self.assertNotIn('write-secret', config)
                    return b''

                with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), \
                        patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                            'NPM_REGISTRY': 'https://write.example.test/hosted', 'NPM_TOKEN': 'write-secret',
                            'NPM_INSTALL_REGISTRY': 'https://read.example.test/group', 'NPM_INSTALL_TOKEN': 'read-secret',
                        }, clear=True):
                    release.release(install_registry=override)
                self.assertIn('install', observed)
                self.assertEqual(observed.count('publish'), 2)

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
            for changed, content in release.frontend_version.changes(frontend, '0.2.0-alpha.1').items():
                changed.write_text(content)
            metadata = json.loads(path.read_text())
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
            with self.assertRaisesRegex(ValueError, 'Frontend versions differ'):
                release.package_metadata(frontend)
            for version in ['1.0.0', '1.0.0-alpha.1', '0.2.1+build.7']:
                self.assertIsNotNone(release.SEMVER.fullmatch(version))

    def test_unified_version_updates_lock_and_internal_dependency_without_changing_resolutions(self):
        with tempfile.TemporaryDirectory() as source:
            frontend = self.fixture(source)
            lock_path = frontend / 'packages/routing-client/package-lock.json'
            lock = json.loads(lock_path.read_text())
            lock['packages']['node_modules/typescript'] = {'version': '5.9.3', 'integrity': 'unchanged'}
            lock_path.write_text(json.dumps(lock))
            for path, content in release.frontend_version.changes(frontend, '1.2.3-beta.2').items():
                path.write_text(content)
            self.assertEqual(release.frontend_version.check(frontend), '1.2.3-beta.2')
            metadata = release.package_metadata(frontend)
            self.assertEqual({d['version'] for d in metadata.values()}, {'1.2.3-beta.2'})
            self.assertEqual(metadata['maplibre']['dependencies']['@routeweft/routing-client'], '1.2.3-beta.2')
            updated = json.loads(lock_path.read_text())
            self.assertEqual(updated['version'], '1.2.3-beta.2')
            self.assertEqual(updated['packages']['']['version'], '1.2.3-beta.2')
            self.assertEqual(updated['packages']['node_modules/typescript'], lock['packages']['node_modules/typescript'])
            self.assertEqual(release.frontend_version.changes(frontend), {})
            before = lock_path.read_bytes()
            with self.assertRaises(ValueError):
                release.frontend_version.changes(frontend, '01.2.3')
            self.assertEqual(lock_path.read_bytes(), before)

    def test_version_drift_blocks_release_before_dependency_install(self):
        for relative in ('package.json', 'packages/maplibre/package.json', 'packages/routing-client/package-lock.json'):
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as source:
                frontend = self.fixture(source)
                path = frontend / relative
                data = json.loads(path.read_text())
                data['version'] = '9.0.0'
                path.write_text(json.dumps(data))
                with patch.object(release, 'FRONTEND', frontend), patch.object(release, 'load_release_env'), \
                        patch.object(release, 'run') as run, patch.dict(os.environ, {
                            'NPM_REGISTRY': 'https://write.example.test/', 'NPM_TOKEN': 'secret',
                        }, clear=True):
                    with self.assertRaisesRegex(ValueError, 'Frontend versions differ'):
                        release.release()
                run.assert_not_called()

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
