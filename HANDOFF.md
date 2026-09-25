# HANDOFF.md

_최종 갱신: 2026-09-25 KST · Claude_

## 목표

경쟁 재조사(2026-09-25)에서 정한 보강 네 가지를 구현·리뷰·머지하고 **v0.6.0으로 공개
배포·설치 검증까지 완료**했다. 남은 승인 작업은 없다. 다음 제품 작업은 미정이다.

## 현재 상태

- 프로젝트: `/Users/jinhongan/Desktop/schemagraph`. main은 깨끗하며 이 문서 갱신 PR 외 진행 중인 브랜치는 없다.
- 공개 버전: [v0.6.0](https://github.com/ictechgy/schemagraph/releases/tag/v0.6.0).
  태그·6개 crate·Go 바이너리의 내장 커밋은 `e5f3b9cdfe453b6c6ee872e1283fd6a18af592b2`
  ([PR #29](https://github.com/ictechgy/schemagraph/pull/29) 머지 커밋).
  - GitHub Release [빌드](https://github.com/ictechgy/schemagraph/actions/runs/36138711491),
    Pages [실행](https://github.com/ictechgy/schemagraph/actions/runs/36139205607),
    Central [실행](https://github.com/ictechgy/schemagraph/actions/runs/36139607863)
    (deployment `be128a33-79b8-446f-9cd5-ecd8459411c4` **PUBLISHED**). 완료 기록은 [#30](https://github.com/ictechgy/schemagraph/pull/30).
  - 이전 공개본 v0.5.1(`fce98ec`, [#22](https://github.com/ictechgy/schemagraph/pull/22))·v0.5.0(`94d1db8`)은 보존했다.
- v0.6.0에 들어간 변경(각 PR에 리뷰 지적과 처리 내역 코멘트가 있다):
  - [#24](https://github.com/ictechgy/schemagraph/pull/24) `search`(names/summary 점진 공개), MCP 도구
    `search`·`dead`·`cycles`·`lint`·`stats`와 스냅샷 resources(`schemagraph://graph/summary`, `schemagraph://skill`).
  - [#25](https://github.com/ictechgy/schemagraph/pull/25) lint `fk-type-mismatch`(blocking),
    `table-without-primary-key`·`duplicate-index`(advisory), `schema_metadata.catalog_complete`,
    **PG native·Go의 `pk_position` 미수집 결함 수정**과 엔진 보정.
  - [#26](https://github.com/ictechgy/schemagraph/pull/26) `unused`, `usage.scans`, 파티션 부모 usage 제외,
    `track_counts=off` 처리(세 수집기).
  - [#27](https://github.com/ictechgy/schemagraph/pull/27) `openlineage`(RunEvent 2-0-2, `schemas/openlineage`에 공식 스키마 고정).
  - 새 CI 단계: `verify-schema-lint.py`·`verify-unused.py`(실제 PG, native·Go·JDBC),
    `verify-openlineage.py`(실제 PG, 공식 스키마·그래프 양방향 대조).
- v0.5.1(2026-09-25)은 isthmus `facts` 명령(#18)과 PG materialized view 컬럼 수집 수정(#20)을 배포했다.
  isthmus 쪽 명령 표기 수정([isthmus#113](https://github.com/ictechgy/isthmus/pull/113))도 머지됐다.
- 로컬 Colima는 fixture 검증 때만 켰다가 다시 껐다(원래 꺼져 있음).

## 중요한 결정과 경계

- **공통 원칙(AGENTS.md):** 그래프 의미론은 Rust 엔진에만, 결정적 JSON, reader가 부여한 id만,
  `dead`·`unused`는 사실 보고이지 삭제 허가가 아니다. 사용 기록 없음과 관측된 0은 다르다.
- **lint:** 기존 규칙의 `lint --strict` 결과를 바꾸지 않도록 새 규칙 중 PK 부재·중복 인덱스는 advisory다
  (`blockingCount`로만 strict 판정). PK 부재는 `catalog_complete`일 때만 확정하고, 중복 인덱스는
  access method를 수집하지 않아 항상 미확인이다. FK 타입 비교는 PG `serial` 계열만 정수형으로 접는다
  (JDBC가 `serial`/`bigserial`로 보고).
- **unused:** PG 테이블 `reads`는 튜플 수라 빈 채로 폴링되는 테이블을 잡지 못한다 — 테이블은 `usage.scans`로 판정한다.
  index-only scan을 받은 테이블, 파티션 부모(usage 없음)는 후보가 아니다. `track_counts=off`면 usage를 싣지 않는다.
- **openlineage:** 값 계보는 subtype 없는 DIRECT, INDIRECT(JOIN·FILTER)는 단일 SELECT인 뷰에만 싣는다
  (루틴은 조건의 소속 문장을 그래프가 모른다). 부분 분석은 `schemagraphAnalysis` job facet. runId는 내용 기반 UUID v8.
- **MCP `serve`:** 보존 정책은 명시한 `--config/--retain/--as-of`만 읽고 작업 디렉터리의 `schemagraph.toml`은 읽지 않는다.
  `review`는 두 스냅샷이 필요해 노출하지 않는다.
- **수집기 차이(알려진 것):** JDBC는 PK 인덱스도 인덱스 정점으로 수집한다(native·Go는 제외).
  MV 이름은 `pg_matviews`라 권한과 무관하게 보이고 MV 컬럼은 권한 규칙을 따른다.
- **배포:** Pages/Central은 기존 릴리스 태그에서 수동 게시한다. 기존 버전·태그·JAR·POM을 덮어쓰지 않는다.
  DML CI의 0.4.3 다운로드는 의도된 비교 기준이다.

## 효과가 있었던 방법

- 기대값을 엔진 출력이 아니라 DB 시스템 카탈로그·DDL·실행한 워크로드에서 도출하고, 같은 scan 그래프와
  양방향 대조했다. 이 방식으로 PG MV 컬럼 누락, `pk_position` 미수집, 빈 폴링 테이블 오판을 찾았다.
- 전제는 먼저 실제 DB로 확인했다(예: `pg_stat_user_tables`에서 빈 테이블 seq_scan 3·seq_tup_read 0,
  파티션 부모 카운터 0, JDBC의 serial 표기). 추측으로 정한 기대값은 여러 번 틀렸다.
- 회귀 검사는 옛 동작으로 잠시 되돌려 실패하는지 확인했다(예: `serve`의 cwd 설정 파일 회귀).
- 기능마다 `/code-review high` → 지적 검증 → 반영/보류 근거를 PR 코멘트로 남김 → CI → merge commit.
  리뷰가 실제 결함(과대 귀속, strict 호환성, 튜플/스캔 혼동)을 여러 번 잡았다.
- 목적이 섞인 변경은 전체본을 보관한 뒤 중간 상태를 만들어 커밋을 나누고, 커밋마다 테스트를 돌렸다.
- 릴리스는 v0.5.1 절차를 그대로 재사용했다(아래 다음 단계 4번).

## 효과가 없었던 방법 (반복하지 말 것)

- `cargo fmt` 이후 원문 문자열로 치환하는 스크립트는 줄바꿈이 달라 실패한다 — 치환 전 현재 모양을 다시 읽는다.
- zsh는 `$var`를 단어로 나누지 않는다 — 목록은 파일이나 `${=var}`로 넘긴다.
- `verify-fixtures.sh`의 로컬 종료 코드 1은 `set -e` 상태의 EXIT trap(없는 컨테이너 `docker rm`) 때문일 수 있다.
  마지막 `verify-fixtures: OK`와 `주의:` 줄로 판정한다. 로컬엔 Oracle JDBC가 없어 Oracle 프로브만 건너뛴다.
- 저장소의 fixture 디렉터리는 `Fixtures/`(대문자)다. macOS가 소문자 경로도 열어 주지만 git 경로는 대문자다.
- `pg_stat_reset()`은 슈퍼유저 함수다 — 검증에서는 관리자 역할로 실행한다.
- MCP 테스트에서 인자 오류는 조정 스레드가, 도구 결과는 워커가 써서 응답 순서가 바뀔 수 있다 — id로 정렬한다.
- "JDBC는 PG 사용 통계를 수집하지 않는다", "그래프에 절 역할이 없다"는 가정은 틀렸다(JDBC는 수집하고,
  `origins.role`에 join/predicate 등이 있다). 코드로 확인하기 전에 결론을 내지 않는다.
- PR의 2~3초 `fail`은 취소된 push 실행이다. `gh run list --commit <sha>`로 `pull_request` 실행 성공을 확인한다.
- Pages 첫 게시의 HTTP 503은 같은 실행의 재시도로 해결된 적이 있다. 새 버전 게시로 해결할 일이 아니다.

## 주요 파일과 근거 위치

| 경로 | 먼저 볼 내용 |
| --- | --- |
| [AGENTS.md](AGENTS.md), [DESIGN.md](DESIGN.md) | 작업 규칙·설계·에이전트 출력 계약 |
| [COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md) | 2026-09-25 재조사, 남은 경쟁 공백, 하지 않을 것 |
| [RELEASE-NOTES.md](RELEASE-NOTES.md), [INSTALLATION.md](INSTALLATION.md), [MAVEN.md](MAVEN.md) | 릴리스 변경·설치·게시 절차 |
| [ANALYSIS.md](ANALYSIS.md), [CATALOG.md](CATALOG.md), [SUPPORT-MATRIX.md](SUPPORT-MATRIX.md) | 명령 계약(unused·openlineage·lint 포함)·document 필드·검증 범위 |
| `Scripts/verify-*.py`, `Scripts/verify-fixtures.sh`, `.github/workflows/` | 실DB 검증과 CI |

로컬 근거 루트는 `~/Library/Application Support/schemagraph/verification/`이다.

- `v0.6.0-20260925/`: `plan.json`(complete), `publish.log`, `public-crates.json`, `published-crates/`,
  `installed-features/result.json`(설치본 15개 검사), `public-verification.json`,
  검증 스크립트 `verify-{crates,installed,public}.py`(다음 릴리스의 출발점). `registry-install/`은 0.6.0 설치본.
- `v0.5.1-20260925/`, `v0.5.0-20260922/`: 이전 릴리스 근거(빌드 캐시는 지우고 `.crate`·설치본만 보존).
- 과거 상세 이력: [v0.5.0 시점 HANDOFF](https://github.com/ictechgy/schemagraph/blob/739ac6170286d1f768062406ef5c699661f30630/HANDOFF.md),
  [v0.6.0 직후 HANDOFF](https://github.com/ictechgy/schemagraph/blob/c0621814714072715bbc5b2420b648291ac5a0e1/HANDOFF.md). 옛 "미배포" 문구는 당시 상태다.

## Blocker와 열린 질문

없음. 추가 게시나 실패 CI 재실행이 남아 있지 않다.

- isthmus persistence 조인이 npm에 배포되면 CI의 `verify-bridge-facts.py`에 `--isthmus`를 붙일 수 있다.
- OpenLineage 출력의 실제 카탈로그(Marquez·DataHub·OpenMetadata) 적재와 MySQL `unused`의 실DB 검증은 하지 않았다.

## 다음 단계

1. `git status --short`, `git branch --show-current`로 상태를 확인한다. 배포는 반복하지 않는다.
2. 새 기능은 [COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md)의 남은 공백 중 사용자가 고른 것부터 한다.
   우선순위 후보: migration 파일의 lock·rewrite 위험과 의존 영향 결합, PR의 컬럼 단위 변경 등급,
   저장 스냅샷 대 라이브 scan drift 워크플로, 중복 인덱스 확정을 위한 access method 수집.
3. 수집기(native·Go·JDBC)를 바꾸면 세 수집기 검증(`verify-collection-scope.py`, `verify-schema-lint.py`,
   `verify-unused.py`)과 Docker fixture(`verify-fixtures.sh`, Colima 필요 — 끝나면 다시 끈다)를 함께 돌리고,
   golden 차이는 줄마다 검토한 뒤 갱신한다.
4. 릴리스 절차: 버전·RELEASE-NOTES PR → CI → merge commit 머지 → 병합 트리 = PR 헤드 확인 → 병합 커밋 main CI →
   lightweight 태그 push(GitHub Release, 본문을 RELEASE-NOTES 항목으로 교체) → `cargo publish --workspace --locked`
   (별도 target) → `verify-crates.py` → registry 설치 + `verify-installed.py` → Pages → Central → repo1 조회 →
   `verify-public.py`(openjdk@17을 PATH에) → 근거 정리 → HANDOFF 완료 PR.

## 재개 프롬프트

```text
/Users/jinhongan/Desktop/schemagraph에서 HANDOFF.md와 AGENTS.md를 읽어줘.
v0.6.0(search·MCP 확장, lint 규칙, unused, openlineage) 배포·공개 설치 검증은 완료됐어.
먼저 git 상태를 확인하고 로컬 변경을 보존한 뒤, 내가 추가로 요청하는 작업부터 이어가줘.
```
