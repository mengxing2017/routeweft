#!/usr/bin/env python3
"""Resident provincial acceptance with fixed query budgets and an exact sample oracle.

Writes all requests/outcomes, including failures; actual road distance defines
bins. Never raises query budgets for the production Auto measurements.
"""
import argparse
from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import subprocess
import time

BINS = {10: (5, 20), 50: (20, 75), 100: (75, 150), 300: (200, 400), 500: (400, 700)}


def group(distance_m):
    for key, (low, high) in BINS.items():
        if low * 1000 <= distance_m < high * 1000:
            return key
    return None


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--dataset', type=Path, required=True)
    p.add_argument('--scenarios', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--max-load-bytes', type=int, default=12 * 1024**3)
    p.add_argument('--oracle-per-group', type=int, default=2)
    args = p.parse_args()
    if not 1 <= args.oracle_per_group <= 20:
        p.error('--oracle-per-group must be 1..20')
    config = json.loads(args.scenarios.read_text())
    args.output.mkdir(parents=True, exist_ok=False)
    summary = {'complete': False, 'passed': False, 'failures': []}
    binary = str(args.binary.resolve(strict=True))
    requests = []
    for seed in config['seeds']:
        a, b = [config['anchors'][key] for key in seed['anchors']]
        for i, (dx, dy) in enumerate(config['jitter_degrees']):
            points = [{'lon': a[0] + dx, 'lat': a[1] + dy},
                      {'lon': b[0] - dx, 'lat': b[1] - dy}]
            for direction in range(2):
                requests.append({'id': f'{seed["id"]}-{i}-{direction}',
                                 'coordinates': points[::1 if direction == 0 else -1]})
    for name in config['core_cities']:
        for direction in range(2):
            points = [dict(zip(('lon', 'lat'), config['anchors'][key])) for key in ('Guangzhou', name)]
            requests.append({'id': f'core-{name}-{direction}',
                             'coordinates': points[::1 if direction == 0 else -1]})
    (args.output / 'requests.json').write_text(json.dumps(requests, indent=2) + '\n')
    index = args.dataset / 'routing.mld'
    identity = {'binary_sha256': digest(Path(binary)),
                'index_sha256': digest(index) if index.exists() else None,
                'version': json.loads(subprocess.check_output([binary, 'version', '--json'])),
                'manifest': json.loads((args.dataset / 'manifest.json').read_text()),
                'scenarios_sha256': hashlib.sha256(args.scenarios.read_bytes()).hexdigest()}
    (args.output / 'identity.json').write_text(json.dumps(identity, indent=2) + '\n')

    def run(algorithm, queries):
        command = [binary, 'serve', '--dataset', str(args.dataset), '--graph-capacity', 'regional',
                   '--max-load-bytes', str(args.max_load_bytes), '--algorithm', algorithm]
        if algorithm == 'dijkstra':
            command += ['--max-work', '40000000', '--max-states', '8000000',
                        '--max-queue', '4000000', '--timeout-ms', '60000']
        # communicate drains both pipes; no persistent pipe deadlock. Every query
        # also has the router's own deadline. Preserve complete process evidence.
        start = time.monotonic()
        result = subprocess.run(command, input=''.join(json.dumps(q) + '\n' for q in queries),
                                text=True, capture_output=True, timeout=180 + len(queries) * 65)
        records = [json.loads(line) for line in result.stdout.splitlines()]
        evidence = {'command': command, 'exit_code': result.returncode,
                    'process_seconds': time.monotonic() - start, 'stderr': result.stderr,
                    'ready': records[0] if records else None, 'records': records[1:]}
        (args.output / f'{algorithm}.json').write_text(json.dumps(evidence, indent=2) + '\n')
        if result.returncode or not records or records[0].get('status') != 'ready' or len(records) != len(queries) + 1:
            raise RuntimeError(f'{algorithm}: incomplete resident protocol')
        if [r.get('id') for r in records[1:]] != [q['id'] for q in queries]:
            raise RuntimeError(f'{algorithm}: request identity mismatch')
        return evidence

    try:
        auto = run('auto', requests)
        if not auto['ready'].get('acceleration'):
            summary['failures'].append('MLD index not loaded')
        buckets = {key: [] for key in BINS}
        selected = {key: [] for key in BINS}
        errors = Counter()
        for query, result in zip(requests, auto['records']):
            if result['status'] != 'ok':
                errors[result.get('error', {}).get('kind', 'unknown')] += 1
                continue
            diagnostics = result.get('diagnostics', {})
            if diagnostics.get('validated_route_edges', 0) == 0 and result['route']['distance_m'] > 1:
                summary['failures'].append(f'{query["id"]}: no original-edge legality replay')
            key = group(result['route']['distance_m'])
            if key is not None:
                buckets[key].append(result)
                selected[key].append(query)
        if errors:
            summary['failures'].append(f'query failures: {dict(errors)}')
        summary['errors'] = dict(errors)
        summary['core_failures'] = [r['id'] for r in auto['records']
                                    if r['id'].startswith('core-') and r['status'] != 'ok']
        summary['budget_failures'] = dict(Counter(
            (r.get('diagnostics', {}).get('failure') or {}).get('reason', 'unknown')
            for r in auto['records'] if r.get('error', {}).get('kind') == 'BudgetExceeded'))
        summary['groups'] = {}
        for key, records in buckets.items():
            times = [r['diagnostics']['elapsed_ms'] for r in records]
            directions = {}
            for r in records:
                pair, direction = r['id'].rsplit('-', 1)
                directions.setdefault(pair, set()).add(direction)
            bidirectional = sum(v == {'0', '1'} for v in directions.values())
            summary['groups'][key] = {'count': len(records), 'range_km': BINS[key],
                                     'bidirectional_pairs': bidirectional,
                                     'p50_ms': percentile(times, .5), 'p95_ms': percentile(times, .95),
                                     'max_ms': max(times, default=None),
                                     'max_work': max((r['diagnostics']['work'] for r in records), default=0),
                                     'max_states': max((r['diagnostics']['states'] for r in records), default=0)}
            if bidirectional < 20:
                summary['failures'].append(f'{key} km: fewer than 20 bidirectional pairs')
            if times and (percentile(times, .95) > 1000 or max(times) > 5000):
                summary['failures'].append(f'{key} km: latency gate failed')
        oracle_queries = [q for key in BINS for q in selected[key][:args.oracle_per_group]]
        if len(oracle_queries) != len(BINS) * args.oracle_per_group:
            summary['failures'].append('insufficient exact oracle coverage')
        else:
            baseline = run('dijkstra', oracle_queries)
            actual = {r['id']: r for r in auto['records']}
            for reference in baseline['records']:
                if reference['status'] != 'ok':
                    summary['failures'].append(f'{reference["id"]}: exact oracle failed')
                elif round(reference['route']['duration_s'] * 1000) != round(actual[reference['id']]['route']['duration_s'] * 1000):
                    summary['failures'].append(f'{reference["id"]}: exact cost mismatch')
        times = [r['diagnostics']['elapsed_ms'] for r in auto['records'] if 'diagnostics' in r]
        summary.update({'complete': True, 'queries': len(requests),
                        'core_queries': sum(q['id'].startswith('core-') for q in requests),
                        'oracle_queries': len(oracle_queries), 'p95_ms_including_failures': percentile(times, .95),
                        'passed': not summary['failures']})
    except Exception as error:
        summary['failures'].append(str(error))
    finally:
        (args.output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary), flush=True)
    raise SystemExit(0 if summary['passed'] else 1)


if __name__ == '__main__':
    main()
