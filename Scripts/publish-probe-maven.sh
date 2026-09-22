#!/usr/bin/env bash
set -euo pipefail

# Central Publisher Portal용 Maven repository bundle을 만든다. 기본 동작은
# 로컬 bundle만 생성하며, --upload를 명시해야 외부 네트워크 요청을 한다.
# 비밀값은 MAVEN_SIGNING_KEY/PASSWORD와 CENTRAL_TOKEN_* 환경변수에서만 받는다.

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
PROBE_DIR=${PROBE_SOURCE_DIR:-"$ROOT_DIR/probe"}
VERSION=${PROBE_VERSION:-0.5.0}
GROUP_ID=${PROBE_GROUP:-io.github.ictechgy}
ARTIFACT_ID=schemagraph-probe
UPLOAD=false
PUBLISH=false
DEPLOYMENT_ID=""
COMPARE_REPOSITORY=""
POLL_SECONDS=${CENTRAL_POLL_SECONDS:-10}
POLL_TIMEOUT=${CENTRAL_POLL_TIMEOUT:-1800}
OUTPUT_PATH="$ROOT_DIR/probe/build/central-bundle-$VERSION.zip"

usage() {
    cat <<'EOF'
usage: Scripts/publish-probe-maven.sh [--output PATH] [--upload] [--publish]
       [--deployment-id ID] [--poll-seconds N] [--poll-timeout N]
       [--compare-repository HTTPS_URL]

Builds and validates a signed Central Publisher Portal bundle. The upload
step is disabled unless --upload is supplied explicitly. --publish is also
explicit and acts only after Central reports VALIDATED.
--compare-repository requires all five payloads to match an existing public
Maven repository byte-for-byte before upload. PROBE_SOURCE_DIR can point to
the probe directory of a separate release checkout.

Required for bundle creation:
  MAVEN_SIGNING_KEY       ASCII-armored private key injected into Gradle
  MAVEN_SIGNING_PASSWORD  optional passphrase for the in-memory key

Required with --upload, --publish, or --deployment-id:
  CENTRAL_TOKEN_USERNAME / CENTRAL_TOKEN_PASSWORD  Central user token
EOF
}

while (($#)); do
    case "$1" in
        --output)
            (($# >= 2)) || { echo "error: --output needs a path" >&2; exit 2; }
            OUTPUT_PATH=$2
            shift 2
            ;;
        --upload)
            UPLOAD=true
            shift
            ;;
        --publish)
            PUBLISH=true
            shift
            ;;
        --deployment-id)
            (($# >= 2)) || { echo "error: --deployment-id needs an id" >&2; exit 2; }
            DEPLOYMENT_ID=$2
            shift 2
            ;;
        --compare-repository)
            (($# >= 2)) || { echo "error: --compare-repository needs a URL" >&2; exit 2; }
            COMPARE_REPOSITORY=$2
            shift 2
            ;;
        --poll-seconds)
            (($# >= 2)) || { echo "error: --poll-seconds needs a number" >&2; exit 2; }
            POLL_SECONDS=$2
            shift 2
            ;;
        --poll-timeout)
            (($# >= 2)) || { echo "error: --poll-timeout needs a number" >&2; exit 2; }
            POLL_TIMEOUT=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

command -v gradle >/dev/null 2>&1 || {
    echo "error: Gradle is required; install Gradle 9.6.1 or use the project toolchain" >&2
    exit 2
}
command -v python3 >/dev/null 2>&1 || {
    echo "error: Python 3 is required for bundle validation" >&2
    exit 2
}
command -v zip >/dev/null 2>&1 || {
    echo "error: zip is required to assemble the Central bundle" >&2
    exit 2
}
command -v curl >/dev/null 2>&1 || {
    echo "error: curl is required for --upload" >&2
    exit 2
}

BUILD_BUNDLE=true
if [[ -n "$DEPLOYMENT_ID" && "$UPLOAD" != true ]]; then
    BUILD_BUNDLE=false
fi
if [[ "$BUILD_BUNDLE" == false && -n "$COMPARE_REPOSITORY" ]]; then
    echo "error: --compare-repository requires building a new bundle" >&2
    exit 2
fi
if [[ "$BUILD_BUNDLE" == true && -z ${MAVEN_SIGNING_KEY:-} ]]; then
    echo "error: MAVEN_SIGNING_KEY is required; no signing key file is read" >&2
    exit 2
fi
if [[ "$VERSION" == *-SNAPSHOT || -z "$VERSION" ]]; then
    echo "error: PROBE_VERSION must be a non-SNAPSHOT release version" >&2
    exit 2
fi
case "$POLL_SECONDS" in
    ''|*[!0-9]*) echo "error: --poll-seconds must be a positive integer" >&2; exit 2 ;;
esac
case "$POLL_TIMEOUT" in
    ''|*[!0-9]*) echo "error: --poll-timeout must be a positive integer" >&2; exit 2 ;;
esac
if (( POLL_SECONDS < 1 || POLL_SECONDS > 300 )); then
    echo "error: --poll-seconds must be between 1 and 300" >&2
    exit 2
fi
if (( POLL_TIMEOUT < 1 || POLL_TIMEOUT > 86400 )); then
    echo "error: --poll-timeout must be between 1 and 86400" >&2
    exit 2
fi
if [[ "$PUBLISH" == true && -z "$DEPLOYMENT_ID" && "$UPLOAD" != true ]]; then
    echo "error: --publish requires --upload or --deployment-id" >&2
    exit 2
fi
if [[ ( "$UPLOAD" == true || "$PUBLISH" == true || -n "$DEPLOYMENT_ID" ) \
    && ( -z ${CENTRAL_TOKEN_USERNAME:-} || -z ${CENTRAL_TOKEN_PASSWORD:-} ) ]]; then
    echo "error: Central API operations require CENTRAL_TOKEN_USERNAME and CENTRAL_TOKEN_PASSWORD" >&2
    exit 2
fi

output_dir=$(dirname "$OUTPUT_PATH")
mkdir -p "$output_dir"
OUTPUT_PATH="$(cd "$output_dir" && pwd)/$(basename "$OUTPUT_PATH")"

temp_root=""
trap '[[ -z "$temp_root" ]] || rm -rf "$temp_root"' EXIT

if [[ "$BUILD_BUNDLE" == true ]]; then
temp_root=$(mktemp -d "${TMPDIR:-/tmp}/schemagraph-probe-publish.XXXXXX")
repo_dir="$temp_root/repository"
mkdir -p "$repo_dir"

gradle -p "$PROBE_DIR" \
    publishMavenJavaPublicationToLocalStagingRepository \
    -PprobeGroup="$GROUP_ID" \
    -PprobeVersion="$VERSION" \
    -PprobeRepositoryDir="$repo_dir" \
    -PsigningRequired=true \
    --console=plain --no-daemon

group_path=$(printf '%s' "$GROUP_ID" | tr '.' '/')
artifact_dir="$repo_dir/$group_path/$ARTIFACT_ID/$VERSION"
export ARTIFACT_DIR="$artifact_dir" GROUP_ID ARTIFACT_ID VERSION

python3 - <<'PY'
import hashlib
import os
import pathlib

root = pathlib.Path(os.environ["ARTIFACT_DIR"])
artifact = os.environ["ARTIFACT_ID"]
version = os.environ["VERSION"]
if not root.is_dir():
    raise SystemExit(f"error: signed publication directory is missing: {root}")

payloads = sorted(
    path for path in root.iterdir()
    if path.is_file() and path.suffix in {".jar", ".pom"}
)
if not payloads:
    raise SystemExit("error: no Maven payload files were generated")
for payload in payloads:
    for suffix in (".asc", ".md5", ".sha1"):
        sidecar = payload.with_name(payload.name + suffix)
        if not sidecar.is_file() or sidecar.stat().st_size == 0:
            raise SystemExit(f"error: missing Central sidecar: {sidecar}")
        if suffix in {".md5", ".sha1"}:
            expected = sidecar.read_text(encoding="utf-8").strip().split()[0].lower()
            actual = hashlib.new(suffix[1:], payload.read_bytes()).hexdigest()
            if actual != expected:
                raise SystemExit(f"error: checksum mismatch: {sidecar}")

required = {
    f"{artifact}-{version}.jar",
    f"{artifact}-{version}-sources.jar",
    f"{artifact}-{version}-javadoc.jar",
    f"{artifact}-{version}.pom",
}
if not required.issubset({path.name for path in payloads}):
    raise SystemExit("error: signed bundle is missing thin, source, javadoc, or POM payload")
print(f"validated signed payloads: {len(payloads)}")
PY

mkdir -p "$(dirname "$OUTPUT_PATH")"
if [[ -e "$OUTPUT_PATH" ]]; then
    echo "error: refusing to overwrite existing output: $OUTPUT_PATH" >&2
    exit 2
fi
bundle_root="$temp_root/bundle"
bundle_artifact_dir="$bundle_root/$group_path/$ARTIFACT_ID/$VERSION"
mkdir -p "$bundle_artifact_dir"
for payload in \
    "$artifact_dir/$ARTIFACT_ID-$VERSION.pom" \
    "$artifact_dir/$ARTIFACT_ID-$VERSION.jar" \
    "$artifact_dir/$ARTIFACT_ID-$VERSION"-*.jar; do
    [[ -f "$payload" ]] || continue
    cp "$payload" "$bundle_artifact_dir/"
    for suffix in asc md5 sha1; do
        sidecar="$payload.$suffix"
        [[ -f "$sidecar" ]] || {
            echo "error: signed Central sidecar is missing: $sidecar" >&2
            exit 2
        }
        cp "$sidecar" "$bundle_artifact_dir/"
    done
done
(cd "$bundle_root" && zip -q -r "$OUTPUT_PATH" "$group_path")
test -s "$OUTPUT_PATH"
echo "created Central bundle: $OUTPUT_PATH"
fi

if [[ -n "$COMPARE_REPOSITORY" ]]; then
    python3 - "$OUTPUT_PATH" "$COMPARE_REPOSITORY" "$GROUP_ID" "$ARTIFACT_ID" "$VERSION" <<'PY'
import sys
import urllib.parse
import urllib.request
import zipfile

bundle, repository, group, artifact, version = sys.argv[1:]
url = urllib.parse.urlsplit(repository)
if url.scheme != "https" or not url.hostname or url.username or url.password or url.query or url.fragment:
    raise SystemExit("error: comparison repository must be an HTTPS URL without credentials, query, or fragment")
prefix = f"{group.replace('.', '/')}/{artifact}/{version}/"
names = [f"{artifact}-{version}{suffix}" for suffix in (
    ".pom", ".jar", "-all.jar", "-sources.jar", "-javadoc.jar"
)]
with zipfile.ZipFile(bundle) as archive:
    for name in names:
        path = prefix + name
        if archive.namelist().count(path) != 1:
            raise SystemExit(f"error: missing or duplicate bundle payload: {name}")
        local = archive.read(path)
        with urllib.request.urlopen(repository.rstrip("/") + "/" + path, timeout=60) as response:
            remote = response.read(len(local) + 1)
        if remote != local:
            raise SystemExit(f"error: existing public Maven payload differs: {name}")
        print(f"verified existing public Maven payload: {name}")
PY
fi

if [[ "$UPLOAD" == true ]]; then
    token=$(printf '%s:%s' "$CENTRAL_TOKEN_USERNAME" "$CENTRAL_TOKEN_PASSWORD" | base64 | tr -d '\n')
    upload_url=$(python3 -c 'import sys,urllib.parse; print("https://central.sonatype.com/api/v1/publisher/upload?" + urllib.parse.urlencode({"name": sys.argv[1], "publishingType": "USER_MANAGED"}))' "$GROUP_ID:$ARTIFACT_ID:$VERSION")
    deployment_id=$(curl --fail --silent --show-error \
        --header "Authorization: Bearer $token" \
        --form "bundle=@$OUTPUT_PATH" \
        "$upload_url")
    DEPLOYMENT_ID=$(printf '%s' "$deployment_id" | tr -d '[:space:]')
    [[ -n "$DEPLOYMENT_ID" ]] || {
        echo "error: Central upload returned no deployment id" >&2
        exit 1
    }
    echo "Central deployment uploaded: $DEPLOYMENT_ID"
fi

if [[ -n "$DEPLOYMENT_ID" ]]; then
    token=${token:-$(printf '%s:%s' "$CENTRAL_TOKEN_USERNAME" "$CENTRAL_TOKEN_PASSWORD" | base64 | tr -d '\n')}
    deadline=$(( $(date +%s) + POLL_TIMEOUT ))
    deployment_status=""
    status_json=""
    while :; do
        status_json=$(curl --fail --silent --show-error \
            --request POST \
            --header "Authorization: Bearer $token" \
            "https://central.sonatype.com/api/v1/publisher/status?id=$DEPLOYMENT_ID")
        deployment_status=$(printf '%s' "$status_json" | python3 -c \
            'import json,sys; print(json.load(sys.stdin).get("deploymentState", "UNKNOWN"))')
        echo "Central deployment $DEPLOYMENT_ID: $deployment_status"
        case "$deployment_status" in
            FAILED)
                printf '%s\n' "$status_json" >&2
                exit 1
                ;;
            VALIDATED|PUBLISHED)
                break
                ;;
            PENDING|VALIDATING|PUBLISHING|UNKNOWN)
                if (( $(date +%s) >= deadline )); then
                    echo "error: Central deployment polling timed out" >&2
                    exit 1
                fi
                sleep "$POLL_SECONDS"
                ;;
            *)
                echo "error: unknown Central deployment state: $deployment_status" >&2
                printf '%s\n' "$status_json" >&2
                exit 1
                ;;
        esac
    done

    if [[ "$PUBLISH" == true ]]; then
        if [[ "$deployment_status" == "PUBLISHED" ]]; then
            printf '%s\n' "$status_json"
            echo "Central deployment is already PUBLISHED; no publish request was needed"
        else
            [[ "$deployment_status" == "VALIDATED" ]] || {
                echo "error: --publish requires Central state VALIDATED, got $deployment_status" >&2
                exit 1
            }
            curl --fail --silent --show-error \
                --request POST \
                --header "Authorization: Bearer $token" \
                "https://central.sonatype.com/api/v1/publisher/deployment/$DEPLOYMENT_ID"
            echo
            while :; do
                status_json=$(curl --fail --silent --show-error \
                    --request POST \
                    --header "Authorization: Bearer $token" \
                    "https://central.sonatype.com/api/v1/publisher/status?id=$DEPLOYMENT_ID")
                deployment_status=$(printf '%s' "$status_json" | python3 -c \
                    'import json,sys; print(json.load(sys.stdin).get("deploymentState", "UNKNOWN"))')
                echo "Central deployment $DEPLOYMENT_ID: $deployment_status"
                case "$deployment_status" in
                    PUBLISHED)
                        printf '%s\n' "$status_json"
                        break
                        ;;
                    FAILED)
                        printf '%s\n' "$status_json" >&2
                        exit 1
                        ;;
                    *)
                        if (( $(date +%s) >= deadline )); then
                            echo "error: Central publish polling timed out" >&2
                            exit 1
                        fi
                        sleep "$POLL_SECONDS"
                        ;;
                esac
            done
        fi
    else
        printf '%s\n' "$status_json"
        echo "Central deployment is validated; run --publish explicitly to release it"
    fi
fi
