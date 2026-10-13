#!/bin/bash
# 실제 gh로 위임하지 않는 오프라인 상태 모형이다. 자격 증명·네트워크는 쓰지 않는다.
set -euo pipefail
printf '%s\n' "$*" >> "$CASE_DIR/commands"
[[ $* != *--clobber* ]] || exit 90
header() { printf 'HTTP/2.0 %s Mock\nContent-Type: application/json\r\n\r\n' "$1"; }
if [[ $1 == api ]]; then
  [[ $# == 7 && $3 == --include && $4 == --method && $5 == GET && $6 == --jq ]] || exit 90
  endpoint=$2
  case "$endpoint" in
    repos/ictechgy/schemagraph/git/ref/tags/v0.7.1)
      calls=$(cat "$CASE_DIR/tag-calls"); calls=$((calls + 1)); echo "$calls" > "$CASE_DIR/tag-calls"
      [[ $SCENARIO != missing-tag ]] || { header 404; exit 1; }
      [[ $SCENARIO != bad-http ]] || { echo unavailable; exit 1; }
      header 200
      if [[ $SCENARIO == annotated || $SCENARIO == tag-cycle ]]; then
        printf 'tag\t%s\n' 2222222222222222222222222222222222222222
      elif [[ $SCENARIO == wrong-tag || ( ( $SCENARIO == moved-before-upload || $SCENARIO == moved-before-create ) && $calls -ge 2 ) || ( $SCENARIO == moved-after-upload && $calls -ge 3 ) ]]; then
        printf 'commit\t%s\n' 3333333333333333333333333333333333333333
      else printf 'commit\t%s\n' "$BUILT_COMMIT"; fi
      ;;
    repos/ictechgy/schemagraph/git/tags/2222222222222222222222222222222222222222)
      header 200
      if [[ $SCENARIO == tag-cycle ]]; then printf 'tag\t%s\n' 2222222222222222222222222222222222222222
      else printf 'commit\t%s\n' "$BUILT_COMMIT"; fi
      ;;
    repos/ictechgy/schemagraph/releases/tags/v0.7.1)
      case "$SCENARIO" in
        forbidden) header 403; exit 1;;
        server-error) header 500; exit 1;;
      esac
      [[ -f $CASE_DIR/release ]] || { header 404; echo '{"message":"Not Found"}'; exit 1; }
      header 200
      [[ $SCENARIO != filter-error ]] || exit 1
      case "$SCENARIO" in
        wrong-release) printf '1\tv0.1.0\tfalse\tfalse\tfalse\n';;
        draft) printf '1\tv0.7.1\ttrue\tfalse\tfalse\n';;
        prerelease) printf '1\tv0.7.1\tfalse\ttrue\tfalse\n';;
        immutable-*) printf '1\tv0.7.1\tfalse\tfalse\ttrue\n';;
        unknown-immutable) printf '1\tv0.7.1\tfalse\tfalse\t\n';;
        *) printf '1\tv0.7.1\tfalse\tfalse\tfalse\n';;
      esac
      ;;
    'repos/ictechgy/schemagraph/releases/1/assets?per_page=100&page='*)
      [[ $SCENARIO != missing-inventory ]] || { header 404; exit 1; }
      header 200
      if [[ $SCENARIO == pagination && $endpoint == *page=1 ]]; then
        printf '100\n'
        for ((i=0;i<100;i++)); do printf 'unrelated-%s\tuploaded\t1\tsha256:unrelated\n' "$i"; done
      else
        wc -l < "$CASE_DIR/assets" | tr -d ' '
        cat "$CASE_DIR/assets"
      fi
      ;;
    *) exit 90;;
  esac
elif [[ $1 == release && ( $2 == create || $2 == upload ) && $3 == v0.7.1 ]]; then
  operation=$2
  printf '%s\n' "$operation" >> "$CASE_DIR/writes"
  shift 3
  arguments=("$@")
  if [[ $operation == create ]]; then
    [[ " ${arguments[*]} " == *' --verify-tag '* ]] || exit 90
    [[ " ${arguments[*]} " == *' --title schemagraph 0.7.1 '* ]] || exit 90
    [[ " ${arguments[*]} " == *' --generate-notes '* ]] || exit 90
    [[ " ${arguments[*]} " != *' --notes-file '* ]] || exit 90
    [[ $SCENARIO != create-failed ]] || exit 1
    touch "$CASE_DIR/release"
    [[ $SCENARIO != create-race ]] || exit 1
  fi
  while [[ $# -gt 0 && $1 != --repo ]]; do
    file=$1; shift
    [[ -f $file && $file != */* ]] || exit 90
    # helper가 이미 존재한 이름을 다시 올리려 하면 모형도 덮어쓰지 않고 실패한다.
    ! awk -F '\t' -v name="$file" '$1 == name { found=1 } END { exit !found }' "$CASE_DIR/assets" || exit 1
    grep -F "$file" "$CASE_DIR/expected" >> "$CASE_DIR/assets"
    if [[ $SCENARIO == upload-partial ]]; then exit 1; fi
  done
  [[ ${1-} == --repo && ${2-} == ictechgy/schemagraph ]] || exit 90
  if [[ $SCENARIO == upload-conflict ]]; then sed 's/sha256:/sha256:conflict/' "$CASE_DIR/assets" > "$CASE_DIR/new-assets"; mv "$CASE_DIR/new-assets" "$CASE_DIR/assets"; exit 1; fi
  [[ $SCENARIO != upload-uncertain && $SCENARIO != create-uncertain ]] || exit 1
else
  exit 90
fi
