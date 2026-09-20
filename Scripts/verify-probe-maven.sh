#!/usr/bin/env bash
set -euo pipefail

# 로컬 Maven 저장소 publication과 외부 Gradle consumer를 검증한다.
# Gradle은 생성된 POM과 Maven repository layout을 읽으므로 Maven CLI가
# 설치되지 않은 개발 환경에서도 소비자 경계를 확인할 수 있다.

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
PROBE_DIR="$ROOT_DIR/probe"
VERSION=${PROBE_VERSION:-0.3.0}
GROUP_ID=${PROBE_GROUP:-io.github.ictechgy}
ARTIFACT_ID=schemagraph-probe

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "error: required command '$1' was not found" >&2
        exit 2
    }
}

require_command gradle
require_command python3

case "$VERSION" in
    ""|*-SNAPSHOT|*/*|*\\*)
        echo "error: PROBE_VERSION must be a non-SNAPSHOT Maven version" >&2
        exit 2
        ;;
esac

temp_root=$(mktemp -d "${TMPDIR:-/tmp}/schemagraph-probe-maven.XXXXXX")
trap 'rm -rf "$temp_root"' EXIT
repo_dir="$temp_root/repository"
consumer_dir="$temp_root/consumer"
mkdir -p "$repo_dir" "$consumer_dir"

gradle -p "$PROBE_DIR" \
    publishMavenJavaPublicationToLocalStagingRepository \
    -PprobeGroup="$GROUP_ID" \
    -PprobeVersion="$VERSION" \
    -PprobeRepositoryDir="$repo_dir" \
    --console=plain

group_path=$(printf '%s' "$GROUP_ID" | tr '.' '/')
artifact_dir="$repo_dir/$group_path/$ARTIFACT_ID/$VERSION"
export ARTIFACT_DIR="$artifact_dir"
export GROUP_ID ARTIFACT_ID VERSION

python3 - <<'PY'
import hashlib
import os
import pathlib
import zipfile
import xml.etree.ElementTree as ET

root = pathlib.Path(os.environ["ARTIFACT_DIR"])
if not root.is_dir():
    raise SystemExit(f"error: publication directory is missing: {root}")

version = os.environ["VERSION"]
artifact = os.environ["ARTIFACT_ID"]
required = [
    f"{artifact}-{version}.jar",
    f"{artifact}-{version}-sources.jar",
    f"{artifact}-{version}-javadoc.jar",
    f"{artifact}-{version}.pom",
]
for name in required:
    path = root / name
    if not path.is_file() or path.stat().st_size == 0:
        raise SystemExit(f"error: required publication file is missing or empty: {path}")
    for suffix in (".md5", ".sha1"):
        checksum = path.with_name(path.name + suffix)
        if not checksum.is_file():
            raise SystemExit(f"error: required checksum is missing: {checksum}")
        expected = checksum.read_text(encoding="utf-8").strip().split()[0].lower()
        actual = hashlib.new(suffix[1:], path.read_bytes()).hexdigest()
        if expected != actual:
            raise SystemExit(f"error: checksum mismatch: {checksum}")

all_jar = root / f"{artifact}-{version}-all.jar"
if not all_jar.is_file() or all_jar.stat().st_size == 0:
    raise SystemExit(f"error: executable all classifier is missing: {all_jar}")
for suffix in (".md5", ".sha1"):
    sidecar = all_jar.with_name(all_jar.name + suffix)
    if not sidecar.is_file():
        raise SystemExit(f"error: all classifier checksum is missing: {sidecar}")
    expected = sidecar.read_text(encoding="utf-8").strip().split()[0].lower()
    actual = hashlib.new(suffix[1:], all_jar.read_bytes()).hexdigest()
    if expected != actual:
        raise SystemExit(f"error: all classifier checksum mismatch: {sidecar}")

thin_entries = set(zipfile.ZipFile(root / f"{artifact}-{version}.jar").namelist())
all_entries = set(zipfile.ZipFile(all_jar).namelist())
javadoc_entries = set(zipfile.ZipFile(root / f"{artifact}-{version}-javadoc.jar").namelist())
if "schemagraph/probe/MainKt.class" not in thin_entries:
    raise SystemExit("error: thin jar does not contain the probe main class")
if any(entry.startswith("com/fasterxml/jackson/") for entry in thin_entries):
    raise SystemExit("error: thin jar unexpectedly embeds Jackson classes")
if "schemagraph/probe/MainKt.class" not in all_entries:
    raise SystemExit("error: all jar does not contain the probe main class")
if not any(entry.endswith(".html") for entry in javadoc_entries):
    raise SystemExit("error: javadoc jar does not contain HTML documentation")

namespace = {"m": "http://maven.apache.org/POM/4.0.0"}
pom = ET.parse(root / f"{artifact}-{version}.pom").getroot()
text = lambda path: pom.findtext(path, default="", namespaces=namespace)
expected = {
    "groupId": os.environ["GROUP_ID"],
    "artifactId": artifact,
    "version": version,
}
for key, value in expected.items():
    if text(f"m:{key}") != value:
        raise SystemExit(f"error: POM {key} is not {value!r}")
for key in ("name", "description", "url"):
    if not text(f"m:{key}"):
        raise SystemExit(f"error: POM {key} is empty")
for section in ("licenses", "developers", "scm"):
    if pom.find(f"m:{section}", namespace) is None:
        raise SystemExit(f"error: POM {section} metadata is missing")
dependencies = {
    (node.findtext("m:groupId", namespaces=namespace), node.findtext("m:artifactId", namespaces=namespace))
    for node in pom.findall("m:dependencies/m:dependency", namespace)
}
for coordinate in {
    ("com.fasterxml.jackson.module", "jackson-module-kotlin"),
    ("com.h2database", "h2"),
    ("org.xerial", "sqlite-jdbc"),
}:
    if coordinate not in dependencies:
        raise SystemExit(f"error: POM runtime dependency is missing: {coordinate}")
print(f"validated publication: {os.environ['GROUP_ID']}:{artifact}:{version}")
PY

cat >"$consumer_dir/settings.gradle.kts" <<EOF
rootProject.name = "probe-maven-consumer"
EOF
cat >"$consumer_dir/build.gradle.kts" <<EOF
plugins {
    application
}

repositories {
    maven { url = uri("$repo_dir") }
    mavenCentral()
}

dependencies {
    implementation("$GROUP_ID:$ARTIFACT_ID:$VERSION")
}

application {
    mainClass.set("schemagraph.probe.MainKt")
}

tasks.named<JavaExec>("run") {
    args("--url", "jdbc:h2:mem:probe_maven_consumer", "-o", "$consumer_dir/catalog.json")
}
EOF

gradle -p "$consumer_dir" run --console=plain
test -s "$consumer_dir/catalog.json"
echo "external Maven repository consumer and probe CLI succeeded"

if command -v mvn >/dev/null 2>&1; then
    echo "Maven CLI detected: $(mvn -version | head -1)"
else
    echo "note: Maven CLI is unavailable; Gradle Maven-repository consumer verification was used" >&2
fi
