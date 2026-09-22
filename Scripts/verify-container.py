#!/usr/bin/env python3
"""격리된 실제 컨테이너에서 입력·출력과 검토 종료 코드를 확인한다."""

import argparse
import importlib.util
import json
from pathlib import Path
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
        outputs.mkdir(mode=0o777)
        outputs.chmod(0o777)

        def run(arguments, expected):
            name = 'schemagraph-check-'+uuid.uuid4().hex[:12]
            command = ['docker', 'run', '--rm', '--name', name, '--network', 'none', '--read-only',
                       '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                       '--tmpfs', '/tmp:rw,noexec,nosuid,size=16m',
                       '--mount', f'type=bind,src={inputs},dst=/input,readonly',
                       '--mount', f'type=bind,src={outputs},dst=/output', image, *arguments]
            try:
                result = subprocess.run(command, capture_output=True, text=True, timeout=120)
            except (subprocess.TimeoutExpired, KeyboardInterrupt):
                subprocess.run(['docker', 'stop', '--time', '1', name], capture_output=True, timeout=15)
                raise
            assert result.returncode == expected, (result.returncode, result.stderr)
            return result.stdout

        version = run(['--version'], 0).strip()
        run(['scan', '--document', '/input/before.json', '-o', '/output/graph.json'], 0)
        graph = json.loads((outputs/'graph.json').read_text())
        assert {v['id'] for v in graph['vertices']} == {
            'main', 'main.customers', 'main.customers.id', 'main.customers.email'}
        query = json.loads(run(['query', 'main.customers', '--graph', '/output/graph.json'], 0))
        assert query['subject']['id'] == 'main.customers'
        review = json.loads(run(['review', '/input/before.json', '/input/after.json', '--strict',
                                 '--require-complete', '--policy', '/input/policy.toml'], 1))
        assert review['policy']['failed'] is True and review['totalChanges'] == 1
        sarif = json.loads(run(['review', '/input/before.json', '/input/after.json',
                                '--strict', '--format', 'sarif'], 1))
        assert sarif['version'] == '2.1.0' and len(sarif['runs'][0]['results']) == 1
        return {'status': 'ok', 'image_id': metadata['Id'], 'architecture': metadata['Architecture'],
                'os': metadata['Os'], 'user': metadata['Config']['User'], 'version': version,
                'network': 'none', 'read_only_root': True,
                'checks': ['scan', 'query', 'policy-exit-1', 'sarif']}


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
