#!/usr/bin/env python3
"""Resident differential benchmark and explicit navigation acceptance gates."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time


def digest(path):
    hasher = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(65536), b''):
            hasher.update(block)
    return hasher.hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + '\n')


def percentile(values, fraction):
    return sorted(values)[math.ceil(len(values) * fraction) - 1] if values else None


class Resident:
    """Serial session with bounded lines, output and wall time."""
    def __init__(self, command, evidence, max_output_bytes):
        self.command = command
        self.evidence = evidence
        self.remaining = max_output_bytes
        self.buffer = bytearray()
        self.stderr = bytearray()
        self.process = None
        self.selector = selectors.DefaultSelector()

    def __enter__(self):
        self.started = time.monotonic()
        self.process = subprocess.Popen(self.command, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.selector.register(self.process.stdout, selectors.EVENT_READ, 'stdout')
        self.selector.register(self.process.stderr, selectors.EVENT_READ, 'stderr')
        return self

    def read(self, timeout):
        until = time.monotonic() + timeout
        while True:
            newline = self.buffer.find(b'\n')
            if newline >= 0:
                if newline > 16 * 1024**2:
                    raise RuntimeError('resident response exceeds 16 MiB')
                line = bytes(self.buffer[:newline + 1])
                del self.buffer[:newline + 1]
                self.evidence.write(line)
                self.evidence.flush()
                return json.loads(line)
            if len(self.buffer) > 16 * 1024**2:
                raise RuntimeError('resident response exceeds 16 MiB')
            left = until - time.monotonic()
            if left <= 0:
                raise TimeoutError('resident ready/response wall-clock timeout')
            events = self.selector.select(left)
            if not events:
                raise TimeoutError('resident ready/response wall-clock timeout')
            for key, _ in events:
                block = os.read(key.fd, 65536)
                if not block:
                    self.selector.unregister(key.fileobj)
                    if key.data == 'stdout':
                        raise RuntimeError('resident exited before a complete response')
                    continue
                self.remaining -= len(block)
                if self.remaining < 0:
                    raise RuntimeError('resident output budget exceeded')
                if key.data == 'stderr':
                    self.stderr.extend(block)
                    if len(self.stderr) > 1024**2:
                        raise RuntimeError('resident stderr exceeds 1 MiB')
                else:
                    self.buffer.extend(block)

    def request(self, request, timeout):
        started = time.monotonic()
        self.process.stdin.write((json.dumps(request, allow_nan=False) + '\n').encode())
        self.process.stdin.flush()
        response = self.read(timeout)
        return response, (time.monotonic() - started) * 1000

    def __exit__(self, kind, value, traceback):
        try:
            if self.process is not None:
                if kind is None:
                    self.process.stdin.close()
                    until = time.monotonic() + 5
                    while self.selector.get_map():
                        left = until - time.monotonic()
                        if left <= 0:
                            raise TimeoutError('resident did not close output after EOF')
                        for key, _ in self.selector.select(left):
                            block = os.read(key.fd, 65536)
                            if not block:
                                self.selector.unregister(key.fileobj)
                            elif key.data == 'stdout':
                                self.evidence.write(block)
                                raise RuntimeError('unexpected extra resident output')
                            else:
                                self.remaining -= len(block)
                                if self.remaining < 0:
                                    raise RuntimeError('resident output budget exceeded')
                                self.stderr.extend(block)
                                if len(self.stderr) > 1024**2:
                                    raise RuntimeError('resident stderr exceeds 1 MiB')
                    try:
                        self.process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.process.kill()
                        self.process.wait(timeout=5)
                        raise RuntimeError('resident did not exit after EOF')
                    if self.process.returncode:
                        raise RuntimeError(f'resident exited with code {self.process.returncode}')
                else:
                    self.process.kill()
                    self.process.wait(timeout=5)
        except BaseException:
            if self.process is not None and self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
            raise
        finally:
            if self.buffer:
                self.evidence.write(self.buffer)
                self.evidence.flush()
            self.selector.close()
            if self.process is not None:
                for stream in [self.process.stdin, self.process.stdout, self.process.stderr]:
                    stream.close()


def validate_record(record, expected_id):
    if not isinstance(record, dict) or record.get('schema') != 1 or record.get('id') != expected_id:
        raise RuntimeError('request identity mismatch')
    if record.get('status') not in ('ok', 'error'):
        raise RuntimeError('unexpected response status')
    diagnostics = record.get('diagnostics', {})
    elapsed = diagnostics.get('elapsed_ms')
    if isinstance(elapsed, bool) or not isinstance(elapsed, (int, float)) or not math.isfinite(elapsed) or elapsed < 0:
        raise RuntimeError('missing or invalid elapsed_ms diagnostic')
    for field in ('work', 'states'):
        if type(diagnostics.get(field)) is not int or diagnostics[field] < 0:
            raise RuntimeError(f'missing or invalid {field} diagnostic')
    if diagnostics.get('algorithm') not in ('dijkstra', 'mld'):
        raise RuntimeError('missing or invalid actual algorithm')
    if record['status'] == 'ok':
        route = record.get('route', {})
        cost = route.get('duration_s')
        if isinstance(cost, bool) or not isinstance(cost, (int, float)) or not math.isfinite(cost) or cost < 0 or not isinstance(route.get('legs'), list):
            raise RuntimeError('invalid route result')


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'dataset', 'pairs', 'output'):
        parser.add_argument(f'--{name}', type=Path, required=True)
    parser.add_argument('--mode', choices=['differential', 'acceptance'], default='differential')
    parser.add_argument('--graph-capacity', choices=['city', 'regional'], default='city')
    parser.add_argument('--max-load-bytes', type=int, default=2 * 1024**3)
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--warmup', type=int, default=0, help='rounds excluded from percentiles')
    parser.add_argument('--baseline-max-states', type=int, default=8_000_000)
    parser.add_argument('--require-mld', action='store_true')
    parser.add_argument('--require-reachable', action='store_true')
    for name in ('max-load-ms', 'max-ready-ms', 'max-p95-ms', 'max-p99-ms', 'max-client-p95-ms'):
        parser.add_argument(f'--{name}', type=float)
    parser.add_argument('--max-work', type=int, help='auto observed-work acceptance ceiling')
    parser.add_argument('--max-states', type=int, help='auto observed-states acceptance ceiling')
    parser.add_argument('--ready-timeout-seconds', type=float, default=120)
    parser.add_argument('--response-timeout-seconds', type=float, default=65)
    parser.add_argument('--max-output-bytes', type=int, default=128 * 1024**2)
    args = parser.parse_args(argv)
    if not 1 <= args.repeat <= 100 or not 0 <= args.warmup <= 100:
        parser.error('repeat must be 1-100; warmup must be 0-100')
    if not 1 <= args.baseline_max_states <= 16_000_000:
        parser.error('baseline-max-states must be 1-16000000')
    for name, value in vars(args).items():
        if name.startswith('max_') or name.endswith('_seconds'):
            if value is not None and (not math.isfinite(value) or value <= 0):
                parser.error(f'{name} must be finite and positive')
    policies = ('max_load_ms', 'max_ready_ms', 'max_p95_ms', 'max_p99_ms',
                'max_client_p95_ms', 'max_work', 'max_states')
    if args.mode == 'acceptance' and not (args.require_mld or args.require_reachable or any(
            getattr(args, name) is not None for name in policies)):
        parser.error('acceptance requires an explicit algorithm, reachability or performance policy')
    if args.pairs.stat().st_size > 8 * 1024**2:
        parser.error('pairs input exceeds 8 MiB')
    sample_bytes = args.pairs.read_bytes()
    document = json.loads(sample_bytes)
    args.samples_sha256 = hashlib.sha256(sample_bytes).hexdigest()
    if isinstance(document, dict):
        sampling = document.get('sampling', document)
        if sampling.get('coverage_complete') is not True:
            parser.error('generated sample coverage is incomplete')
        if sampling.get('sampler_version') != 1:
            parser.error('unsupported sampler version')
        pairs = sampling.get('pairs')
        args.sample_identity = {key: sampling.get(key) for key in ('dataset_id', 'graph_sha256', 'profile', 'sampler_version', 'seed_sha256')}
    else:
        pairs = document
    if not isinstance(pairs, list) or not 1 <= len(pairs) <= 10000:
        parser.error('pairs must contain 1-10000 queries')
    for points in pairs:
        if not isinstance(points, list) or not 2 <= len(points) <= 10:
            parser.error('each query must contain 2-10 coordinates')
        for point in points:
            if not isinstance(point, dict) or set(point) != {'lon', 'lat'} or any(
                    isinstance(point[k], bool) or not isinstance(point[k], (int, float))
                    or not math.isfinite(point[k]) or abs(point[k]) > limit
                    for k, limit in [('lon', 180), ('lat', 85)]):
                parser.error('invalid coordinate')
    if len(pairs) * (args.warmup + args.repeat) > 100000:
        parser.error('total warmup plus measured requests exceeds 100000 per engine')
    return args, pairs


def main(argv=None):
    args, pairs = parse_args(argv)
    args.output.mkdir(parents=True, exist_ok=False)
    write_json(args.output / 'pairs.json', pairs)
    policy = {k: v for k, v in vars(args).items() if not isinstance(v, Path)}
    summary = {'schema': 2, 'mode': args.mode, 'accepted': False, 'compared': 0,
               'queries': len(pairs) * args.repeat, 'warmup_queries': len(pairs) * args.warmup,
               'percentiles_include_failures': True, 'percentile_method': 'nearest_rank',
               'percentile_scope': 'measured_rounds_only', 'policy': policy, 'failures': []}
    outcomes = {}

    def fail(reason, **details):
        summary['failures'].append({'reason': reason, **details})

    try:
        binary = args.binary.resolve()
        artifact = args.dataset / 'routing.mld'
        identity = {'benchmark_sha256': digest(Path(__file__)),
                    'binary_sha256': digest(binary),
                    'version': json.loads(subprocess.check_output([str(binary), 'version', '--json'], timeout=10)),
                    'manifest': json.loads((args.dataset / 'manifest.json').read_text()),
                    'mld_sha256': digest(artifact) if artifact.exists() else None,
                    'samples_sha256': args.samples_sha256,
                    'pairs_sha256': hashlib.sha256(json.dumps(pairs, sort_keys=True, separators=(',', ':')).encode()).hexdigest()}
        write_json(args.output / 'identity.json', identity)
        if getattr(args, 'sample_identity', None):
            for field in ('dataset_id', 'graph_sha256', 'profile'):
                if args.sample_identity.get(field) != identity['manifest'].get(field):
                    raise RuntimeError(f'sample set identity mismatch: {field}')
        for algorithm in ['dijkstra', 'auto']:
            command = [str(binary), 'serve', '--dataset', str(args.dataset),
                       '--graph-capacity', args.graph_capacity,
                       '--max-load-bytes', str(args.max_load_bytes), '--algorithm', algorithm]
            if algorithm == 'dijkstra':
                command += ['--max-work', '40000000', '--max-states', str(args.baseline_max_states),
                            '--max-queue', '4000000', '--timeout-ms', '60000']
            records, warmups, client_times = [], [], []
            receipt = {'command': command, 'records': records, 'warmup_records': warmups, 'client_times_ms': client_times}
            session = None
            try:
                with (args.output / f'{algorithm}.jsonl').open('wb') as evidence, Resident(
                        command, evidence, args.max_output_bytes) as session:
                    ready = session.read(args.ready_timeout_seconds)
                    ready_ms = (time.monotonic() - session.started) * 1000
                    receipt.update(ready=ready, ready_wall_ms=ready_ms)
                    if ready.get('schema') != 1 or ready.get('status') != 'ready' or ready.get('dataset_id') != identity['manifest']['dataset_id']:
                        raise RuntimeError('unexpected resident ready identity')
                    if ready.get('graph_sha256', identity['manifest']['graph_sha256']) != identity['manifest']['graph_sha256']:
                        raise RuntimeError('resident graph digest mismatch')
                    load_ms = ready.get('load_ms')
                    if isinstance(load_ms, bool) or not isinstance(load_ms, (int, float)) or not math.isfinite(load_ms) or load_ms < 0:
                        raise RuntimeError('missing or invalid load_ms diagnostic')
                    if algorithm == 'auto' and args.require_mld:
                        if not identity['mld_sha256'] or not isinstance(ready.get('acceleration'), dict) or ready['acceleration'].get('algorithm') != 'mld':
                            fail('mld_required_at_ready')
                        if ready.get('acceleration_sha256') != identity['mld_sha256']:
                            fail('mld_identity_mismatch')
                    if args.mode == 'acceptance':
                        for field in ('graph_sha256', 'profile'):
                            if ready.get(field) != identity['manifest'].get(field):
                                fail('resident_resource_identity_mismatch', algorithm=algorithm, field=field)
                        limits = ready.get('effective_query_limits')
                        if not isinstance(limits, dict) or any(type(limits.get(k)) is not int or limits[k] < 0
                                for k in ('max_work', 'max_states', 'max_queue', 'max_geometry', 'timeout_ms')):
                            fail('effective_query_limits_missing', algorithm=algorithm)
                    for phase, rounds, target in [('warmup', args.warmup, warmups), ('measured', args.repeat, records)]:
                        for repeat in range(rounds):
                            for index, points in enumerate(pairs):
                                request_id = f'{phase}-{repeat}-{index}'
                                record, wall_ms = session.request({'id': request_id, 'coordinates': points}, args.response_timeout_seconds)
                                validate_record(record, request_id)
                                target.append(record)
                                if algorithm == 'dijkstra' and record['diagnostics']['algorithm'] != 'dijkstra':
                                    fail('oracle_algorithm_mismatch', id=request_id)
                                if phase == 'measured':
                                    client_times.append(wall_ms)
                                if algorithm == 'auto' and args.require_mld and record['diagnostics']['algorithm'] != 'mld':
                                    fail('mld_required_for_query', id=request_id, actual=record['diagnostics']['algorithm'])
                                if args.require_reachable and record['status'] != 'ok':
                                    fail('reachable_route_required', algorithm=algorithm, id=request_id)
                    receipt.update(process_seconds=time.monotonic() - session.started,
                                   stderr=session.stderr.decode(errors='replace'))
                if session.buffer:
                    raise RuntimeError('unexpected extra resident output')
            finally:
                if session is not None:
                    receipt['stderr'] = session.stderr.decode(errors='replace')
                    receipt['process_seconds'] = time.monotonic() - session.started
                write_json(args.output / f'{algorithm}.json', receipt)
            outcomes[algorithm] = records
            elapsed = [r['diagnostics']['elapsed_ms'] for r in records]
            summary[algorithm] = {
                'count': len(records), 'successes': sum(r['status'] == 'ok' for r in records),
                'errors': [r.get('error') for r in records if r['status'] != 'ok'],
                'algorithms': sorted({r['diagnostics']['algorithm'] for r in records}),
                'p50_ms': percentile(elapsed, .5), 'p95_ms': percentile(elapsed, .95),
                'p99_ms': percentile(elapsed, .99), 'client_p95_ms': percentile(client_times, .95),
                'load_ms': receipt['ready'].get('load_ms'), 'ready_wall_ms': receipt['ready_wall_ms'],
                'effective_query_limits': receipt['ready'].get('effective_query_limits'),
                'max_states': max(r['diagnostics']['states'] for r in records),
                'max_work': max(r['diagnostics']['work'] for r in records)}
            for r in warmups:
                if r['status'] != 'ok' and r.get('error', {}).get('kind') != 'NoRoute':
                    fail('warmup_failed', algorithm=algorithm, id=r['id'])
        for baseline, actual in zip(outcomes['dijkstra'], outcomes['auto']):
            if baseline['id'] != actual['id']:
                fail('request_identity_mismatch')
            elif baseline['status'] == 'ok':
                if actual['status'] != 'ok':
                    fail('accelerated_query_failed', id=actual['id'])
                elif round(baseline['route']['duration_s'] * 1000) != round(actual['route']['duration_s'] * 1000):
                    fail('cost_mismatch', id=actual['id'])
                elif len(baseline['route']['legs']) != len(actual['route']['legs']):
                    fail('leg_mismatch', id=actual['id'])
                else:
                    summary['compared'] += 1
            elif baseline.get('error', {}).get('kind') == 'NoRoute':
                if actual['status'] == 'error' and actual.get('error', {}).get('kind') == 'NoRoute':
                    summary['compared'] += 1
                else:
                    fail('reachability_mismatch', id=actual['id'])
            else:
                fail('oracle_incomplete', id=baseline['id'])
        if summary['compared'] != summary['queries']:
            fail('incomplete_oracle_comparison')
        for argument, field in [('max_load_ms', 'load_ms'), ('max_ready_ms', 'ready_wall_ms'),
                ('max_p95_ms', 'p95_ms'), ('max_p99_ms', 'p99_ms'), ('max_client_p95_ms', 'client_p95_ms'),
                ('max_work', 'max_work'), ('max_states', 'max_states')]:
            limit = getattr(args, argument)
            if limit is not None:
                observed = summary['auto'][field]
                if not isinstance(observed, (int, float)) or not math.isfinite(observed) or observed > limit:
                    fail('threshold_exceeded', metric=field, observed=observed, limit=limit)
        if digest(Path(__file__)) != identity['benchmark_sha256'] or digest(args.pairs) != identity['samples_sha256'] or digest(binary) != identity['binary_sha256'] or json.loads((args.dataset / 'manifest.json').read_text()) != identity['manifest'] or (
                digest(artifact) if artifact.exists() else None) != identity['mld_sha256']:
            fail('inputs_changed_during_benchmark')
    except Exception as error:
        fail('execution_or_protocol_failure', error=f'{type(error).__name__}: {error}')
    summary['accepted'] = not summary['failures']
    summary['acceptance_passed'] = args.mode == 'acceptance' and summary['accepted']
    write_json(args.output / 'summary.json', summary)
    print(json.dumps(summary, allow_nan=False), flush=True)
    return 0 if summary['accepted'] else 1


if __name__ == '__main__':
    sys.exit(main())
