#!/usr/bin/env python3
"""Explicit local city acceptance; builds and verifies self-created ZIP/tar archives.

Uses local inputs and produces independent validation artifacts. Linux /proc RSS sampling
is optional; elapsed time includes process startup and dataset loading.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tarfile
import time
import zipfile


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='new directory')
    parser.add_argument('--coordinates', required=True, help='reviewed successful route lon,lat;lon,lat')
    parser.add_argument('--no-route', required=True, help='reviewed NoRoute coordinates')
    parser.add_argument('--radius-m', type=float, default=1000)
    parser.add_argument('--no-route-radius-m', type=float, default=1000)
    parser.add_argument('--invalid-restrictions', choices=['reject', 'block_ways'], default='reject')
    parser.add_argument('--timeout-s', type=float, default=600)
    args = parser.parse_args()
    if not 0 < args.timeout_s <= 3600:
        parser.error('timeout must be positive and at most 3600 seconds')
    binary, source, root = args.binary.resolve(strict=True), args.input.resolve(strict=True), args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    summary = {'input_sha256': digest(source), 'binary_sha256': digest(binary),
               'policy': args.invalid_restrictions, 'status': 'running', 'calls': []}

    def call(command, *arguments, expected=0):
        index = len(summary['calls'])
        stdout, stderr = root / f'{index:02}-{command}.stdout.json', root / f'{index:02}-{command}.stderr.txt'
        peak = None
        start = time.monotonic()
        with stdout.open('w') as out, stderr.open('w') as err:
            with subprocess.Popen([str(binary), command, *map(str, arguments)], stdout=out, stderr=err, start_new_session=True) as process:
                while process.poll() is None:
                    # Check exec completed before sampling: pre-exec fork RSS is
                    # not a measurement of the routing executable's memory.
                    try:
                        if Path(f'/proc/{process.pid}/exe').resolve() == binary:
                            status = Path(f'/proc/{process.pid}/status').read_text()
                            hwm = next(int(line.split()[1]) for line in status.splitlines() if line.startswith('VmHWM:'))
                            peak = max(peak or 0, hwm)
                    except (OSError, StopIteration, ValueError):
                        pass
                    if time.monotonic() - start > args.timeout_s:
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        process.wait()
                        raise TimeoutError(f'{command}: outer timeout')
                    time.sleep(0.002)
        value = json.loads(stdout.read_text())
        record = {'command': command, 'arguments': list(map(str, arguments)), 'exit_code': process.returncode,
                  'elapsed_ms': (time.monotonic() - start) * 1000, 'sampled_vm_hwm_kib': peak,
                  'stdout': stdout.name, 'stderr': stderr.name}
        summary['calls'].append(record)
        if value.get('schema') != 1 or process.returncode != expected:
            raise ValueError(f'{command}: unexpected response: {value}')
        if expected == 0 and value.get('status') != 'ok':
            raise ValueError(f'{command}: non-success response')
        if expected == 3 and (value.get('status') != 'error' or value.get('error', {}).get('kind') != 'NoRoute'):
            raise ValueError(f'{command}: expected explicit NoRoute')
        return value

    def queries(dataset):
        call('nearest', '--dataset', dataset, '--coordinate', args.coordinates.split(';')[0], '--radius-m', args.radius_m)
        route = call('route', '--dataset', dataset, '--coordinates', args.coordinates, '--radius-m', args.radius_m)
        call('route', '--dataset', dataset, '--coordinates', args.no_route, '--radius-m', args.no_route_radius_m, expected=3)
        return route

    try:
        common = ['--input', source, '--profile', 'driving', '--invalid-restrictions', args.invalid_restrictions]
        call('version')
        preflight = call('preflight', *common)
        dataset = root / 'dataset'
        built = call('build', *common, '--output', dataset)
        summary['manifest'] = built['manifest']
        summary['report'] = built['report']
        if preflight['report'] != built['report']:
            raise ValueError('preflight/build disagreement')
        if built['manifest']['source_sha256'] != summary['input_sha256']:
            raise ValueError('input identity disagreement')
        call('verify', '--dataset', dataset)
        summary['route'] = queries(dataset)['route']
        repeated = root / 'repeated'
        call('build', *common, '--output', repeated)
        for name in ('manifest.json', 'graph.json'):
            if digest(dataset / name) != digest(repeated / name):
                raise ValueError(f'non-deterministic {name}')
        # Preserve policy and relation-level audit alongside the data. This is
        # supplemental evidence; the engine's v1 format still has two core files.
        (dataset / 'build-report.json').write_text(json.dumps(built['report'], ensure_ascii=False, indent=2))
        summary['archives'] = []
        for extension in ('zip', 'tar.gz'):
            archive = root / f'routing.{extension}'
            extracted = root / ('unzip' if extension == 'zip' else 'untar')
            if extension == 'zip':
                with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as bundle:
                    for path in sorted(dataset.iterdir()):
                        bundle.write(path, f'dataset/{path.name}')
                with zipfile.ZipFile(archive) as bundle:
                    bundle.extractall(extracted)
            else:
                with tarfile.open(archive, 'w:gz') as bundle:
                    bundle.add(dataset, arcname='dataset')
                with tarfile.open(archive) as bundle:
                    # Only archives created immediately above are consumed.
                    bundle.extractall(extracted, filter='data')
            call('verify', '--dataset', extracted / 'dataset')
            queries(extracted / 'dataset')
            summary['archives'].append({'name': archive.name, 'bytes': archive.stat().st_size, 'sha256': digest(archive)})
        if digest(source) != summary['input_sha256']:
            raise ValueError('source changed during acceptance')
        summary['status'] = 'passed'
    except Exception as error:
        summary['status'] = 'failed'
        summary['error'] = str(error)
        raise
    finally:
        (root / 'acceptance.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    print(json.dumps({'status': summary['status'], 'evidence': str(root / 'acceptance.json')}))


if __name__ == '__main__':
    main()
