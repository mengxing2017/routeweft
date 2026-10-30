import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from argparse import Namespace

spec = importlib.util.spec_from_file_location('release', Path(__file__).parents[1] / 'release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def staged_source(args):
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / 'release-source.json').write_text(release.json.dumps({'packages': list(release.ORDER)}))


class ReleaseTests(unittest.TestCase):
    def test_local_config_loads_quotes_comments_and_preserves_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.private-release.env').write_text(
                "# local configuration\n\n"
                "export CARGO_REGISTRIES_INTERNAL_INDEX='https://private.invalid/index' # index\n"
                'CARGO_REGISTRIES_INTERNAL_TOKEN="token with spaces # literal"\n'
                "CARGO_REGISTRY_TOKEN='file-token'\n")
            with patch.object(release, 'ROOT', root), patch.dict(
                    release.os.environ, {'CARGO_REGISTRY_TOKEN': 'exported-token'}, clear=True):
                release.load_release_env()
                self.assertEqual(release.registry_index('internal'), 'https://private.invalid/index')
                self.assertEqual(release.os.environ['CARGO_REGISTRIES_INTERNAL_TOKEN'],
                                 'token with spaces # literal')
                self.assertEqual(release.os.environ['CARGO_REGISTRY_TOKEN'], 'exported-token')

    def test_config_is_not_executed_or_expanded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root / 'must-not-exist'
            value = f'$(touch {marker}) $HOME `id`'
            (root / '.private-release.env').write_text(f'CARGO_REGISTRY_TOKEN="{value}"\n')
            with patch.object(release, 'ROOT', root), patch.dict(release.os.environ, {}, clear=True):
                release.load_release_env()
                self.assertEqual(release.os.environ['CARGO_REGISTRY_TOKEN'], value)
            self.assertFalse(marker.exists())

    def test_invalid_config_does_not_leak_values_or_partially_load(self):
        for invalid in ("CARGO_REGISTRY_TOKEN='secret", 'CARGO_REGISTRY_TOKEN=secret extra',
                        'UNSUPPORTED=secret'):
            with self.subTest(invalid=invalid), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / '.private-release.env').write_text(
                    'CARGO_REGISTRIES_INTERNAL_TOKEN=first-secret\n' + invalid)
                with patch.object(release, 'ROOT', root), patch.dict(release.os.environ, {}, clear=True):
                    with self.assertRaises(release.ReleaseInputError) as error:
                        release.load_release_env()
                    self.assertIn('2', str(error.exception))
                    self.assertNotIn('secret', str(error.exception))
                    self.assertEqual(dict(release.os.environ), {})

    def test_missing_config_keeps_environment(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(release, 'ROOT', Path(directory)), \
                patch.dict(release.os.environ, {'CARGO_REGISTRY_TOKEN': ''}, clear=True):
            release.load_release_env()
            self.assertEqual(dict(release.os.environ), {'CARGO_REGISTRY_TOKEN': ''})

    def test_cli_loads_config_before_registry_validation(self):
        import sys
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.private-release.env').write_text(
                'CARGO_REGISTRIES_INTERNAL_INDEX=https://private.invalid/index\n'
                'CARGO_REGISTRIES_INTERNAL_TOKEN=fixture-token\n')
            def check_config(args):
                self.assertEqual(release.registry_index(args.registry), 'https://private.invalid/index')
                release.require_token(args.registry)
            with patch.object(release, 'ROOT', root), \
                    patch.dict(release.os.environ, {}, clear=True), \
                    patch.object(sys, 'argv', ['release.py', 'check', '--registry', 'internal']), \
                    patch.object(release, 'check', side_effect=check_config) as check:
                release.main()
                check.assert_called_once()

    def test_private_rules_come_from_selected_index(self):
        with patch.dict(release.os.environ, {
                'CARGO_REGISTRIES_INTERNAL_INDEX': 'sparse+https://INTERNAL.invalid/index/'}, clear=True):
            self.assertEqual(release.rules(None, 'internal'), [b'internal.invalid'])

    def test_missing_registry_never_falls_back_to_internal(self):
        with patch.object(release, 'prepare', side_effect=staged_source) as prepare:
            for registries in (None, []):
                with self.subTest(registries=registries), self.assertRaisesRegex(
                        release.ReleaseInputError, '--registry crates-io'):
                    release.release(Namespace(registry=registries))
            prepare.assert_not_called()
        with self.assertRaisesRegex(release.ReleaseInputError, '--registry crates-io'):
            release.registry_name(Namespace())

    def test_cli_requires_explicit_registry(self):
        import subprocess
        import sys
        script = release.ROOT / 'scripts/release.py'
        for command in ('check', 'release'):
            result = subprocess.run([sys.executable, str(script), command],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn('--registry', result.stderr)
            self.assertNotIn('CARGO_REGISTRIES_INTERNAL_INDEX', result.stderr)

    def test_make_requires_explicit_registry(self):
        import subprocess
        for target in ('release-check', 'release-dry-run', 'release-publish'):
            result = subprocess.run(['make', target, 'REGISTRIES='], cwd=release.ROOT,
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('REGISTRIES=crates-io', result.stderr)
            self.assertNotIn('python3 scripts/release.py', result.stdout)

    def test_automatic_release_orders_packages_and_cleans_up(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare, \
                patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=False, registry=['internal']))
            self.assertEqual([call.args[0].package for call in publish.call_args_list], [None])
            self.assertTrue(all(not call.args[0].execute for call in publish.call_args_list))
            self.assertIsNone(prepare.call_args.args[0].license_file)
            self.assertFalse(prepare.call_args.args[0].output.parent.exists())

    def test_execute_retains_dependency_order(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare', side_effect=staged_source), patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=True, registry=['internal']))
        self.assertEqual([call.args[0].package for call in publish.call_args_list], list(release.ORDER))
        self.assertTrue(all(call.args[0].execute for call in publish.call_args_list))

    def test_automatic_release_cleans_up_on_failure(self):
        env = {'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://internal.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'fixture'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare, \
                patch.object(release, 'publish', side_effect=ValueError('failed')) as publish:
            with self.assertRaises(ValueError):
                release.release(Namespace(ref='HEAD', package=None, execute=True, registry=['internal']))
            self.assertEqual(publish.call_count, 1)
            self.assertFalse(prepare.call_args.args[0].output.parent.exists())

    def test_multiple_registries_have_isolated_stages_and_dependency_order(self):
        env = {f'CARGO_REGISTRIES_{name}_{suffix}': value
               for name in ('EAST', 'WEST')
               for suffix, value in (('INDEX', f'https://{name.lower()}.invalid/index'),
                                     ('TOKEN', name.lower() + '-token'))}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'resolve_commit', return_value='a' * 40) as resolve, \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare, \
                patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=True,
                                      registry=['east', 'west', 'east']))
        self.assertEqual([(c.args[0].registry, c.args[0].package)
                          for c in publish.call_args_list],
                         [(r, p) for r in ('east', 'west') for p in release.ORDER])
        resolve.assert_called_once_with('HEAD')
        self.assertTrue(all(c.args[0].ref == 'a' * 40 for c in prepare.call_args_list))
        stages = [c.args[0].output for c in prepare.call_args_list]
        self.assertEqual(len(set(stages)), 2)
        self.assertTrue(all(not stage.exists() for stage in stages))

    def test_all_registries_validated_before_upload(self):
        env = {'CARGO_REGISTRIES_EAST_INDEX': 'https://east.invalid/index',
               'CARGO_REGISTRIES_EAST_TOKEN': 'fixture',
               'CARGO_REGISTRIES_WEST_INDEX': 'https://west.invalid/index'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare:
            with self.assertRaisesRegex(release.ReleaseInputError, 'WEST_TOKEN'):
                release.release(Namespace(registry=['east', 'west'], execute=True))
            prepare.assert_not_called()

    def test_registry_failure_stops_later_destinations(self):
        env = {f'CARGO_REGISTRIES_{name}_{suffix}': value
               for name in ('EAST', 'WEST')
               for suffix, value in (('INDEX', f'https://{name.lower()}.invalid/index'),
                                     ('TOKEN', 'fixture'))}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare, \
                patch.object(release, 'publish', side_effect=release.ReleaseInputError('failed')) as publish:
            with self.assertRaises(release.ReleaseInputError):
                release.release(Namespace(ref='HEAD', package=None, execute=True,
                                          registry=['east', 'west']))
        self.assertEqual(publish.call_count, 1)
        self.assertEqual(prepare.call_args.args[0].registry, 'east')
        self.assertFalse(prepare.call_args.args[0].output.exists())

    def test_public_registry_uses_standard_cargo_configuration(self):
        with patch.dict(release.os.environ, {'CARGO_REGISTRY_TOKEN': 'public-token'}, clear=True):
            self.assertEqual(release.registry_name(Namespace(registry='crates-io')), 'crates-io')
            self.assertEqual(release.registry_index('crates-io'), 'sparse+https://index.crates.io/')
            release.require_token('crates-io')
            self.assertEqual(release.rules(None, 'crates-io'), [])
        with patch.dict(release.os.environ, {
                'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://private.invalid/index'}, clear=True):
            self.assertEqual(release.rules(None, 'crates-io'), [b'private.invalid'])

    def test_public_publish_uses_crates_io_not_private_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'crates-io', 'version': '0.1.0'}))
            with patch.dict(release.os.environ, {'CARGO_REGISTRY_TOKEN': 'public-token'}, clear=True), \
                    patch.object(release, 'run_cargo') as run, \
                    patch.object(release, 'package_archive', return_value=stage / 'fixture.crate'), \
                    patch.object(release, 'scan_archive'):
                release.publish(Namespace(stage=stage, registry='crates-io', patterns=None,
                                          package='routeweft-domain', execute=True))
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args.args[0],
                             ['cargo', 'publish', '--registry', 'crates-io', '-p', 'routeweft-domain'])
            self.assertEqual(run.call_args.args[2]['CARGO_REGISTRY_CREDENTIAL_PROVIDER'], 'cargo:token')
            self.assertNotIn('CARGO_REGISTRIES_INTERNAL_CREDENTIAL_PROVIDER', run.call_args.args[2])

    def test_public_and_private_release_keep_both_targets(self):
        env = {'CARGO_REGISTRY_TOKEN': 'public-token',
               'CARGO_REGISTRIES_INTERNAL_INDEX': 'https://private.invalid/index',
               'CARGO_REGISTRIES_INTERNAL_TOKEN': 'private-token'}
        with patch.dict(release.os.environ, env, clear=True), \
                patch.object(release, 'resolve_commit', return_value='a' * 40), \
                patch.object(release, 'wait_for_publish') as wait, \
                patch.object(release, 'prepare', side_effect=staged_source) as prepare, \
                patch.object(release, 'publish') as publish:
            release.release(Namespace(ref='HEAD', package=None, execute=True,
                                      registry=['crates-io', 'internal']))
        self.assertEqual([(c.args[0].registry, c.args[0].package)
                          for c in publish.call_args_list],
                         [(r, p) for r in ('crates-io', 'internal') for p in release.ORDER])
        self.assertEqual(len({c.args[0].output for c in prepare.call_args_list}), 2)
        self.assertEqual(wait.call_count, len(release.ORDER) - 1)
        self.assertTrue(all(call.args[0] == 60 for call in wait.call_args_list))

    def test_registry_names_reject_ambiguous_environment_mapping(self):
        for name in ('East', 'east-west', '../east', 'east;west', '1east'):
            with self.subTest(name=name), self.assertRaises(release.ReleaseInputError):
                release.registry_name(Namespace(registry=name))

    def test_staging_cannot_be_published_to_different_registry(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'east'}))
            with patch.object(release, 'run_cargo') as run:
                with self.assertRaises(ValueError):
                    release.publish(Namespace(stage=stage, registry='west'))
                run.assert_not_called()

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
            args = Namespace(ref='test', output=root / 'stage', patterns=patterns, license_file=license_file, registry='internal')
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

    def test_real_public_snapshot_uses_committed_version_and_default_registry(self):
        committed = release.tomllib.loads(release.git('show', 'HEAD:backend/Cargo.toml').decode())
        with tempfile.TemporaryDirectory() as directory, patch.dict(release.os.environ, {}, clear=True):
            stage = Path(directory) / 'stage'
            release.prepare(Namespace(ref='HEAD', output=stage, patterns=None,
                                      license_file=None, registry='crates-io'))
            workspace = release.tomllib.loads((stage / 'backend/Cargo.toml').read_text())['workspace']
            version = committed['workspace']['package']['version']
            self.assertEqual(workspace['package']['version'], version)
            self.assertEqual(workspace['package']['publish'], ['crates-io'])
            for name in (name for name in release.ORDER if name in committed['workspace']['dependencies']):
                dep = workspace['dependencies'][name]
                self.assertEqual(dep['version'], '=' + version)
                self.assertNotIn('registry', dep)
                crate = stage / 'backend/crates' / name
                self.assertTrue((crate / 'THIRD_PARTY_NOTICES.md').is_file())
                self.assertTrue((crate / 'docs/data-sources.md').is_file())
            daemon = release.tomllib.loads((stage / 'backend/crates/routeweft-daemon/Cargo.toml').read_text())
            self.assertFalse(daemon['package']['publish'])

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

    def test_cargo_429_extracts_server_deadline_without_leaking_logs(self):
        import contextlib
        import sys
        from datetime import datetime, timezone
        output = io.StringIO()
        env = {'CARGO_REGISTRY_TOKEN': 'private-token'}
        message = ('status 429 Too Many Requests: private-token https://private.invalid; '
                   'Please try again after Thu, 29 Oct 2026 03:00:14 GMT')
        command = [sys.executable, '-c', f'print({message!r}); raise SystemExit(1)']
        with contextlib.redirect_stdout(output), self.assertRaises(release.CargoRateLimitError) as error:
            release.run_cargo(command, Path('/tmp'), env)
        self.assertEqual(error.exception.retry_at,
                         datetime(2026, 10, 29, 3, 0, 14, tzinfo=timezone.utc).timestamp())
        self.assertNotIn('private-token', output.getvalue() + str(error.exception))
        self.assertNotIn('private.invalid', output.getvalue() + str(error.exception))

    def test_upload_retry_preserves_dry_run_and_archive_scan_order(self):
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / 'release-source.json').write_text(release.json.dumps({
                'packages': list(release.ORDER), 'registry': 'crates-io', 'version': '0.1.0'}))
            events = []
            uploads = [0]
            def cargo(command, *args):
                if '--dry-run' in command:
                    events.append('dry-run')
                    return
                events.append('upload')
                uploads[0] += 1
                if uploads[0] == 1:
                    raise release.CargoRateLimitError()
            with patch.dict(release.os.environ, {'CARGO_REGISTRY_TOKEN': 'fixture'}, clear=True), \
                    patch.object(release, 'run_cargo', side_effect=cargo), \
                    patch.object(release, 'package_archive', return_value=stage / 'fixture.crate'), \
                    patch.object(release, 'scan_archive', side_effect=lambda *a, **k: events.append('scan')), \
                    patch.object(release, 'wait_for_publish', side_effect=lambda *a: events.append('wait')):
                release.publish(Namespace(stage=stage, registry='crates-io', patterns=None,
                                          package='routeweft-routing', execute=True, max_rate_limit_retries=1))
            self.assertEqual(events, ['dry-run', 'scan', 'upload', 'wait', 'upload'])

    def test_cli_rejects_invalid_wait_and_retry_settings(self):
        import subprocess
        import sys
        for option, value in (('--publish-interval', '-1'),
                              ('--max-rate-limit-retries', '-1'),
                              ('--publish-interval', '1.5')):
            result = subprocess.run([sys.executable, str(release.ROOT / 'scripts/release.py'),
                                     'release', '--registry', 'crates-io', option, value],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn('非负整数', result.stderr)

    def test_retry_after_seconds_date_and_invalid_values(self):
        from datetime import datetime, timezone
        with patch.object(release.time, 'time', return_value=1000):
            self.assertEqual(release.retry_timestamp('Retry-After: 120'), 1120)
        self.assertEqual(release.retry_timestamp('Retry-After: Thu, 29 Oct 2026 03:00:14 GMT'),
                         datetime(2026, 10, 29, 3, 0, 14, tzinfo=timezone.utc).timestamp())
        for value in ('Please try again after invalid GMT', 'Retry-After: unknown', 'status 400'):
            self.assertIsNone(release.retry_timestamp(value))

    def test_public_upload_retries_same_command_after_server_deadline(self):
        command = ['cargo', 'publish', '--registry', 'crates-io', '-p', 'routeweft-routing']
        with patch.object(release, 'run_cargo', side_effect=[release.CargoRateLimitError(1100), None]) as run, \
                patch.object(release.time, 'time', return_value=1000), \
                patch.object(release, 'wait_for_publish') as wait:
            release.run_publish(command, Path('/tmp'), {}, 'crates-io')
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0], run.call_args_list[1])
        self.assertEqual(wait.call_args.args[0], 101)

    def test_unknown_deadline_backs_off_and_stops_at_retry_limit(self):
        with patch.object(release, 'run_cargo', side_effect=release.CargoRateLimitError()) as run, \
                patch.object(release, 'wait_for_publish') as wait:
            with self.assertRaisesRegex(release.ReleaseInputError, '上限（2 次）'):
                release.run_publish(['cargo'], Path('/tmp'), {}, 'crates-io', max_retries=2)
        self.assertEqual(run.call_count, 3)
        self.assertEqual([call.args[0] for call in wait.call_args_list], [60, 120])

    def test_expired_server_deadline_does_not_cause_negative_sleep(self):
        with patch.object(release, 'run_cargo', side_effect=[release.CargoRateLimitError(900), None]), \
                patch.object(release.time, 'time', return_value=1000), \
                patch.object(release, 'wait_for_publish') as wait:
            release.run_publish(['cargo'], Path('/tmp'), {}, 'crates-io')
        self.assertEqual(wait.call_args.args[0], 1)

    def test_private_upload_and_other_errors_never_retry(self):
        for registry, error, retries in (
                ('internal', release.CargoRateLimitError(), 3),
                ('crates-io', release.ReleaseInputError('status 400'), 3),
                ('crates-io', release.CargoRateLimitError(), 0)):
            with self.subTest(registry=registry, retries=retries), \
                    patch.object(release, 'run_cargo', side_effect=error) as run, \
                    patch.object(release, 'wait_for_publish') as wait:
                with self.assertRaises(release.ReleaseInputError):
                    release.run_publish(['cargo'], Path('/tmp'), {}, registry, retries)
                run.assert_called_once()
                wait.assert_not_called()

    def test_retry_wait_is_cancellable_without_another_upload(self):
        with patch.object(release, 'run_cargo', side_effect=release.CargoRateLimitError()) as run, \
                patch.object(release, 'wait_for_publish', side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                release.run_publish(['cargo'], Path('/tmp'), {}, 'crates-io')
            run.assert_called_once()

    def test_wait_reports_progress_in_short_chunks(self):
        import contextlib
        now = [100.0]
        def sleep(seconds):
            now[0] += seconds
        output = io.StringIO()
        with patch.object(release.time, 'monotonic', side_effect=lambda: now[0]), \
                patch.object(release.time, 'sleep', side_effect=sleep) as pause, \
                contextlib.redirect_stdout(output):
            release.wait_for_publish(65, 'fixture')
        self.assertEqual([call.args[0] for call in pause.call_args_list], [30, 30, 5])
        self.assertIn('65s', output.getvalue())
        self.assertIn('5s', output.getvalue())

    def test_public_interval_is_only_between_successful_uploads(self):
        with patch.object(release, 'prepare', side_effect=staged_source), \
                patch.object(release, 'publish', side_effect=[None, release.ReleaseInputError('failed')]) as publish, \
                patch.object(release, 'wait_for_publish') as wait:
            with self.assertRaises(release.ReleaseInputError):
                release.release_registry(Namespace(ref='HEAD', package=None, execute=True,
                                                   publish_interval=120), 'crates-io')
        self.assertEqual(publish.call_count, 2)
        wait.assert_called_once_with(120, 'crates.io 包间发布间隔')

    def test_private_dry_run_and_single_package_skip_interval(self):
        for registry, execute, package in (('internal', True, None),
                                           ('crates-io', False, None),
                                           ('crates-io', True, 'routeweft-domain')):
            with self.subTest(registry=registry, execute=execute, package=package), \
                    patch.object(release, 'prepare', side_effect=staged_source), patch.object(release, 'publish'), \
                    patch.object(release, 'wait_for_publish') as wait:
                release.release_registry(Namespace(ref='HEAD', package=package, execute=execute), registry)
                wait.assert_not_called()

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
                'packages': list(release.ORDER), 'registry': 'east', 'version': '0.1.0'}))
            env = {'CARGO_REGISTRIES_EAST_INDEX': 'https://internal.invalid/index',
                   'CARGO_REGISTRIES_EAST_TOKEN': 'fixture'}
            args = Namespace(stage=stage, patterns=None, package=None, execute=False, registry='east')
            with patch.dict(release.os.environ, env, clear=True), \
                    patch.object(release, 'run_cargo') as run, \
                    patch.object(release, 'package_archive', return_value=stage / 'fixture.crate') as archive, \
                    patch.object(release, 'scan_archive') as scan:
                release.publish(args)
            run.assert_called_once()
            self.assertEqual(run.call_args.args[2]['CARGO_REGISTRIES_EAST_CREDENTIAL_PROVIDER'], 'cargo:token')
            self.assertEqual(scan.call_args.kwargs['private_index'], env['CARGO_REGISTRIES_EAST_INDEX'])
            self.assertEqual(run.call_args.args[0],
                             ['cargo', 'publish', '--registry', 'east', '--workspace', '--dry-run'])
            self.assertEqual([call.args[1] for call in archive.call_args_list], list(release.ORDER))
            self.assertEqual(scan.call_count, len(release.ORDER))
            self.assertEqual(run.call_args.args[1], stage / 'backend')
            self.assertEqual(run.call_args.args[2]['CARGO_TARGET_DIR'], str(stage / 'backend/target'))
            self.assertEqual(archive.call_args.args[0], stage / 'backend')

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
            args = Namespace(stage=stage, patterns=patterns, package='routeweft-domain', execute=True, registry='internal')
            with patch.dict(release.os.environ, env, clear=True), \
                    patch.object(release, 'run_cargo') as run, \
                    patch.object(release, 'scan_archive', side_effect=ValueError('blocked')):
                with self.assertRaises(ValueError):
                    release.publish(args)
                self.assertEqual(run.call_count, 1)
                self.assertIn('--dry-run', run.call_args.args[0])


if __name__ == '__main__':
    unittest.main()
