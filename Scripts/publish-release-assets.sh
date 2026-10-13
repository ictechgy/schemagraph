#!/bin/bash
# 검증된 같은 바이트만 재사용하고, 기존 릴리스의 자산·태그·설정은 덮어쓰지 않는다.
set -euo pipefail
export GH_HOST=github.com GH_PROMPT_DISABLED=1

fail() { printf 'Release publication stopped: %s\n' "$*" >&2; exit 1; }
[[ $# == 2 ]] || fail 'expected TAG and built commit'
tag=$1
commit=$2
repo=ictechgy/schemagraph
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'invalid stable version tag'
[[ $commit =~ ^[0-9a-f]{40}$ ]] || fail 'expected a full built commit SHA'
version=${tag#v}
files=("schemagraph-$version-x86_64-unknown-linux-gnu.tar.gz"
       "schemagraph-$version-aarch64-apple-darwin.tar.gz"
       "schemagraph-probe-$version-all.jar" SHA256SUMS)
sizes=()
digests=()
for file in "${files[@]}"; do
  [[ -f $file && ! -L $file && -s $file ]] || fail "missing regular artifact: $file"
  sizes+=("$(wc -c < "$file" | tr -d '[:space:]')")
  digest=$(shasum -a 256 "$file")
  digests+=("sha256:${digest%% *}")
done

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# gh가 반환한 실제 HTTP 상태만 사용한다. 인증·전송 오류를 릴리스 부재로 취급하지 않는다.
api() {
  local result=0 status
  gh api "$1" --include --method GET --jq "$2" > "$scratch/response" 2> "$scratch/error" || result=$?
  IFS= read -r status < "$scratch/response" || true
  status=${status%$'\r'}
  [[ $status =~ ^HTTP/[0-9.]+\ ([0-9]{3})(\ |$) ]] || fail 'API response has no HTTP status'
  api_status=${BASH_REMATCH[1]}
  api_body=$(awk 'body { sub(/\r$/, ""); print; next } /^\r?$/ { body=1 }' "$scratch/response")
  if [[ $api_status == 200 ]]; then
    [[ $result == 0 ]] || fail 'API response or JSON filtering failed'
  elif [[ $api_status != 404 ]]; then
    fail "GitHub API returned HTTP $api_status"
  fi
}

# 가벼운 태그와 주석 태그를 모두 커밋까지 풀고, 빌드한 소스와 매번 대조한다.
verify_tag() {
  local kind sha depth=0
  api "repos/$repo/git/ref/tags/$tag" '.object | [.type, .sha] | @tsv'
  [[ $api_status == 200 ]] || fail 'release tag does not exist; this helper never creates tags'
  IFS=$'\t' read -r kind sha <<< "$api_body"
  while [[ $kind == tag ]]; do
    [[ $sha =~ ^[0-9a-f]{40}$ && $depth -lt 8 ]] || fail 'invalid or excessively nested annotated tag'
    api "repos/$repo/git/tags/$sha" '.object | [.type, .sha] | @tsv'
    [[ $api_status == 200 ]] || fail 'annotated tag could not be resolved'
    IFS=$'\t' read -r kind sha <<< "$api_body"
    depth=$((depth + 1))
  done
  [[ $kind == commit && $sha == "$commit" ]] || fail 'remote tag does not match the built commit'
}

read_release() {
  api "repos/$repo/releases/tags/$tag" '[.id, .tag_name, .draft, .prerelease, .immutable] | @tsv'
  [[ $api_status == 200 ]] || return 1
  local actual_tag draft prerelease
  IFS=$'\t' read -r release_id actual_tag draft prerelease immutable <<< "$api_body"
  [[ $release_id =~ ^[1-9][0-9]*$ && $actual_tag == "$tag" ]] || fail 'unexpected release identity'
  [[ $draft == false && $prerelease == false ]] || fail 'existing release must be published and stable'
  [[ $immutable == true || $immutable == false ]] || fail 'release immutability is unknown'
}

# 먼저 전체 목록을 검사한다. 충돌을 발견한 뒤 다른 파일부터 올리는 부분 성공을 피한다.
inventory() {
  local page=1 count rows='' file matches name state size digest index
  while :; do
    api "repos/$repo/releases/$release_id/assets?per_page=100&page=$page" \
      '(if type != "array" then error("expected assets array") else . end) | ([length] | @tsv), (.[] | [.name, .state, .size, (.digest // "")] | @tsv)'
    [[ $api_status == 200 ]] || fail 'release asset list disappeared'
    count=${api_body%%$'\n'*}
    [[ $count =~ ^[0-9]+$ && $count -le 100 ]] || fail 'invalid asset page'
    if [[ $api_body == *$'\n'* ]]; then rows+="${api_body#*$'\n'}"$'\n'; fi
    [[ $count == 100 ]] || break
    [[ $page -lt 20 ]] || fail 'asset inventory exceeds safety bound'
    page=$((page + 1))
  done
  missing=()
  missing_count=0
  for ((index=0; index<${#files[@]}; index++)); do
    file=${files[$index]}
    matches=$(printf '%s' "$rows" | awk -F '\t' -v name="$file" '$1 == name { print }')
    if [[ -z $matches ]]; then
      missing[$missing_count]=$file
      missing_count=$((missing_count + 1))
      continue
    fi
    [[ $matches != *$'\n'* ]] || fail "duplicate asset name: $file"
    IFS=$'\t' read -r name state size digest <<< "$matches"
    [[ $state == uploaded && $size == "${sizes[$index]}" && $digest == "${digests[$index]}" ]] ||
      fail "existing asset differs or has no verified SHA-256: $file"
  done
}

verify_tag
if ! read_release; then
  verify_tag
  # --verify-tag는 기본 gh 동작의 암묵적 태그 생성을 금지한다. 실패도 재조회로 먼저 대조한다.
  if ! gh release create "$tag" "${files[@]}" --repo "$repo" --title "schemagraph $version" \
    --generate-notes --verify-tag; then
    printf 'Create did not report success; reconciling remote state before continuing.\n' >&2
  fi
  read_release || fail 'release creation failed; inspect the run before retrying'
fi
inventory
if [[ ${missing_count} -gt 0 ]]; then
  [[ $immutable == false ]] || fail 'immutable release is missing artifacts; no settings are changed'
  verify_tag
  # 이름 충돌 시 gh는 실패한다. --clobber·삭제·수정으로 복구하지 않는다.
  if ! gh release upload "$tag" "${missing[@]}" --repo "$repo"; then
    printf 'Upload did not report success; verifying the remote inventory.\n' >&2
  fi
  read_release || fail 'release disappeared after upload'
  inventory
  [[ ${missing_count} == 0 ]] || fail 'upload incomplete; retry only with the same validated artifact bytes'
fi
verify_tag
printf 'Verified all four release assets for %s at %s.\n' "$tag" "$commit"
