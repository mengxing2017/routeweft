#!/usr/bin/env python3
"""Cargo release staging for crates.io and private registries."""
import argparse
from email.utils import parsedate_to_datetime
import math
import io
import json
import os
from pathlib import Path
import re
import queue
import signal
import shlex
import threading
import time
import subprocess
import shutil
import tarfile
import tempfile
import tomllib
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
ORDER = ('routeweft-domain', 'routeweft-dataset', 'routeweft-routing', 'routeweft-preprocess')
REPOSITORY = 'https://github.com/mengxing2017/routeweft'


class ReleaseInputError(ValueError):
    """Safe, actionable input error without private configuration."""


class CargoRateLimitError(ReleaseInputError):
    """Cargo failed with HTTP 429; retain only the server's retry timestamp."""

    def __init__(self, retry_at=None):
        super().__init__('Cargo 发布被限流（HTTP 429）；详见上方脱敏日志。')
        self.retry_at = retry_at


def retry_timestamp(line):
    seconds = re.search(r'(?i)retry-after:\s*(\d+)\s*$', line.strip())
    if seconds:
        return time.time() + int(seconds.group(1))
    match = re.search(r'(?i)(?:try again after|retry-after:)\s*(.+?GMT)\b', line)
    if match:
        try:
            date = parsedate_to_datetime(match.group(1))
            if date.tzinfo is not None:
                return date.timestamp()
        except (ValueError, OverflowError, TypeError):
            pass
    return None


def wait_for_publish(seconds, reason):
    deadline = time.monotonic() + seconds
    while (remaining := deadline - time.monotonic()) > 0:
        print(f'{reason}：还需等待 {math.ceil(remaining)}s；可按 Ctrl+C 取消。', flush=True)
        time.sleep(min(30, remaining))


def run_publish(command, stage, env, registry, max_retries=3):
    for attempt in range(max_retries + 1):
        try:
            run_cargo(command, stage, env)
            return
        except CargoRateLimitError as error:
            if registry != 'crates-io':
                raise
            if attempt == max_retries:
                raise ReleaseInputError(
                    f'crates.io 限流重试已达上限（{max_retries} 次），停止发布；'
                    '请按服务端时间等待后使用同一 --ref 和 --package 续发当前包。') from None
            delay = (max(1, error.retry_at - time.time() + 1) if error.retry_at is not None
                     else 60 * 2 ** min(attempt, 6))
            wait_for_publish(delay, f'crates.io 限流，准备第 {attempt + 1}/{max_retries} 次重试当前包')


def nonnegative_integer(value):
    try:
        number = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError('须为非负整数') from None
    if number < 0:
        raise argparse.ArgumentTypeError('须为非负整数')
    return number


def load_release_env():
    """Load local Cargo settings as data, preserving exported environment values."""
    path = ROOT / '.private-release.env'
    if not path.exists():
        return
    values = {}
    for number, line in enumerate(path.read_text().splitlines(), 1):
        try:
            fields = shlex.split(line, comments=True, posix=True)
        except ValueError:
            raise ReleaseInputError(
                f'.private-release.env 第 {number} 行引号格式错误。') from None
        if fields[:1] == ['export']:
            fields = fields[1:]
        if not fields:
            continue
        if len(fields) != 1 or '=' not in fields[0]:
            raise ReleaseInputError(
                f'.private-release.env 第 {number} 行须使用 KEY=value 格式。')
        key, value = fields[0].split('=', 1)
        if key not in ('NPM_REGISTRY_INTERNAL', 'NPM_TOKEN_INTERNAL') and not re.fullmatch(r'CARGO_(?:REGISTRY|REGISTRIES_[A-Z0-9_]+)_[A-Z0-9_]+', key):
            raise ReleaseInputError(
                f'.private-release.env 第 {number} 行须使用 Cargo registry 配置变量。')
        values[key] = value
    for key, value in values.items():
        os.environ.setdefault(key, value)


def git(*args):
    return subprocess.check_output(['git', '-C', str(ROOT), *args], stderr=subprocess.PIPE)


def resolve_commit(ref):
    try:
        return git('rev-parse', '--verify', '--end-of-options', ref + '^{commit}').decode().strip()
    except subprocess.CalledProcessError:
        if ref == 'HEAD':
            raise ReleaseInputError(
                'HEAD 无法解析为提交。请先在本仓库提交待发布源码，再运行 make release-dry-run REGISTRIES=<目标仓库>；'
                '发布脚本不会自动提交，也不会包含未提交文件。') from None
        raise ReleaseInputError(
            '指定的 --ref 无法解析为提交，请检查 tag 或 commit 是否存在于本地仓库。') from None


def registry_name(args):
    name = getattr(args, 'registry', None)
    if not name:
        raise ReleaseInputError('请使用 --registry crates-io（公共）或 --registry internal（私有）指定目标仓库。')
    if name == 'crates-io':
        return name
    if not re.fullmatch(r'[a-z][a-z0-9_]*', name):
        raise ReleaseInputError('registry 名称须以小写字母开头，仅包含小写字母、数字和下划线。')
    return name


def registry_variable(registry, suffix):
    if registry == 'crates-io':
        return f'CARGO_REGISTRY_{suffix}'
    return f'CARGO_REGISTRIES_{registry.upper()}_{suffix}'


def registry_index(registry):
    if registry == 'crates-io':
        return 'sparse+https://index.crates.io/'
    key = registry_variable(registry, 'INDEX')
    index = os.environ.get(key, '')
    url = urlsplit(index.removeprefix('sparse+'))
    if url.scheme != 'https' or not url.hostname or url.username or url.password:
        raise ReleaseInputError(f'set {key} to an HTTPS index')
    return index


def require_token(registry):
    key = registry_variable(registry, 'TOKEN')
    if not os.environ.get(key):
        raise ReleaseInputError(f'{key} is required')


def rules(path, registry):
    if path is None:
        if registry == 'crates-io':
            # Scan known private hosts, never the public index's own hostname.
            patterns = []
            for key, value in os.environ.items():
                if key.startswith('CARGO_REGISTRIES_') and key.endswith('_INDEX') and value:
                    host = urlsplit(value.removeprefix('sparse+')).hostname
                    if host:
                        patterns.append(host.lower().encode())
            return sorted(set(patterns))
        index = registry_index(registry)
        url = urlsplit(index.removeprefix('sparse+'))
        return [url.hostname.lower().encode()]
    values = [line.strip().lower().encode() for line in path.read_text().splitlines()
              if line.strip() and not line.lstrip().startswith('#')]
    if not values:
        raise ValueError('private address pattern file must not be empty')
    return values


def inspect_bytes(data, patterns, label):
    # Literal, case-insensitive matching; never print the matched private value.
    if any(pattern in data.lower() for pattern in patterns):
        raise ValueError(f'private address detected in {label}')


def scan_tree(path, patterns):
    for file in sorted(path.rglob('*')):
        inspect_bytes(str(file.relative_to(path)).encode(), patterns, 'file name')
        if file.is_symlink():
            raise ValueError('symlinks are not permitted in release staging')
        if file.is_file():
            inspect_bytes(file.read_bytes(), patterns, str(file.relative_to(path)))


def scan_archive(path, patterns, private_index=None):
    with tarfile.open(path, 'r:gz') as archive:
        for member in archive:
            inspect_bytes(member.name.encode(), patterns, 'archive entry name')
            if member.isfile():
                data = archive.extractfile(member).read()
                # Cargo embeds the selected index in generated manifests/lockfiles.
                # These archives remain private; exempt only that exact index there.
                if private_index and Path(member.name).name in ('Cargo.toml', 'Cargo.lock'):
                    for value in (private_index, private_index.removeprefix('sparse+')):
                        data = data.replace(value.encode(), b'<private-index>')
                inspect_bytes(data, patterns, 'archive entry')
            elif not member.isdir():
                raise ValueError('unsupported archive entry type')


def check(args):
    registry = registry_name(args)
    patterns = rules(args.patterns, registry)
    if args.history:
        # Include commit/tag messages and all reachable historical blobs, not .git/config.
        objects = git('rev-list', '--objects', '--all').splitlines()
        for entry in objects:
            oid = entry.split(b' ', 1)[0].decode()
            kind = git('cat-file', '-t', oid).decode().strip()
            if kind in ('blob', 'commit', 'tag'):
                inspect_bytes(git('cat-file', kind, oid), patterns, f'Git object {oid}')
        for oid in git('for-each-ref', '--format=%(objectname)', 'refs/tags').splitlines():
            inspect_bytes(git('cat-file', '-p', oid.decode()), patterns, 'Git tag')
    if args.path:
        if args.path.is_dir():
            scan_tree(args.path, patterns)
        else:
            scan_archive(args.path, patterns)
    elif not args.history:
        for name in git('ls-files', '-z').split(b'\0'):
            if name:
                file = ROOT / os.fsdecode(name)
                if file.is_symlink():
                    raise ValueError('tracked symlink requires manual review')
                if file.is_file():
                    inspect_bytes(file.read_bytes(), patterns, 'tracked file')
    print('Private address check passed (literal patterns only).')


def prepare(args):
    registry = registry_name(args)
    patterns = rules(args.patterns, registry)
    destination = args.output.resolve()
    if destination.is_relative_to(ROOT) or destination.exists():
        raise ValueError('output must be a new directory outside the source repository')
    license_data = args.license_file.read_bytes() if args.license_file else b''
    inspect_bytes(license_data, patterns, 'project license')
    commit = resolve_commit(args.ref)
    archive_data = git('archive', commit, 'backend', 'LICENSES', 'THIRD_PARTY_NOTICES.md',
                       'docs/dependency-licenses.json', 'docs/data-sources.md')
    destination.mkdir(mode=0o700, parents=True)
    # Extract only regular files/directories; reject links and escaping paths.
    with tarfile.open(fileobj=io.BytesIO(archive_data)) as archive:
        for member in archive:
            target = (destination / member.name).resolve()
            if not target.is_relative_to(destination):
                raise ValueError('unsafe archive path')
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(archive.extractfile(member).read())
            else:
                raise ValueError('unsupported Git archive entry')
    scan_tree(destination, patterns)
    manifest = destination / 'backend/Cargo.toml'
    text = manifest.read_text()
    metadata = tomllib.loads(text)
    version = metadata['workspace']['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?', version):
        raise ValueError('unsupported release version')
    if text.count('publish = false') != 1:
        raise ValueError('workspace publish protection changed; review transformation')
    text = text.replace('publish = false', f'publish = ["{registry}"]\nrepository = "{REPOSITORY}"')
    for name in ORDER:
        original = metadata['workspace']['dependencies'][name]
        expected = f'{name} = {{ path = "{original["path"]}" }}'
        registry_field = '' if registry == 'crates-io' else f', registry = "{registry}"'
        replacement = f'{name} = {{ path = "{original["path"]}", version = "={version}"{registry_field} }}'
        if text.count(expected) != 1:
            raise ValueError(f'dependency layout changed: {name}')
        text = text.replace(expected, replacement)
    manifest.write_text(text)
    for name in (*ORDER, 'routeweft-daemon'):
        crate = destination / 'backend/crates' / name
        manifest = crate / 'Cargo.toml'
        text = manifest.read_text()
        if text.count('publish.workspace = true') != 1:
            raise ValueError(f'publish layout changed: {name}')
        if name == 'routeweft-daemon':
            text = text.replace('publish.workspace = true', 'publish = false')
        else:
            text = text.replace('publish.workspace = true',
                                'publish.workspace = true\nrepository.workspace = true')
            if license_data:
                text = text.replace('repository.workspace = true',
                                    'repository.workspace = true\nlicense-file = "LICENSE"')
                (crate / 'LICENSE').write_bytes(license_data)
            shutil.copytree(destination / 'LICENSES', crate / 'LICENSES')
            shutil.copyfile(destination / 'THIRD_PARTY_NOTICES.md', crate / 'THIRD_PARTY_NOTICES.md')
            (crate / 'docs').mkdir(exist_ok=True)
            for notice in ('dependency-licenses.json', 'data-sources.md'):
                shutil.copyfile(destination / 'docs' / notice, crate / 'docs' / notice)
        manifest.write_text(text)
    # Lockfile may contain registry URLs after Cargo resolves internal dependencies.
    # Keep the entire staging directory private, including logs and archives.
    (destination / 'release-source.json').write_text(json.dumps({
        'commit': commit, 'version': version, 'packages': list(ORDER),
        'registry': registry,
    }, indent=2) + '\n')
    scan_tree(destination, patterns)
    print(f'Release staging prepared: {version} ({commit[:12]}), registry={registry}. Source repository was not modified.')


def redact_output(output, env):
    for key, value in env.items():
        if value and any(word in key.upper() for word in ('TOKEN', 'PASSWORD', 'SECRET')):
            output = output.replace(value, '<redacted>')
    return re.sub(r'(?:sparse\+)?https?://[^\s<>"\']+', '<registry-url>', output)


def run_cargo(command, stage, env, heartbeat_seconds=15):
    started = time.monotonic()
    print('Cargo 开始执行；实时日志已脱敏。', flush=True)
    try:
        process = subprocess.Popen(command, cwd=stage, env=env, start_new_session=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, encoding='utf-8', errors='replace', bufsize=1)
    except FileNotFoundError:
        raise ReleaseInputError('找不到 cargo；请进入 devbox shell 后重试。') from None
    lines = queue.Queue()
    rate_limited = False
    retry_at = None

    def read_output():
        try:
            for line in process.stdout:
                lines.put(line)
        finally:
            lines.put(None)

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()
    try:
        while True:
            try:
                line = lines.get(timeout=heartbeat_seconds)
            except queue.Empty:
                elapsed = int(time.monotonic() - started)
                print(f'Cargo 仍在运行（已用时 {elapsed}s，最近 {heartbeat_seconds}s 无新日志）；'
                      '可按 Ctrl+C 取消。', flush=True)
                continue
            if line is None:
                break
            safe_line = redact_output(line, env)
            if re.search(r'(?i)status\s+429\b', safe_line):
                rate_limited = True
            timestamp = retry_timestamp(safe_line)
            if timestamp is not None:
                retry_at = timestamp if retry_at is None else max(retry_at, timestamp)
            print(safe_line, end='', flush=True)
        code = process.wait()
    except BaseException:
        # Cargo and its compiler children must stop before temporary files are removed.
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        raise
    finally:
        reader.join(timeout=1)
        process.stdout.close()
    elapsed = int(time.monotonic() - started)
    if code:
        if rate_limited:
            raise CargoRateLimitError(retry_at)
        raise ReleaseInputError(f'Cargo 执行失败（退出码 {code}，用时 {elapsed}s），详见上方脱敏日志。')
    print(f'Cargo 执行完成（用时 {elapsed}s）。', flush=True)


def package_archive(stage, package, version):
    directory = stage / 'target/package'
    name = f'{package}-{version}.crate'
    # Cargo versions differ: single-package output or workspace publish staging.
    candidates = [directory / name, directory / 'tmp-crate' / name,
                  directory / 'tmp-registry' / name]
    found = [path for path in candidates if path.is_file()]
    if not found:
        raise ReleaseInputError('Cargo 返回成功，但未找到待检查的 .crate 归档。')
    # Inspect every generated copy; do not silently accept inconsistent outputs.
    if any(path.read_bytes() != found[0].read_bytes() for path in found[1:]):
        raise ReleaseInputError('Cargo 生成了内容不一致的同名归档，已停止发布。')
    return found[0]


def publish(args):
    stage = args.stage.resolve()
    if stage.is_relative_to(ROOT):
        raise ValueError('release staging must be outside the source repository')
    source = json.loads((stage / 'release-source.json').read_text())
    registry = registry_name(args)
    if source['packages'] != list(ORDER) or source['registry'] != registry:
        raise ValueError('unexpected release staging metadata')
    index = registry_index(registry)
    require_token(registry)
    patterns = rules(args.patterns, registry)
    # Do not scan a resolved Cargo.lock: it intentionally contains private index data.
    for name in ORDER:
        scan_tree(stage / 'backend/crates' / name, patterns)
    # Run one package at a time. Caller waits for index visibility before proceeding.
    if args.package is None and args.execute:
        raise ReleaseInputError('整组入口只用于预演；正式上传必须逐包执行。')
    selection = ['-p', args.package] if args.package else ['--workspace']
    command = ['cargo', 'publish', '--registry', registry, *selection]
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(stage / 'backend/target')
    env[registry_variable(registry, 'CREDENTIAL_PROVIDER')] = 'cargo:token'
    # Capture Cargo output: errors may contain the private URL or credentials.
    run_cargo([*command, '--dry-run'], stage / 'backend', env)
    for package in ([args.package] if args.package else ORDER):
        archive = package_archive(stage / 'backend', package, source['version'])
        scan_archive(archive, patterns, private_index=index)
    if args.execute:
        run_publish(command, stage / 'backend', env, registry, getattr(args, 'max_rate_limit_retries', 3))
    print('Publish completed.' if args.execute else 'Publish dry-run completed.')
    if args.package is None:
        print('整组包构建及归档检查通过；未上传。包组内依赖由 Cargo 临时 registry 验证，'
              '不代表远端 registry 已有这些版本。')


def release(args):
    registries = getattr(args, 'registry', None)
    if not registries:
        raise ReleaseInputError('请使用 --registry crates-io（公共）或 --registry internal（私有）指定目标仓库。')
    registries = list(dict.fromkeys(registries))
    # Validate every destination before any upload can start.
    for registry in registries:
        registry_name(argparse.Namespace(registry=registry))
        registry_index(registry)
        require_token(registry)
    args = argparse.Namespace(**vars(args))
    args.ref = resolve_commit(args.ref)
    for registry in registries:
        print(f'Registry: {registry}', flush=True)
        release_registry(args, registry)


def release_registry(args, registry):
    with tempfile.TemporaryDirectory(prefix='routeweft-release-') as temporary:
        stage = Path(temporary) / 'stage'
        prepare(argparse.Namespace(patterns=getattr(args, 'patterns', None), output=stage, ref=args.ref,
                                   license_file=None, registry=registry))
        if not args.execute and not args.package:
            print('Workspace: dry-run (no upload)', flush=True)
            publish(argparse.Namespace(stage=stage, patterns=getattr(args, 'patterns', None), package=None,
                                       execute=False, registry=registry))
            return
        for position, package in enumerate([args.package] if args.package else ORDER):
            if position and args.execute and registry == 'crates-io':
                wait_for_publish(getattr(args, 'publish_interval', 60), 'crates.io 包间发布间隔')
            print(f'{package}: {"publish" if args.execute else "dry-run"}', flush=True)
            publish(argparse.Namespace(stage=stage, patterns=getattr(args, 'patterns', None), package=package,
                                       execute=args.execute, registry=registry,
                                       max_rate_limit_retries=getattr(args, 'max_rate_limit_retries', 3)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    checker = commands.add_parser('check')
    checker.add_argument('--patterns', type=Path)
    checker.add_argument('--path', type=Path)
    checker.add_argument('--history', action='store_true')
    checker.set_defaults(run=check)
    staging = commands.add_parser('prepare')
    staging.add_argument('--ref', default='HEAD')
    staging.add_argument('--output', type=Path, required=True)
    staging.add_argument('--patterns', type=Path)
    staging.add_argument('--license-file', type=Path)
    staging.set_defaults(run=prepare)
    publisher = commands.add_parser('publish')
    publisher.add_argument('--stage', type=Path, required=True)
    publisher.add_argument('--patterns', type=Path)
    publisher.add_argument('--package', choices=ORDER, required=True)
    publisher.add_argument('--execute', action='store_true', help='upload; default is dry-run')
    publisher.set_defaults(run=publish)
    automatic = commands.add_parser('release', help='prepare and publish all libraries automatically')
    automatic.add_argument('--ref', default='HEAD')
    automatic.add_argument('--patterns', type=Path, help='private address patterns to scan')
    automatic.add_argument('--package', choices=ORDER)
    automatic.add_argument('--execute', action='store_true', help='upload; default is dry-run')
    automatic.set_defaults(run=release)
    for command in (checker, staging, publisher):
        command.add_argument('--registry', required=True, help='crates-io or private Cargo registry alias')
    automatic.add_argument('--registry', action='append', required=True,
                           help='required target: crates-io or private alias; repeat for multiple destinations')
    for command in (publisher, automatic):
        command.add_argument('--max-rate-limit-retries', type=nonnegative_integer, default=3,
                             help='additional attempts on crates.io upload HTTP 429 (default: 3)')
    automatic.add_argument('--publish-interval', type=nonnegative_integer, default=60,
                           help='seconds between crates.io package uploads (default: 60)')
    args = parser.parse_args()
    try:
        load_release_env()
        args.run(args)
    except KeyboardInterrupt:
        parser.exit(130, '\n已取消发布操作，子进程已停止。\n')
    except ReleaseInputError as error:
        parser.exit(1, str(error) + '\n')
    except (ValueError, OSError, KeyError, tarfile.TarError, subprocess.CalledProcessError):
        # Exceptions can embed paths or private configuration; avoid printing their contents.
        parser.exit(1, 'Release operation failed; verify inputs and staging configuration.\n')


if __name__ == '__main__':
    main()
