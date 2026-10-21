import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('frontend_release', Path(__file__).parents[1] / 'frontend-release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class FrontendReleaseTests(unittest.TestCase):
    def test_registry_auth_is_scoped_and_token_not_written(self):
        registry, config = release.registry_config('https://packages.example.test/repository/npm', 'secret')
        self.assertEqual(registry, 'https://packages.example.test/repository/npm/')
        self.assertIn('//packages.example.test/repository/npm/:_authToken=${NPM_TOKEN_INTERNAL}', config)
        self.assertNotIn('secret', config)

    def test_invalid_configuration(self):
        for url in ['', 'http://packages.example.test', 'https://user:secret@packages.example.test',
                    'https://packages.example.test/?q=x', 'https://packages.example.test/\nregistry=x']:
            with self.assertRaises(ValueError):
                release.registry_config(url, 'secret')
        with self.assertRaises(ValueError):
            release.registry_config('https://packages.example.test', '')

    def test_dry_run_and_upload_same_archive_and_cleanup(self):
        for execute in [False, True]:
            with self.subTest(execute=execute), tempfile.TemporaryDirectory() as source:
                package = Path(source)
                (package / 'package.json').write_text('{}')
                (package / 'dist').mkdir()
                (package / 'dist/stale.js').write_text('stale')
                calls = []
                configs = []

                def run(command, cwd, env):
                    calls.append(command)
                    config = Path(env['NPM_CONFIG_USERCONFIG'])
                    configs.append(config)
                    self.assertTrue(config.exists())
                    self.assertEqual(config.stat().st_mode & 0o777, 0o600)
                    self.assertNotIn('npm_config_registry', env)
                    if command[1] == 'ci':
                        self.assertFalse((cwd / 'dist').exists())
                    if command[1] == 'pack':
                        return b'[{"filename":"routing-client-0.1.0.tgz"}]'
                    return b''

                with patch.object(release, 'PACKAGE', package), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                    'NPM_REGISTRY_INTERNAL': 'https://packages.example.test/npm/',
                    'NPM_TOKEN_INTERNAL': 'secret', 'npm_config_registry': 'wrong',
                }):
                    release.release(execute)
                self.assertEqual([call[1] for call in calls], ['ci', 'test', 'pack', 'publish'] + (['publish'] if execute else []))
                self.assertIn('--dry-run', calls[3])
                if execute:
                    self.assertEqual(calls[3][:-1], calls[4])
                self.assertTrue(all(not config.exists() for config in configs))

    def test_failed_tests_prevent_publishing(self):
        with tempfile.TemporaryDirectory() as source:
            calls = []
            def run(command, cwd, env):
                calls.append(command[1])
                if command[1] == 'test':
                    raise ValueError('test failed')
                return b''
            with patch.object(release, 'PACKAGE', Path(source)), patch.object(release, 'run', side_effect=run), patch.dict(os.environ, {
                'NPM_REGISTRY_INTERNAL': 'https://packages.example.test', 'NPM_TOKEN_INTERNAL': 'secret',
            }):
                with self.assertRaises(ValueError):
                    release.release(True)
            self.assertEqual(calls, ['ci', 'test'])
