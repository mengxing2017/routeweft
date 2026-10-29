#!/usr/bin/env python3
"""Explicit resident MLD/Dijkstra differential benchmark; no implicit map paths."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--dataset', type=Path, required=True)
    parser.add_argument('--pairs', type=Path, required=True,
                        help='JSON array of arrays of {lon,lat}; 2-10 waypoints each')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--graph-capacity', choices=['city', 'regional'], default='city')
    parser.add_argument('--max-load-bytes', type=int, default=2 * 1024**3)
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--baseline-max-states', type=int, default=8_000_000)
    args = parser.parse_args()
    if not 1 <= args.repeat <= 100:
        parser.error('repeat must be between 1 and 100')
    if not 1 <= args.baseline_max_states <= 16_000_000:
        parser.error('baseline-max-states must be between 1 and 16000000')
    binary = args.binary.resolve()
    pairs = json.loads(args.pairs.read_text())
    if not pairs or len(pairs) > 10000 or any(not 2 <= len(p) <= 10 for p in pairs):
        parser.error('pairs must contain 1-10000 queries of 2-10 coordinates')
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / 'pairs.json').write_text(json.dumps(pairs, indent=2) + '\n')
    identity = {
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'version': json.loads(subprocess.check_output([str(binary), 'version', '--json'])),
        'manifest': json.loads((args.dataset / 'manifest.json').read_text()),
    }
    (args.output / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')
    outcomes = {}
    for algorithm in ['dijkstra', 'auto']:
        command = [str(binary), 'serve', '--dataset', str(args.dataset),
                   '--graph-capacity', args.graph_capacity,
                   '--max-load-bytes', str(args.max_load_bytes), '--algorithm', algorithm]
        # Let the baseline complete enough work to act as an oracle. Auto retains
        # the actual default budgets, rather than hiding failures by raising them.
        if algorithm == 'dijkstra':
            command += ['--max-work', '40000000', '--max-states', str(args.baseline_max_states),
                        '--max-queue', '4000000', '--timeout-ms', '60000']
        payload = ''.join(json.dumps({'id': f'{repeat}-{i}', 'coordinates': points}) + '\n'
                          for repeat in range(args.repeat) for i, points in enumerate(pairs))
        start = time.monotonic()
        completed = subprocess.run(command, input=payload, text=True, capture_output=True,
                                   timeout=120 + len(pairs) * args.repeat * 65, check=True)
        lines = [json.loads(line) for line in completed.stdout.splitlines()]
        if not lines or lines[0].get('status') != 'ready' or len(lines) != 1 + len(pairs) * args.repeat:
            raise RuntimeError('unexpected resident protocol output')
        records = lines[1:]
        outcomes[algorithm] = records
        (args.output / f'{algorithm}.json').write_text(json.dumps({
            'command': command, 'ready': lines[0], 'process_seconds': time.monotonic() - start,
            'stderr': completed.stderr, 'records': records,
        }, ensure_ascii=False, indent=2) + '\n')
    compared = 0
    for baseline, actual in zip(outcomes['dijkstra'], outcomes['auto']):
        if baseline['id'] != actual['id']:
            raise AssertionError('request identity mismatch')
        if baseline['status'] == 'ok':
            if actual['status'] != 'ok':
                raise AssertionError(f'accelerated query failed: {actual}')
            a, b = baseline['route'], actual['route']
            if round(a['duration_s'] * 1000) != round(b['duration_s'] * 1000):
                raise AssertionError(f'cost mismatch for {actual["id"]}')
            if len(a['legs']) != len(b['legs']):
                raise AssertionError('leg mismatch')
            compared += 1
        elif baseline.get('error', {}).get('kind') == 'NoRoute':
            if actual.get('error', {}).get('kind') != 'NoRoute':
                raise AssertionError('reachability mismatch')
            compared += 1
    summary = {'compared': compared, 'queries': len(pairs) * args.repeat,
               'percentiles_include_failures': True}
    for algorithm, records in outcomes.items():
        times = sorted(r['diagnostics']['elapsed_ms'] for r in records if 'diagnostics' in r)
        summary[algorithm] = {
            'count': len(records), 'successes': sum(r['status'] == 'ok' for r in records),
            'errors': [r.get('error') for r in records if r['status'] != 'ok'],
            'p50_ms': times[math.ceil(len(times) * .50) - 1],
            'p95_ms': times[math.ceil(len(times) * .95) - 1],
            'p99_ms': times[math.ceil(len(times) * .99) - 1],
            'max_states': max(r.get('diagnostics', {}).get('states', 0) for r in records),
            'max_work': max(r.get('diagnostics', {}).get('work', 0) for r in records),
        }
    (args.output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary), flush=True)
    if compared != summary['queries']:
        raise RuntimeError('incomplete oracle comparison; inspect preserved failures')


if __name__ == '__main__':
    main()
