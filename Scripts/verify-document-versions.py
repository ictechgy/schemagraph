#!/usr/bin/env python3
"""전송 버전·형식만 바꿔도 그래프가 같고, 모르는 의미와 불완전 전송은 거부하는지 확인한다."""

import copy
import json
from pathlib import Path
import subprocess
import sys


def run(binary, *arguments, expected=0):
    """검증 실패를 숨기지 않되 외부 DB 접속 정보는 인자로 받지 않는다."""
    result = subprocess.run([str(binary), *map(str, arguments)], capture_output=True, text=True)
    if result.returncode != expected:
        raise RuntimeError(f"Catalog compatibility check failed ({result.returncode}, expected {expected}): {result.stderr}")
    return result.stdout


def ndjson(document):
    """독립적인 전송 조립으로 엔진 자체 writer와 같은 구현을 재검사하지 않는다."""
    header = {key: value for key, value in document.items() if key not in {"schemas", "limitations"}}
    records = [{"type": "document", **header, "limitations": []}]
    for schema in document["schemas"]:
        records.append({"type": "schema", "name": schema["name"]})
        for collection, kind in [("objects", "object"), ("routines", "routine")]:
            records.extend({"type": kind, "schema": schema["name"], "data": value} for value in schema[collection])
    records.append({"type": "limitations", "data": document["limitations"]})
    return "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records)


def check(binary, source, prefix):
    """협상 출력부터 v2 소비와 실패 경로까지 실제 CLI 경로를 검증한다."""
    capabilities = json.loads(run(binary, "document-capabilities"))
    assert capabilities["supportedVersions"] == [1, 2]
    v1, v2 = Path(str(prefix) + "-v1.json"), Path(str(prefix) + "-v2.json")
    baseline = Path(str(prefix) + "-v1.graph.json")
    graph = Path(str(prefix) + "-v2.graph.json")
    run(binary, "scan", "--document", source, "--emit-document", v1, "-o", baseline)
    run(binary, "scan", "--document", v1, "--emit-document", v2, "--document-version", 2, "-o", graph)
    expected = json.loads(baseline.read_text())
    assert json.loads(graph.read_text()) == expected, "v2 changed graph semantics"
    document = json.loads(v2.read_text())
    assert document["version"] == 2 and "reader" not in document
    transport = Path(str(prefix) + "-v2.ndjson")
    transport.write_text(ndjson(document))
    run(binary, "scan", "--document", transport, "-o", graph)
    assert json.loads(graph.read_text()) == expected, "NDJSON changed graph semantics"
    run(binary, "diff", v1, v2, "--strict")
    run(binary, "diff", v1, transport, "--strict")

    rejected = Path(str(prefix) + "-rejected.json")
    for invalid in [
        {**document, "version": 4294967297},
        {**document, "required_features": ["unsupported-meaning-v9"]},
        {**document, "reader": "removed-field"},
    ]:
        rejected.write_text(json.dumps(invalid))
        run(binary, "scan", "--document", rejected, "-o", "-", expected=2)
    transport.write_text("\n".join(ndjson(document).splitlines()[:-1]) + "\n")
    run(binary, "scan", "--document", transport, "-o", "-", expected=2)

    optional = copy.deepcopy(document)
    optional["future_optional_field"] = True
    rejected.write_text(json.dumps(optional))
    result = json.loads(run(binary, "scan", "--document", rejected, "-o", "-"))
    assert any("future_optional_field" in note for note in result["limitations"])


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve(), Path(sys.argv[2]), Path(sys.argv[3]))
