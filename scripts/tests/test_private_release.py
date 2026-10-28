import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from argparse import Namespace

spec = importlib.util.spec_from_file_location('release', Path(__file__).parents[1] / 'private-release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_load_local_config_without_overriding_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config'
            path.write_text("# local config\nexport CARGO_REGISTRIES_INTERNAL_INDEX='https://internal.invalid/index'\nCARGO_REGISTRIES_INTERNAL_TOKEN='local-token' # comment\n")
            with patch.dict(release.os.environ, {'CARGO_REGISTRIES_INTERNAL_TOKEN': 'existing'}, clear=True):
                release.load_config(path)
                self.assertEqual(release.os.environ['CARGO_REGISTRIES_INTERNAL_TOKEN'], 'existing')
                self.assertEqual(release.rules(None), [b'internal.invalid'])

    def test_config_values_are_not_executed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'config'
            marker = Path(directory) / 'executed'
            value = f'$(touch {marker})'
            path.write_text(f"CARGO_REGISTRIES_INTERNAL_TOKEN='{value}'\n")
            with patch.dict(release.os.environ, {}, clear=True):
                release.load_config(path)
                self.assertEqual(release.os.environ['CARGO_REGISTRIES_INTERNAL_TOKEN'], value)
            self.assertFalse(marker.exists())

    def test_missing_index_has_actionable_error(self):
        with patch.dict(release.os.environ, {}, clear=True):
            with self.assertRaisesRegex(release.ReleaseInputError, 'CARGO_REGISTRIES_INTERNAL_INDEX'):
                release.rules(None)

    def test_default_rules_come_from_index(self):
        with patch.dict(release.os.environ, {
                'CARGO_REGISTRIES_INTERNAL_INDEX': 'sparse+https://INTERNAL.invalid/index/'}, clear=True):
            self.assertEqual(release.rules(None), [b'internal.invalid'])

    def test_automatic_release_orders_packages_and_cleans_up(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare') as prepare, \
                patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=False))
            self.assertEqual([call.args[0].package for call in publish.call_args_list], [None])
            self.assertTrue(all(not call.args[0].execute for call in publish.call_args_list))
            self.assertIsNone(prepare.call_args.args[0].license_file)
            self.assertFalse(prepare.call_args.args[0].output.parent.exists())

    def test_automatic_release_cleans_up_on_failure(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare') as prepare, \
                patch.object(release, 'publish', side_effect=ValueError('failed')) as publish:
            with self.assertRaises(ValueError):
                release.release(Namespace(ref='HEAD', package=None, execute=True))
            self.assertEqual(publish.call_count, 1)
            self.assertFalse(prepare.call_args.args[0].output.parent.exists())

    def test_case_insensitive_check(self):
        with self.assertRaises(ValueError):
            release.inspect_bytes(b'https://INTERNAL.invalid/path', [b'internal.invalid'], 'fixture')

    def test_archive_checks_uncompressed_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'test.crate'
            with tarfile.open(path, 'w:gz') as archive:
                data = b'https://internal.invalid/index'
                member = tarfile.TarInfo('test/src/lib.rs')
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            with self.assertRaises(ValueError):
                release.scan_archive(path, [b'internal.invalid'], 'https://internal.invalid/index')

    def test_prepare_transforms_only_private_copy(self):
        # Use real tracked manifests, but substitute Git's snapshot with a local fixture.
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w') as archive:
            manifests = [release.ROOT / 'backend/Cargo.toml']
            manifests += [release.ROOT / 'backend/crates' / name / 'Cargo.toml'
                          for name in (*release.ORDER, 'routeweft-daemon')]
            for file in manifests:
                archive.add(file, arcname=str(file.relative_to(release.ROOT)))
            for name in ('LICENSES', 'THIRD_PARTY_NOTICES.md',
                         'docs/dependency-licenses.json', 'docs/data-sources.md'):
                archive.add(release.ROOT / name, arcname=name)
        snapshot = buffer.getvalue()
        before = (release.ROOT / 'backend/Cargo.toml').read_bytes()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            patterns = root / 'patterns'
            patterns.write_text('internal.invalid\n')
            license_file = root / 'license'
            license_file.write_text('TEST FIXTURE ONLY - NOT A PROJECT LICENSE\n')
            args = Namespace(ref='test', output=root / 'stage', patterns=patterns, license_file=license_file)
            with patch.object(release, 'git', side_effect=[b'a' * 40 + b'\n', snapshot]):
                release.prepare(args)
            manifest = release.tomllib.loads((args.output / 'backend/Cargo.toml').read_text())
            self.assertEqual(manifest['workspace']['package']['publish'], ['internal'])
            self.assertEqual(manifest['workspace']['dependencies']['routeweft-domain']['registry'], 'internal')
            daemon = release.tomllib.loads((args.output / 'backend/crates/routeweft-daemon/Cargo.toml').read_text())
            self.assertFalse(daemon['package']['publish'])
            self.assertEqual(before, (release.ROOT / 'backend/Cargo.toml').read_bytes())
            args.output = root / 'without-license'
            args.license_file = None
            with patch.object(release, 'git', side_effect=[b'a' * 40 + b'\n', snapshot]):
                release.prepare(args)
            crate_manifest = release.tomllib.loads(
                (args.output / 'backend/crates/routeweft-domain/Cargo.toml').read_text())
            self.assertNotIn('license-file', crate_manifest['package'])
            self.assertFalse((args.output / 'backend/crates/routeweft-domain/LICENSE').exists())

    def test_missing_target_fails_before_cargo(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'internal'}))
            with patch.dict(release.os.environ, {}, clear=True), patch.object(release.subprocess, 'run') as run:
                with self.assertRaises(ValueError):
                    release.publish(Namespace(stage=stage))
                run.assert_not_called()

    def test_execute_checks_archive_before_upload(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory) / 'stage'
            stage.mkdir()
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'internal', 'version': '0.1.0'}))
            archive = stage / 'backend/target/package/routeweft-domain-0.1.0.crate'
            archive.parent.mkdir(parents=True)
            archive.touch()
            patterns = Path(directory) / 'patterns'
            patterns.write_text('internal.invalid\n')
            env = {'CARGO_HOME': str(Path(directory) / 'cargo'),
                   'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
                   'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture-token'}
            args = Namespace(stage=stage, patterns=patterns, package='routeweft-domain', execute=True)
            with patch.dict(release.os.environ, env, clear=True), \
                    patch.object(release, 'run_cargo') as run, \
                    patch.object(release, 'scan_archive', side_effect=ValueError('blocked')):
                with self.assertRaises(ValueError):
                    release.publish(args)
                self.assertEqual(run.call_count, 1)
                self.assertIn('--dry-run', run.call_args.args[0])

    def test_execute_retains_dependency_order(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare'), patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=True))
        self.assertEqual([call.args[0].package for call in publish.call_args_list], list(release.ORDER))
        self.assertTrue(all(call.args[0].execute for call in publish.call_args_list))

    def test_unborn_head_has_actionable_error(self):
        import subprocess
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(['git', 'init', str(repo)], check=True, capture_output=True)
            with patch.object(release, 'ROOT', repo):
                with self.assertRaisesRegex(release.ReleaseInputError, '先在本仓库提交'):
                    release.resolve_commit('HEAD')
                with self.assertRaisesRegex(release.ReleaseInputError, '--ref'):
                    release.resolve_commit('missing-tag')

    def test_package_archive_supports_cargo_layouts(self):
        for layout in ('', 'tmp-crate', 'tmp-registry'):
            with self.subTest(layout=layout), tempfile.TemporaryDirectory() as directory:
                stage = Path(directory)
                archive = stage / 'target/package' / layout / 'routeweft-domain-0.1.0.crate'
                archive.parent.mkdir(parents=True)
                archive.write_bytes(b'archive')
                self.assertEqual(release.package_archive(stage, 'routeweft-domain', '0.1.0'), archive)

    def test_inconsistent_archives_stop_release(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            for layout in ('tmp-crate', 'tmp-registry'):
                archive = stage / 'target/package' / layout / 'routeweft-domain-0.1.0.crate'
                archive.parent.mkdir(parents=True)
                archive.write_text(layout)
            with self.assertRaises(release.ReleaseInputError):
                release.package_archive(stage, 'routeweft-domain', '0.1.0')

    def test_cargo_failure_redacts_credentials_and_url(self):
        import contextlib
        import sys
        output = io.StringIO()
        env = {'CARGO_REGISTRIES_INTERNAL_TOKEN': 'private-token'}
        command = [sys.executable, '-c',
                   "print('error: private-token https://internal.invalid/index unavailable'); raise SystemExit(1)"]
        with contextlib.redirect_stdout(output), self.assertRaises(release.ReleaseInputError):
            release.run_cargo(command, Path('/tmp'), env)
        self.assertNotIn('private-token', output.getvalue())
        self.assertNotIn('internal.invalid', output.getvalue())
        self.assertIn('unavailable', output.getvalue())

    def test_live_output_and_silent_heartbeat(self):
        import contextlib
        import sys
        output = io.StringIO()
        command = [sys.executable, '-u', '-c',
                   "import time; print('Compiling fixture'); time.sleep(0.2); print('Finished fixture')"]
        with contextlib.redirect_stdout(output):
            release.run_cargo(command, Path('/tmp'), {}, heartbeat_seconds=0.05)
        value = output.getvalue()
        self.assertLess(value.index('Compiling fixture'), value.index('仍在运行'))
        self.assertLess(value.index('仍在运行'), value.index('Finished fixture'))

    def test_workspace_dry_run_checks_every_archive_without_upload(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'internal', 'version': '0.1.0'}))
            env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
                   'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
            args = Namespace(stage=stage, patterns=None, package=None, execute=False)
            with patch.dict(release.os.environ, env, clear=True), \
                    patch.object(release, 'run_cargo') as run, \
                    patch.object(release, 'package_archive', return_value=stage / 'fixture.crate') as archive, \
                    patch.object(release, 'scan_archive') as scan:
                release.publish(args)
            run.assert_called_once()
            self.assertEqual(run.call_args.args[0],
                             ['cargo', 'publish', '--registry', 'internal', '--workspace', '--dry-run'])
            self.assertEqual([call.args[1] for call in archive.call_args_list], list(release.ORDER))
            self.assertEqual(scan.call_count, len(release.ORDER))
            self.assertEqual(run.call_args.args[1], stage / 'backend')
            self.assertEqual(archive.call_args.args[0], stage / 'backend')
            self.assertEqual(run.call_args.args[2]['CARGO_TARGET_DIR'], str(stage / 'backend/target'))


if __name__ == '__main__':
    unittest.main()
