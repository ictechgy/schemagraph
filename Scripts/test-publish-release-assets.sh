#!/bin/bash
# 게시 경로는 gh 모형만 호출한다. 배포 자격 증명 없이 실행하는 회귀 검사다.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"
cp "$root/Scripts/tests/gh-release-mock.sh" "$scratch/bin/gh"
chmod +x "$scratch/bin/gh"
export PATH="$scratch/bin:$PATH"
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN || true
export BUILT_COMMIT=1111111111111111111111111111111111111111
passed=0

check_case() {
  export SCENARIO=$1 CASE_DIR="$scratch/$1"
  local expected=$2 writes=$3 file digest result=0 actual
  mkdir "$CASE_DIR"
  (
    cd "$CASE_DIR"
    : > commands; : > writes; : > assets; : > expected
    echo 0 > tag-calls
    printf 'version=0.7.1\ncommit=%s\n' "$BUILT_COMMIT" > RELEASE-METADATA.txt
    for file in schemagraph-0.7.1-{x86_64-unknown-linux-gnu,aarch64-apple-darwin}.tar.gz schemagraph-probe-0.7.1-all.jar SHA256SUMS; do
      printf 'synthetic artifact: %s\n' "$file" > "$file"
      digest=$(shasum -a 256 "$file"); digest=${digest%% *}
      printf '%s\tuploaded\t%s\tsha256:%s\n' "$file" "$(wc -c < "$file" | tr -d ' ')" "$digest" >> expected
    done
    case "$SCENARIO" in absent|create-*|moved-before-create) ;; *) touch release;; esac
    case "$SCENARIO" in complete|immutable-complete|pagination|duplicate) cat expected > assets;;
      partial|size-conflict|state-conflict|digest-conflict|missing-digest) head -1 expected > assets;;
      late-conflict) tail -1 expected > assets;;
      extra-asset) printf 'user-notes.txt\tuploaded\t1\tsha256:unrelated\n' > assets;;
    esac
    case "$SCENARIO" in
      digest-conflict|late-conflict) sed 's/sha256:/sha256:conflict/' assets > altered; mv altered assets;;
      size-conflict) awk -F '\t' 'BEGIN {OFS="\t"} {$3=999; print}' assets > altered; mv altered assets;;
      state-conflict) sed 's/uploaded/starter/' assets > altered; mv altered assets;;
      missing-digest) awk -F '\t' '{printf "%s\t%s\t%s\t\n",$1,$2,$3}' assets > altered; mv altered assets;;
      duplicate) head -1 expected >> assets;;
    esac
    "$BASH" "$root/Scripts/publish-release-assets.sh" v0.7.1 "$BUILT_COMMIT" > output 2>&1 || result=$?
    if [[ $expected == pass ]]; then
      [[ $result == 0 ]] || { cat output >&2; exit 1; }
      grep -q 'Verified all four release assets' output
      # 같은 바이트 재실행은 읽기만 하고 성공해야 한다.
      if [[ $SCENARIO == absent ]]; then "$BASH" "$root/Scripts/publish-release-assets.sh" v0.7.1 "$BUILT_COMMIT" >> output 2>&1; fi
    else
      [[ $result != 0 ]] || { echo "Unexpected success: $SCENARIO" >&2; exit 1; }
    fi
    actual=$(wc -l < writes | tr -d ' ')
    [[ $actual == "$writes" ]] || { echo "Unexpected writes in $SCENARIO: $actual" >&2; cat commands >&2; exit 1; }
    ! grep -E 'clobber|release (delete|edit)|api .*--method (POST|PATCH|PUT|DELETE)|/settings|/secrets|RELEASE-METADATA' commands
    if [[ $SCENARIO == partial ]]; then
      ! grep '^release upload ' commands | grep -F 'schemagraph-0.7.1-x86_64-unknown-linux-gnu.tar.gz'
    fi
    if [[ $SCENARIO == extra-asset ]]; then grep -q '^user-notes.txt' assets; fi
  )
  passed=$((passed + 1))
  printf 'PASS %s\n' "$SCENARIO"
}

check_case absent pass 1
check_case empty pass 1
check_case partial pass 1
check_case complete pass 0
check_case annotated pass 1
check_case immutable-complete pass 0
check_case pagination pass 0
check_case extra-asset pass 1
check_case create-race pass 2
check_case create-uncertain pass 1
check_case upload-uncertain pass 1
for scenario in missing-tag wrong-tag tag-cycle bad-http filter-error moved-before-create forbidden server-error wrong-release draft prerelease immutable-empty unknown-immutable missing-inventory digest-conflict late-conflict size-conflict state-conflict missing-digest duplicate moved-before-upload; do
  check_case "$scenario" fail 0
done
check_case create-failed fail 1
check_case upload-partial fail 1
check_case upload-conflict fail 1
check_case moved-after-upload fail 1
printf '%s offline release publication cases passed.\n' "$passed"
