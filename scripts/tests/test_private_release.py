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
            self.assertEqual([call.args[0].package for call in publish.call_args_list], list(release.ORDER))
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
                    patch.object(release.subprocess, 'run', return_value=Namespace(returncode=0)) as run, \
                    patch.object(release, 'scan_archive', side_effect=ValueError('blocked')):
                with self.assertRaises(ValueError):
                    release.publish(args)
                self.assertEqual(run.call_count, 1)
                self.assertIn('--dry-run', run.call_args.args[0])


if __name__ == '__main__':
    unittest.main()
