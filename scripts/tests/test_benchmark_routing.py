import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('benchmark', Path(__file__).parents[1] / 'benchmark-routing.py')
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)

FAKE = '''#!/usr/bin/env python3
import hashlib, json, pathlib, sys, time
config = json.loads(pathlib.Path(__file__).with_suffix('.json').read_text())
if sys.argv[1] == 'version':
    print(json.dumps({'schema': 1, 'version': 'protocol-test-double'})); sys.exit()
dataset = pathlib.Path(sys.argv[sys.argv.index('--dataset') + 1])
manifest = json.loads((dataset / 'manifest.json').read_text())
algorithm = sys.argv[sys.argv.index('--algorithm') + 1]
actual = 'mld' if config.get('mld') and algorithm == 'auto' else 'dijkstra'
if config.get('hang_ready'):
    time.sleep(10)
artifact = dataset / 'routing.mld'
print(json.dumps({'schema': 1, 'status': 'ready', 'dataset_id': manifest['dataset_id'],
    'graph_sha256': manifest['graph_sha256'], 'profile': config.get('ready_profile', 'driving-v1'),
    'acceleration': {'algorithm': 'mld'} if config.get('mld') else None,
    'acceleration_sha256': hashlib.sha256(artifact.read_bytes()).hexdigest() if artifact.exists() else None,
    'load_ms': config.get('load_ms', 5),
    'effective_query_limits': {'max_work': 2000000, 'max_states': 250000,
        'max_queue': 500000, 'max_geometry': 100000, 'timeout_ms': 10000}}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    elapsed = config.get('elapsed_ms', 10)
    if request['id'].startswith('warmup'):
        elapsed = config.get('warmup_ms', elapsed)
    record = {'schema': 1, 'id': request['id'], 'status': 'ok',
        'route': {'duration_s': config.get('actual_cost', 1) if algorithm == 'auto' else 1, 'legs': [{}]},
        'diagnostics': {'algorithm': actual, 'elapsed_ms': elapsed, 'states': 10, 'work': 20}}
    if config.get('fallback_query') and algorithm == 'auto':
        record['diagnostics']['algorithm'] = 'dijkstra'
    if config.get('baseline_failure') and algorithm == 'dijkstra':
        record.update(status='error', error={'kind': 'BudgetExceeded'})
    if config.get('no_route'):
        record.update(status='error', error={'kind': 'NoRoute'})
    if config.get('missing_diagnostics'):
        del record['diagnostics']
    print(json.dumps(record), flush=True)
if config.get('extra_output'):
    print('{}', flush=True)
'''


class BenchmarkTests(unittest.TestCase):
    def run_case(self, config, flags=()):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'engine.py'
            binary.write_text(FAKE)
            binary.chmod(0o755)
            binary.with_suffix('.json').write_text(json.dumps(config))
            dataset = root / 'dataset'
            dataset.mkdir()
            (dataset / 'manifest.json').write_text(json.dumps({'dataset_id': 'a' * 64,
                'graph_sha256': 'b' * 64, 'profile': 'driving-v1'}))
            if config.get('mld'):
                (dataset / 'routing.mld').write_bytes(b'protocol-test-artifact')
            pairs = root / 'pairs.json'
            pairs.write_text(json.dumps([[{'lon': 0, 'lat': 0}, {'lon': .01, 'lat': 0}]]))
            output = root / 'evidence'
            args = ['--binary', str(binary), '--dataset', str(dataset), '--pairs', str(pairs),
                    '--output', str(output), '--repeat', '2', *flags]
            with contextlib.redirect_stdout(io.StringIO()):
                code = benchmark.main(args)
            summary = json.loads((output / 'summary.json').read_text())
            receipts = {name: json.loads((output / f'{name}.json').read_text())
                        for name in ('auto', 'dijkstra') if (output / f'{name}.json').exists()}
            self.assertTrue((output / 'identity.json').exists())
            self.assertTrue((output / 'dijkstra.jsonl').exists())
            return code, summary, receipts

    def test_differential_fallback_is_compatible_but_not_acceptance(self):
        code, summary, _ = self.run_case({})
        self.assertEqual(code, 0)
        self.assertEqual(summary['compared'], 2)
        self.assertFalse(summary['acceptance_passed'])

    def test_mld_requirement_rejects_fast_and_slow_fallbacks(self):
        for elapsed in (10, 5000):
            with self.subTest(elapsed=elapsed):
                code, summary, _ = self.run_case({'elapsed_ms': elapsed}, ['--mode', 'acceptance', '--require-mld'])
                self.assertEqual(code, 1)
                self.assertIn('mld_required_at_ready', [f['reason'] for f in summary['failures']])
                self.assertIn('mld_required_for_query', [f['reason'] for f in summary['failures']])

    def test_independent_warmup_is_preserved_and_excluded_from_percentiles(self):
        code, summary, receipts = self.run_case({'mld': True, 'warmup_ms': 5000},
            ['--mode', 'acceptance', '--require-mld', '--warmup', '1', '--max-p95-ms', '20'])
        self.assertEqual(code, 0, summary)
        self.assertTrue(summary['acceptance_passed'])
        self.assertEqual(summary['auto']['p95_ms'], 10)
        self.assertEqual(len(receipts['auto']['warmup_records']), 1)
        self.assertEqual(receipts['auto']['warmup_records'][0]['diagnostics']['elapsed_ms'], 5000)
        self.assertEqual(len(receipts['auto']['client_times_ms']), 2)

    def test_load_latency_work_and_state_thresholds_fail_with_saved_reasons(self):
        for flags, metric in [(['--max-p95-ms', '5'], 'p95_ms'), (['--max-p99-ms', '5'], 'p99_ms'),
                (['--max-load-ms', '1'], 'load_ms'), (['--max-work', '1'], 'max_work'),
                (['--max-states', '1'], 'max_states')]:
            with self.subTest(metric=metric):
                code, summary, _ = self.run_case({'mld': True}, ['--mode', 'acceptance', *flags])
                self.assertEqual(code, 1)
                self.assertTrue(any(f.get('metric') == metric for f in summary['failures']))

    def test_oracle_failure_cost_mismatch_and_query_fallback_cannot_pass(self):
        for config, reason in [({'baseline_failure': True}, 'oracle_incomplete'),
                ({'actual_cost': 2}, 'cost_mismatch'), ({'fallback_query': True}, 'mld_required_for_query')]:
            with self.subTest(reason=reason):
                code, summary, _ = self.run_case({'mld': True, **config}, ['--require-mld'])
                self.assertEqual(code, 1)
                self.assertIn(reason, [f['reason'] for f in summary['failures']])

    def test_malformed_diagnostics_timeout_and_extra_output_preserve_failure_receipts(self):
        for config, flags in [({'missing_diagnostics': True}, []), ({'extra_output': True}, []),
                ({'hang_ready': True}, ['--ready-timeout-seconds', '.05'])]:
            with self.subTest(config=config):
                code, summary, receipts = self.run_case(config, flags)
                self.assertEqual(code, 1)
                self.assertFalse(summary['accepted'])
                self.assertIn('execution_or_protocol_failure', [f['reason'] for f in summary['failures']])
                self.assertIn('dijkstra', receipts)

    def test_no_route_is_compared_but_reachable_policy_rejects_it(self):
        code, summary, _ = self.run_case({'no_route': True})
        self.assertEqual(code, 0)
        self.assertEqual(summary['compared'], 2)
        code, summary, _ = self.run_case({'no_route': True}, ['--mode', 'acceptance', '--require-reachable'])
        self.assertEqual(code, 1)
        self.assertIn('reachable_route_required', [f['reason'] for f in summary['failures']])

    def test_acceptance_binds_the_loaded_profile(self):
        code, summary, _ = self.run_case({'mld': True, 'ready_profile': 'different-profile'},
            ['--mode', 'acceptance', '--require-mld'])
        self.assertEqual(code, 1)
        self.assertIn('resident_resource_identity_mismatch', [f['reason'] for f in summary['failures']])


if __name__ == '__main__':
    unittest.main()
