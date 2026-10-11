#!/usr/bin/env python3
"""Docker 없이 실행 경계와 private 산출물의 호스트 사용자 계약을 검증한다."""

import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location('container_smoke', Path(__file__).with_name('verify-container.py'))
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)
IMAGE = 'schemagraph:test'
HOST_USER = f'{os.getuid()}:{os.getgid()}'


class DockerFixture:
    """이미지 실행만 대체하고 호스트 산출물의 파일 모드·읽기는 실제로 검사한다."""

    def __init__(self, *, image_user='65532:65532', timeout=False, output_mode=0o600):
        self.image_user = image_user
        self.timeout = timeout
        self.created_mode = output_mode
        self.commands = []
        self.output_mode = None
        self.directory_mode = None

    def run(self, command, **kwargs):
        self.commands.append(command)
        if command[:3] == ['docker', 'image', 'inspect']:
            return subprocess.CompletedProcess(command, 0, json.dumps([{
                'Id': 'sha256:fixture', 'Architecture': 'amd64', 'Os': 'linux',
                'Config': {'User': self.image_user}}]), '')
        if command[:2] == ['docker', 'stop']:
            return subprocess.CompletedProcess(command, 0, '', '')
        assert command[:2] == ['docker', 'run'], command
        args = command[command.index(IMAGE) + 1:]
        if args == ['--version']:
            return subprocess.CompletedProcess(command, 0, 'schemagraph fixture\n', '')
        if '--entrypoint' in command:
            assert '--user' not in command
            assert command[command.index('--entrypoint') + 1] == '/bin/sh'
            assert args[0] == '-ec'
            assert 'id -u' in args[1] and '65532:65532' in args[1]
            assert "stat -c '%a:%u' /tmp/graph.json" in args[1] and '600:65532' in args[1]
            assert 'schemagraph scan' in args[1] and 'schemagraph query' in args[1]
            assert not any('dst=/output' in value for value in command)
            return subprocess.CompletedProcess(command, 0, json.dumps({'subject': {'id': 'main.customers'}}), '')
        if args[0] in ('scan', 'query'):
            # UID가 다르면 0600 파일을 호스트가 읽을 수 없던 실제 CI 실패를 재현한다.
            if '--user' not in command or command[command.index('--user') + 1] != HOST_USER:
                raise PermissionError('private container output is not owned by the host user')
            mount = next(value for value in command if 'dst=/output' in value)
            directory = Path(mount.split('src=', 1)[1].split(',dst=', 1)[0])
            self.directory_mode = stat.S_IMODE(directory.stat().st_mode)
            output = directory/'graph.json'
            if args[0] == 'scan':
                if self.timeout:
                    raise subprocess.TimeoutExpired(command, kwargs['timeout'])
                output.write_text(json.dumps({'vertices': [{'id': name} for name in (
                    'main', 'main.customers', 'main.customers.id', 'main.customers.email')]}))
                output.chmod(self.created_mode)
                self.output_mode = stat.S_IMODE(output.stat().st_mode)
                return subprocess.CompletedProcess(command, 0, '', '')
            assert json.loads(output.read_text())['vertices']
            return subprocess.CompletedProcess(command, 0, json.dumps({'subject': {'id': 'main.customers'}}), '')
        assert args[0] == 'review', args
        if '--policy' in args:
            result = {'policy': {'failed': True}, 'totalChanges': 1}
        else:
            result = {'version': '2.1.0', 'runs': [{'results': [{}]}]}
        return subprocess.CompletedProcess(command, 1, json.dumps(result), '')


class ContainerTests(unittest.TestCase):
    def test_default_and_host_user_workflows_keep_private_files(self):
        docker = DockerFixture()
        with patch.object(SMOKE.subprocess, 'run', side_effect=docker.run):
            report = SMOKE.verify(IMAGE)
        self.assertEqual(report['status'], 'ok')
        self.assertEqual(report['user'], '65532:65532')
        self.assertEqual(report['bind_mount_user'], HOST_USER)
        self.assertEqual(report['output_mode'], '0600')
        self.assertEqual(docker.output_mode, 0o600)
        self.assertEqual(docker.directory_mode, 0o700)
        runs = [command for command in docker.commands if command[:2] == ['docker', 'run']]
        self.assertEqual(len(runs), 6)
        self.assertEqual(sum('--user' in command for command in runs), 2)
        for command in runs:
            for flag in ('--rm', '--read-only', '--cap-drop', '--security-opt'):
                self.assertIn(flag, command)
            self.assertEqual(command[command.index('--network') + 1], 'none')
            self.assertEqual(command[command.index('--cap-drop') + 1], 'ALL')
            self.assertEqual(command[command.index('--security-opt') + 1], 'no-new-privileges')
            self.assertTrue(any('dst=/input,readonly' in value for value in command))

    def test_wrong_default_image_identity_is_rejected_before_running(self):
        docker = DockerFixture(image_user='0:0')
        with patch.object(SMOKE.subprocess, 'run', side_effect=docker.run):
            with self.assertRaises(AssertionError):
                SMOKE.verify(IMAGE)
        self.assertEqual(len(docker.commands), 1)

    def test_smoke_rejects_world_readable_export(self):
        docker = DockerFixture(output_mode=0o644)
        with patch.object(SMOKE.subprocess, 'run', side_effect=docker.run):
            with self.assertRaises(AssertionError):
                SMOKE.verify(IMAGE)
        self.assertEqual(docker.output_mode, 0o644)

    def test_timeout_stops_the_owned_container(self):
        docker = DockerFixture(timeout=True)
        with patch.object(SMOKE.subprocess, 'run', side_effect=docker.run):
            with self.assertRaises(subprocess.TimeoutExpired):
                SMOKE.verify(IMAGE)
        last_run, stop = docker.commands[-2:]
        name = last_run[last_run.index('--name') + 1]
        self.assertEqual(stop, ['docker', 'stop', '--time', '1', name])


if __name__ == '__main__':
    unittest.main()
