#!/usr/bin/env python3
"""격리된 실제 컨테이너에서 입력·출력과 검토 종료 코드를 확인한다."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import uuid

SPEC = importlib.util.spec_from_file_location('review_fixture', Path(__file__).with_name('verify-review-action.py'))
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


def verify(image):
    """네트워크 없이 비루트·읽기 전용 root filesystem에서 카탈로그 파일만 사용한다."""
    inspected = subprocess.run(['docker', 'image', 'inspect', image], check=True,
                               capture_output=True, text=True, timeout=30)
    metadata = json.loads(inspected.stdout)[0]
    assert metadata['Config']['User'] == '65532:65532'
    # macOS의 Docker VM에서도 공유되는 checkout 아래에 fixture를 만든다.
    with tempfile.TemporaryDirectory(prefix='.container-smoke-', dir=Path(__file__).resolve().parents[1]) as temporary:
        directory = Path(temporary)
        inputs, outputs = directory/'inputs', directory/'outputs'
        FIXTURE.fixtures(inputs)
        inputs.chmod(0o755)
        outputs.mkdir(mode=0o700)
        host_user = f'{os.getuid()}:{os.getgid()}'

        def run(arguments, expected, *, as_host=False, entrypoint=None):
            name = 'schemagraph-check-'+uuid.uuid4().hex[:12]
            command = ['docker', 'run', '--rm', '--name', name, '--network', 'none', '--read-only',
                       '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                       '--tmpfs', '/tmp:rw,noexec,nosuid,size=16m',
                       '--mount', f'type=bind,src={inputs},dst=/input,readonly']
            if as_host:
                # private 산출물을 호스트가 읽어야 하므로 파일 소유자를 호출자에 맞춘다.
                command.extend(['--user', host_user, '--mount', f'type=bind,src={outputs},dst=/output'])
            if entrypoint:
                command.extend(['--entrypoint', entrypoint])
            command.extend([image, *arguments])
            try:
                result = subprocess.run(command, capture_output=True, text=True, timeout=120)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                subprocess.run(['docker', 'stop', '--time', '1', name], capture_output=True, timeout=15)
                raise
            assert result.returncode == expected, (result.returncode, result.stderr)
            return result.stdout

        version = run(['--version'], 0).strip()
        # 기본 비루트 사용자의 private 파일은 같은 컨테이너 안에서 검증한다.
        default_query = json.loads(run(['-ec',
            'test "$(id -u):$(id -g)" = "65532:65532"; '
            'schemagraph scan --document /input/before.json -o /tmp/graph.json; '
            'test "$(stat -c \'%a:%u\' /tmp/graph.json)" = "600:65532"; '
            'schemagraph query main.customers --graph /tmp/graph.json'],
            0, entrypoint='/bin/sh'))
        assert default_query['subject']['id'] == 'main.customers'
        run(['scan', '--document', '/input/before.json', '-o', '/output/graph.json'], 0, as_host=True)
        output_stat = (outputs/'graph.json').stat()
        assert output_stat.st_uid == os.getuid()
        assert stat.S_IMODE(output_stat.st_mode) == 0o600
        graph = json.loads((outputs/'graph.json').read_text())
        assert {v['id'] for v in graph['vertices']} == {
            'main', 'main.customers', 'main.customers.id', 'main.customers.email'}
        query = json.loads(run(['query', 'main.customers', '--graph', '/output/graph.json'], 0, as_host=True))
        assert query['subject']['id'] == 'main.customers'
        review = json.loads(run(['review', '/input/before.json', '/input/after.json', '--strict',
                                 '--require-complete', '--policy', '/input/policy.toml'], 1))
        assert review['policy']['failed'] is True and review['totalChanges'] == 1
        sarif = json.loads(run(['review', '/input/before.json', '/input/after.json',
                                '--strict', '--format', 'sarif'], 1))
        assert sarif['version'] == '2.1.0' and len(sarif['runs'][0]['results']) == 1
        return {'status': 'ok', 'image_id': metadata['Id'], 'architecture': metadata['Architecture'],
                'os': metadata['Os'], 'user': metadata['Config']['User'], 'version': version,
                'bind_mount_user': host_user, 'output_mode': '0600',
                'network': 'none', 'read_only_root': True,
                'checks': ['default-user-private-roundtrip', 'host-user-scan', 'host-user-query',
                           'policy-exit-1', 'sarif']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = verify(args.image)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, sort_keys=True, indent=2)+'\n')
    print(json.dumps(result, sort_keys=True))


if __name__ == '__main__':
    main()
