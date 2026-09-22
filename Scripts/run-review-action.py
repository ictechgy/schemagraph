#!/usr/bin/env python3
"""GitHub Actions에서 동일 스냅샷의 검토 결과와 종료 코드를 보존한다."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile

MAX_INPUT_BYTES = 128 * 1024 * 1024
MAX_REPORT_BYTES = 64 * 1024 * 1024


class ReviewCancelled(Exception):
    """취소를 일반 분석 실패와 구분해 CLI의 130 계약을 보존한다."""


def terminate_owned(process):
    """종료와 경합하더라도 이미 만든 프로세스를 회수하고 다른 PID는 건드리지 않는다."""
    if process.poll() is None:
        try:
            if os.name == 'posix':
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except ProcessLookupError:
            # poll 뒤에 종료됐더라도 자식의 종료 상태는 반드시 회수한다.
            return process.wait()
    return process.wait()


def fingerprint(path):
    """세 번의 렌더링 사이에 입력이 바뀌면 혼합 보고서를 성공으로 내지 않는다."""
    digest = hashlib.sha256()
    size = 0
    with path.open('rb') as stream:
        while chunk := stream.read(65536):
            size += len(chunk)
            if size > MAX_INPUT_BYTES:
                raise ValueError('Review input exceeds 128 MiB; narrow the collected scope')
            digest.update(chunk)
    return digest.hexdigest()


def input_file(value, workspace, label):
    """입력은 checkout 내부의 실제 파일로 제한하고 URL을 파일로 오인하지 않는다."""
    if not value or any(c in value for c in '\r\n\0') or '://' in value:
        raise ValueError(f'{label} must name a file inside GITHUB_WORKSPACE')
    path = (workspace / value).resolve()
    if not path.is_relative_to(workspace) or not path.is_file():
        raise ValueError(f'{label} must name a file inside GITHUB_WORKSPACE')
    if path.stat().st_size > MAX_INPUT_BYTES:
        raise ValueError(f'{label} exceeds 128 MiB')
    return path


def execute(command, stdout, stderr, timeout):
    """명령은 인자 배열로 실행하고 시간 초과 시 소유한 프로세스만 종료한다."""
    with stdout.open('wb') as out, stderr.open('wb') as err:
        process = subprocess.Popen(command, stdout=out, stderr=err,
                                   start_new_session=os.name == 'posix')
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            terminate_owned(process)
            raise ValueError('Review command timed out; inspect the input size and traversal limits') from None
        except BaseException:
            terminate_owned(process)
            raise
    if stdout.stat().st_size > MAX_REPORT_BYTES:
        raise ValueError('Review report exceeds 64 MiB; narrow the reviewed change set')
    return code


def engine_path(workspace, directory):
    """공개 전 옵션을 오래된 배포본에 넘기지 않도록 기본값은 고정 action 소스 빌드다."""
    selected = os.environ.get('SG_REVIEW_ENGINE', '')
    if selected:
        path = (workspace / selected).resolve()
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError('engine-path must name an executable schemagraph binary')
        return path
    cargo = shutil.which('cargo')
    if cargo is None:
        raise ValueError('Install Rust 1.96.0 or provide engine-path')
    version = subprocess.run([cargo, '--version'], capture_output=True, text=True, timeout=30)
    prefix = [cargo]
    if version.returncode or not re.match(r'cargo 1\.96\.0\b', version.stdout):
        rustup = shutil.which('rustup')
        if rustup is None:
            raise ValueError('Install Rust 1.96.0 or provide engine-path')
        code = execute([rustup, 'toolchain', 'install', '1.96.0', '--profile', 'minimal'],
                       directory/'toolchain.log', directory/'toolchain.stderr', 300)
        if code:
            raise ValueError('Rust toolchain setup failed; see toolchain.stderr')
        prefix += ['+1.96.0']
    repository = Path(__file__).resolve().parents[1]
    target = directory/'build'
    code = execute(prefix + ['build', '--manifest-path', str(repository/'engine/Cargo.toml'),
                             '--locked', '--release', '-p', 'schemagraph-cli',
                             '--target-dir', str(target)],
                   directory/'build.log', directory/'build.stderr', 900)
    if code:
        raise ValueError('Source build failed; provide native build prerequisites or engine-path (see build.stderr)')
    return target/'release'/('schemagraph.exe' if os.name == 'nt' else 'schemagraph')


def write_outputs(values):
    """워크플로 명령 주입이 가능한 줄바꿈은 출력 값으로 내보내지 않는다."""
    path = os.environ.get('GITHUB_OUTPUT')
    if path:
        with Path(path).open('a', encoding='utf-8') as output:
            for key, value in values.items():
                if any(c in str(value) for c in '\r\n\0'):
                    raise ValueError('Action output contains an invalid newline')
                output.write(f'{key}={value}\n')


def run_review():
    """검토 발견(1)과 불완전·실행 실패(2)를 구분해 caller의 CI gate에 전달한다."""
    workspace = Path(os.environ['GITHUB_WORKSPACE']).resolve()
    temporary = Path(os.environ['RUNNER_TEMP']).resolve()
    inputs = {name: input_file(os.environ.get('SG_REVIEW_'+name.upper(), ''), workspace, name)
              for name in ('before', 'after')}
    for name in ('policy', 'baseline'):
        if value := os.environ.get('SG_REVIEW_'+name.upper(), ''):
            inputs[name] = input_file(value, workspace, name)
    original = {key: fingerprint(path) for key, path in inputs.items()}
    complete = os.environ.get('SG_REVIEW_COMPLETE', 'true')
    if complete not in ('true', 'false'):
        raise ValueError('require-complete must be true or false')
    timeout = int(os.environ.get('SG_REVIEW_TIMEOUT', '180'))
    if not 1 <= timeout <= 1800:
        raise ValueError('timeout-seconds must be between 1 and 1800')
    selected = os.environ.get('SG_REVIEW_OUTPUT', '')
    if selected:
        if any(c in selected for c in '\r\n\0'):
            raise ValueError('output-directory contains invalid characters')
        directory = (workspace/selected).resolve()
        if not any(directory.is_relative_to(root) for root in (workspace, temporary)):
            raise ValueError('output-directory must be inside the workspace or runner temporary directory')
        directory.mkdir(parents=True, exist_ok=False)
    else:
        directory = Path(tempfile.mkdtemp(prefix='schemagraph-review-', dir=temporary))
    engine = engine_path(workspace, directory)
    arguments = [str(engine), 'review', str(inputs['before']), str(inputs['after']), '--strict']
    if complete == 'true':
        arguments.append('--require-complete')
    for name in ('policy', 'baseline'):
        if name in inputs:
            arguments += ['--'+name, str(inputs[name])]
    if as_of := os.environ.get('SG_REVIEW_AS_OF', ''):
        arguments += ['--as-of', as_of]
    outputs, codes = {}, []
    for format_name, suffix in (('json', 'json'), ('markdown', 'md'), ('sarif', 'sarif')):
        output = directory/f'review.{suffix}'
        code = execute(arguments + ['--format', format_name], output,
                       directory/f'{format_name}.stderr', timeout)
        if code == 130:
            raise ReviewCancelled()
        if code not in (0, 1, 2):
            raise ValueError('Review process did not return a supported decision; inspect its stderr artifact')
        if {key: fingerprint(path) for key, path in inputs.items()} != original:
            raise ValueError('Input snapshots or policy changed during review; rerun against fixed inputs')
        if format_name in ('json', 'sarif'):
            try:
                value = json.loads(output.read_text(encoding='utf-8'))
                if format_name == 'json' and value.get('kind') != 'review':
                    raise ValueError('not a review report')
                if format_name == 'sarif' and (value.get('version') != '2.1.0' or not isinstance(value.get('runs'), list)):
                    raise ValueError('not a SARIF report')
            except (ValueError, AttributeError):
                raise ValueError('Review did not emit a valid report; inspect its stderr artifact') from None
        outputs[format_name] = str(output)
        codes.append(code)
    if len(set(codes)) != 1:
        raise ValueError('Review renderers returned inconsistent decisions')
    outputs['exit_code'] = str(codes[0])
    write_outputs(outputs)
    if summary := os.environ.get('GITHUB_STEP_SUMMARY'):
        with Path(summary).open('a', encoding='utf-8') as destination:
            destination.write(Path(outputs['markdown']).read_text(encoding='utf-8'))
    (directory/'action-result.json').write_text(json.dumps({'exit_code': codes[0], 'input_sha256': original,
                                                          'reports': outputs}, sort_keys=True, indent=2)+'\n')
    print(f'schemagraph review completed with exit code {codes[0]}')
    return codes[0]


def main():
    def requested_cancel(_signal, _frame):
        raise ReviewCancelled()

    # CI runner가 보내는 종료 요청도 예외 경로에서 자식을 회수하게 한다.
    signal.signal(signal.SIGINT, requested_cancel)
    signal.signal(signal.SIGTERM, requested_cancel)
    try:
        return run_review()
    except (KeyboardInterrupt, ReviewCancelled):
        write_outputs({'exit_code': '130'})
        print('schemagraph review action cancelled', file=sys.stderr)
        return 130
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        write_outputs({'exit_code': '2'})
        message = str(error).replace('\r', ' ').replace('\n', ' ')
        print('schemagraph review action failed: '+message, file=sys.stderr)
        return 2

if __name__ == '__main__':
    raise SystemExit(main())
