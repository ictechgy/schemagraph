#!/usr/bin/env python3
"""Provision official IBM disposable containers and run one selected fixture."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
VERIFY = ROOT / "Scripts/verify-ibm-fixtures.py"
DB2_IMAGE = "icr.io/db2_community/db2@sha256:e48ca934c29ab72d9e8b602929e6a52b64841cb7f8ef9426ee8cf5e1bc3e5fb3"
INFORMIX_IMAGE = "icr.io/informix/informix-developer-database@sha256:5eb0d519188d1c5d655775343974cec2063244b7a0f60b4e4dda8809ca9b376e"
DB2_JCC_SHA = "04150111a29370247d162c3f05b8e938bf1314f19a63ad9a5302d88dd478e8e8"
INFORMIX_JDBC_SHA = "152fe3380e414261266d7bde6bacae348c94b6db0cf16f969d1094368449cec7"


SECRET_VALUES: list[str] = []


def scrub(text: str) -> str:
    for value in SECRET_VALUES:
        text = text.replace(value, "<redacted>")
    return text


def run(command: list[str], label: str, timeout: int = 120, input_text: str | None = None) -> str:
    try:
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True,
                                timeout=timeout, check=False, input=input_text)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"{label} failed to start or timed out ({type(error).__name__})") from error
    if result.returncode:
        detail = scrub((result.stderr or result.stdout).strip())
        raise RuntimeError(f"{label} failed with exit code {result.returncode}: {detail[-1000:]}")
    return result.stdout.strip()


def docker(command: list[str], label: str, timeout: int = 120, input_text: str | None = None) -> str:
    return run(["docker", *command], label, timeout, input_text)


def image_available(image: str) -> bool:
    return subprocess.run(["docker", "image", "inspect", image], capture_output=True,
                          text=True, check=False).returncode == 0


def image(image_ref: str) -> None:
    if not image_available(image_ref):
        docker(["pull", "--platform", "linux/amd64", image_ref], f"pull {image_ref}", timeout=900)


def port(container: str, private_port: int) -> int:
    value = docker(["port", container, f"{private_port}/tcp"], f"port lookup {container}")
    return int(value.rsplit(":", 1)[1])


def require_running(container: str) -> None:
    state = docker(["inspect", "--format", "{{.State.Running}}", container], "inspect fixture container")
    if state != "true":
        detail = docker(["logs", "--tail", "20", container], "read fixture startup result")
        raise RuntimeError(f"fixture container stopped during startup: {scrub(detail[-1500:])}")


def cleanup(container: str, run_id: str) -> None:
    result = subprocess.run(["docker", "inspect", "--format",
        '{{ index .Config.Labels "org.schemagraph.ibm-fixtures" }}', container],
        capture_output=True, text=True, check=False)
    if result.returncode == 0 and result.stdout.strip() == run_id:
        docker(["rm", "-f", "-v", container], "remove owned fixture container")


def wait_db2(container: str) -> None:
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        require_running(container)
        # 공식 entrypoint는 DB 생성 뒤 설정을 바꾸고 서버를 재시작한다.
        # 초기 접속 성공만으로 준비 완료를 판단하면 검증 중 연결이 끊긴다.
        startup = docker(["logs", "--tail", "120", container], "check Db2 setup completion")
        if "(*) Setup has completed." not in startup:
            time.sleep(5)
            continue
        result = subprocess.run([
            "docker", "exec", container, "bash", "-lc",
            "su - db2inst1 -c 'db2 connect to SGTEST >/dev/null 2>&1'",
        ], capture_output=True, text=True, check=False)
        if result.returncode == 0:
            return
        time.sleep(5)
    raise RuntimeError("Db2 container did not become ready within 600 seconds")


def wait_informix(container: str) -> None:
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        require_running(container)
        result = subprocess.run(["docker", "exec", container, "sh", "-c", '"$INFORMIXDIR/bin/onstat" -'],
                                capture_output=True, text=True, check=False)
        if result.returncode == 0 and "On-Line" in result.stdout:
            return
        time.sleep(5)
    raise RuntimeError("Informix container did not become ready within 600 seconds")


def verify_driver(path: Path, expected: str, label: str) -> None:
    if not path.is_file():
        raise RuntimeError(f"{label} JDBC jar is missing: {path}")
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != expected:
        raise RuntimeError(f"{label} JDBC jar checksum mismatch")


def create_db_informix(container: str) -> None:
    docker(["exec", "-i", "-u", "informix", container, "sh", "-c", '\"$INFORMIXDIR/bin/dbaccess\" - -'],
           "create Informix fixture database", input_text="CREATE DATABASE sgfix WITH LOG;\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", type=Path)
    parser.add_argument("probe_jar", type=Path)
    parser.add_argument("--database", choices=("db2", "informix", "all"), default="all")
    parser.add_argument("--output-dir", type=Path, default=Path("engine/target/scaling-validation/ibm"))
    parser.add_argument("--memory", default="3g", help="Docker memory limit for one disposable server")
    parser.add_argument("--java", default=os.environ.get("JAVA", "java"))
    parser.add_argument("--drivers-dir", type=Path,
                        default=Path(os.environ.get("IBM_JDBC_DRIVERS", "engine/target/scaling-validation/drivers")))
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not args.engine.is_file() or not args.probe_jar.is_file():
        print("error: engine and probe jar must exist", file=sys.stderr)
        return 2
    selected = ("db2", "informix") if args.database == "all" else (args.database,)
    db2_driver = args.drivers_dir / "db2-jcc.jar"
    informix_driver = args.drivers_dir / "informix-jdbc.jar"
    containers: list[str] = []
    run_id = secrets.token_hex(8)
    try:
        if "db2" in selected:
            verify_driver(db2_driver, DB2_JCC_SHA, "Db2")
            image(DB2_IMAGE)
            name = f"sg-ibm-db2-{run_id}"
            containers.append(name)
            password = secrets.token_urlsafe(18)
            SECRET_VALUES.append(password)
            docker(["run", "-d", "--name", name, "--platform", "linux/amd64", "--privileged",
                    "--memory", args.memory, "--shm-size=256m",
                    "--label", f"org.schemagraph.ibm-fixtures={run_id}",
                    "-e", "LICENSE=accept", "-e", f"DB2INST1_PASSWORD={password}",
                    "-e", "DBNAME=SGTEST", "-p", "127.0.0.1::50000", DB2_IMAGE],
                   "start Db2 container", timeout=120)
            print("Db2 fixture container started; waiting for the database", flush=True)
            wait_db2(name)
            db2_port = port(name, 50000)
            db2_url = f"jdbc:db2://127.0.0.1:{db2_port}/SGTEST"
            run([sys.executable, str(VERIFY), str(args.engine), str(args.probe_jar),
                 "--database", "db2", "--require-all", "--java", args.java,
                 "--output-dir", str(args.output_dir.resolve() / f"db2-{run_id}"),
                 "--db2-url", db2_url, "--db2-user", "db2inst1", "--db2-password", password,
                 "--db2-jdbc", str(db2_driver), "--db2-schema", "SGFIX"],
                "Db2 fixture verification", timeout=900)
            cleanup(name, run_id)
            containers.remove(name)
            print("Db2 fixture passed", flush=True)

        if "informix" in selected:
            verify_driver(informix_driver, INFORMIX_JDBC_SHA, "Informix")
            image(INFORMIX_IMAGE)
            name = f"sg-ibm-informix-{run_id}"
            containers.append(name)
            docker(["run", "-d", "--name", name, "--platform", "linux/amd64", "--privileged",
                    "--memory", args.memory,
                    "--label", f"org.schemagraph.ibm-fixtures={run_id}",
                    "-e", "LICENSE=accept", "-e", "TYPE=oltp", "-p", "127.0.0.1::9088",
                    INFORMIX_IMAGE], "start Informix container", timeout=120)
            print("Informix fixture container started; waiting for the database", flush=True)
            wait_informix(name)
            password = secrets.token_urlsafe(18)
            SECRET_VALUES.append(password)
            docker(["exec", "-i", "-u", "0", name, "chpasswd"], "set ephemeral Informix credentials",
                   input_text=f"informix:{password}\n")
            create_db_informix(name)
            informix_port = port(name, 9088)
            server = docker(["exec", name, "sh", "-c", 'printf "%s" "$INFORMIXSERVER"'], "read Informix server name")
            informix_url = f"jdbc:informix-sqli://127.0.0.1:{informix_port}/sgfix:INFORMIXSERVER={server};"
            run([sys.executable, str(VERIFY), str(args.engine), str(args.probe_jar),
                 "--database", "informix", "--require-all", "--java", args.java,
                 "--output-dir", str(args.output_dir.resolve() / f"informix-{run_id}"),
                 "--informix-url", informix_url, "--informix-user", "informix",
                 "--informix-password", password, "--informix-jdbc", str(informix_driver)],
                "Informix fixture verification", timeout=900)
            cleanup(name, run_id)
            containers.remove(name)
            print("Informix fixture passed", flush=True)
        print(f"IBM container fixture verification passed: {', '.join(selected)}")
        return 0
    except RuntimeError as error:
        print(f"error: {scrub(str(error))}", file=sys.stderr)
        for container in containers:
            metrics = subprocess.run(["docker", "exec", container, "cat", "/sys/fs/cgroup/memory.events"],
                                     capture_output=True, text=True, check=False)
            if metrics.returncode == 0:
                print("Container memory events: " + metrics.stdout.strip(), file=sys.stderr)
            result = subprocess.run(["docker", "logs", "--tail", "40", container],
                                    capture_output=True, text=True, check=False)
            print(scrub((result.stdout + result.stderr)[-3000:]), file=sys.stderr)
        return 1
    finally:
        for container in containers:
            try:
                cleanup(container, run_id)
            except RuntimeError as error:
                print(f"error: fixture cleanup failed: {scrub(str(error))}", file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
