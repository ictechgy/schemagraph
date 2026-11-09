# HANDOFF.md

## 현재 상태 (2026-11-09, PR #59 리뷰 반영 후)

- 발행본은 **0.9.0**이다. 태그 `v0.9.0` = `fad8e9d`([#58](https://github.com/ictechgy/schemagraph/pull/58)).
  그 커밋의 [CI](https://github.com/ictechgy/schemagraph/actions/runs/41583763434)·
  [SQL Server/Oracle](https://github.com/ictechgy/schemagraph/actions/runs/41583763488)·
  [IBM](https://github.com/ictechgy/schemagraph/actions/runs/41583763549)·
  [DML](https://github.com/ictechgy/schemagraph/actions/runs/41583763689) 검증이 모두 성공했다.
- [PR #59](https://github.com/ictechgy/schemagraph/pull/59)(브랜치 `ccr-37904670-hxdjsx`)가 미발행 변경을 담고 있다.
  RELEASE-NOTES `# Unreleased`가 그 목록의 정본이다: facts의 스키마 필터·교환 불가 식별자 범위 공백과
  SQLite 예약 관계 선언, COPY 테이블·컬럼 간선, SQLite 수집기 3종 수정(네이티브·Go·JDBC), 대소문자 접기
  방언의 관계·routine 해석과 몸체 캐시 지문, 서버 대소문자 규칙 수집(`context.lower_case_table_names`·
  `collation`), 프로브 메시지 영어화, JDBC 파티션·외부 테이블과 방언별(대소문자 구분) 시스템 스키마,
  CI의 isthmus 0.14.0 실검증.
- 채널별 게시 현황은 RELEASE-NOTES.md 끝의 "Publication channels" 표가 정본이다.
  crates.io에는 0.7.1·0.8.0·0.8.1이 없고, Maven Central에는 0.7.1이 없다(2026-11-03 조회).
  GitHub Release v0.1.0–v0.3.0에는 첨부 아카이브가 없다(2026-11-09 조회). 누락 버전은 다시 게시하지 않는다.
- isthmus `compatibility.json`은 아직 schemagraph 0.7.0을 고정한다. 갱신은 isthmus 저장소의 일이다.

### PR #59 리뷰에서 처리한 것

GLM 리뷰(`packet-ask`)를 이 세션에서 쓸 수 없어, 같은 체크리스트로 독립된 Claude 리뷰를 대신 돌렸다.

- facts: trim하면 비는 컬럼 이름(공백·NBSP·BOM)이 isthmus 문서 전체를 종료 코드 2로 거부시키던 문제.
  그 컬럼만 빼고 `unexchangeable-identifiers:`로 센다. 스키마 이름 때문에 뺀 것만 `catalog-coverage:`다.
- facts: SQLite `sqlite_master`·`sqlite_sequence` 사용이 거짓 미선언 error가 되던 문제. 존재가 확실한
  예약 관계를 usr 없이 선언한다(그래프 정점이 아니다). 두 건 모두 `Scripts/verify-bridge-facts.py`의
  실제 isthmus 0.14.0 사례로 검증한다.
- parser: 대소문자 접기 방언(MySQL lctn≠0 포함)의 몸체 캐시 지문, COPY 컬럼 목록 간선, MySQL routine
  접기가 DB 한정자를 접지 않게, 터키어·아제르바이잔어·CP1254 CI collation은 ASCII 접기로 확정하지 않게.
- probe(JDBC): PostgreSQL·MySQL 시스템 스키마 비교를 네이티브·Go처럼 대소문자 구분으로.
- 재리뷰 blocker 2건(0c48cc5 기준): SQLite 비ASCII 관계 이름에서 몸체 캐시 지문이 테이블 모양 변화를 놓치던
  29036a3의 회귀(SQLite는 양쪽에 같은 catalog_key를 쓰는 tables 경로로 되돌림), 예약 관계 사실에 계약이 없어
  `isthmus diff --contracts`가 같은 문서도 review-required로 판정하던 문제(빈 계약 + 한계를 싣고, 0c48cc5의
  capture 검사·문서 변경을 되돌림). 후자는 verify-bridge-facts.py의 계약 자기 비교로 검증한다.
- SQLite 골든: 인라인 제약 완전성 수정의 의도된 결과(FK `complete: true`, `catalog_complete: true`)로 갱신.
- 문서: 채널 표의 v0.1.0–v0.3.0 바이너리 표기, facts 한계 접두사·예약 관계 설명.

### 처리하지 않은 것과 이유

- 스키마 필터가 모든 스키마를 담아도 `catalog-coverage:`가 붙는 보수적 과대 보고는 유지하고 문서화했다.
  정확히 세려면 CatalogDocument에 "필터 밖 스키마 존재" 필드가 필요하다(계약 변경). 네이티브 MySQL reader는
  URL의 DB를 필터로 기록하므로 다른 DB를 `db.table`로 쓰는 경우를 위해 늘 붙는 것이 맞다.
- `sqlite_schema`·`sqlite_temp_schema`는 SQLite 3.33.0부터 있다. 앱 런타임 버전을 모르므로 늘 선언하고,
  그 이전 런타임에서는 거짓 match가 된다는 점을 문서화했다. 예약 관계를 쓰지 않는 앱에는 관계마다
  `relation-decl-without-use` 경고가 나지만 죽은 스키마 후보가 아니다.
- `sqlite_stat*`는 ANALYZE 이력에 달려 선언하지 않는다. JDBC SQLite 문서는 테이블 DDL이 없어
  `sqlite_sequence`를 판정하지 못한다(`sqlite-system-relations:` 메모로 알린다).
- MySQL `lower_case_table_names`·SQL Server collation 수집은 단위 테스트로만 검증된다. 실서버 fixture
  (`Scripts/verify-fixtures.sh`의 Docker 단계)는 이 값을 수집하지만 기대값으로 단언하지는 않는다.
  단언을 더하려면 Docker 데몬이 있는 환경에서 실제 값을 보고 고정해야 한다.
- JDBC 프로브의 JUnit 실행은 이 세션에서 Maven Central 429로 의존성을 받지 못해 못 돌렸다.
  바뀐 `CatalogScopeTest.systemSchemasDependOnTheDialect`만 컴파일된 클래스를 직접 실행해 확인했다(수정 전 실패).

## 머지 뒤 다음 할 일

1. PR #59 머지 뒤 CI·DB fixture job이 main에서 모두 녹색인지 확인한다.
2. 다음 릴리스는 **0.10.0**이다 — `CollectionContext`에 공개 필드가 늘었다. [RELEASING.md](RELEASING.md) 절차로
   GitHub Release·crates.io·Maven Central을 **각각** 게시하고, 채널 표를 실제 조회 결과로 갱신한다.
   crates.io 누락분(0.7.1·0.8.0·0.8.1)은 다시 게시하지 않고 0.10.0이 그 변경을 담는다.
3. isthmus 쪽 `compatibility.json`의 schemagraph 고정을 올리는 일은 isthmus 저장소에 맡긴다.

## Blocker와 열린 질문 (2026-11-09)

- 수집기 사이에 남은 차이는 SUPPORT-MATRIX.md "Known differences between collectors"에 적었다
  (JDBC는 PK 인덱스를 `indexes`에 싣고, SQLite 인라인 제약 한계를 내지 않는다).
- isthmus에 relation-decl용 공유 적합성 벡터가 없다. 생기면 벤더링하고 `conformance.lock`으로 기록한다.
- Docker DB fixture(MySQL·MariaDB·SQL Server·Oracle·IBM)는 컨테이너 데몬이 있는 환경이나 CI에서만 돈다.
- SQL Server의 대소문자 무시 판정은 ASCII 접기다. 비ASCII 이름의 collation 규칙은 따르지 않고 정확 비교로 남는다.

## 이력

아래는 당시 기록이다. 날짜·버전·"미배포" 표기는 그 시점의 상태다.

## 0.7.1 릴리스 검증 순서 보강 (2026-10-12 UTC)

- DML accuracy·IBM fixtures·SQL Server/Oracle accuracy의 push를 모든 브랜치로
  한정한다. 기존 paths·PR·수동 실행·검증 본문·권한은 유지하며, 릴리스 태그는
  `Release binaries`만 시작한다. 별도 DB 컨테이너 검증을 태그 게시와 함께
  부수적으로 시작하지 않도록 실행 시점을 명시한 변경이다.
- 태그 전에는 선택한 **최종 병합 소스 커밋**의 `CI`와 위 세 fixture workflow가
  모두 성공했는지 run URL·`head_sha`·conclusion으로 기록한다. PR의 합성 merge
  커밋이나 부모 커밋 결과를 최종 병합 커밋의 실행으로 대신 표기하지 않는다.
  해당 SHA의 검증이 없으면 적용되는 브랜치/수동 검증을 확보할 때까지 태그를
  보류한다. 소스를 다시 바꾸면 새 커밋을 기준으로 같은 확인을 반복한다.
- `Release binaries`의 산출물·플랫폼 검증과 체크섬 확인은 별도 릴리스 관문으로
  유지한다. 태그/바이너리 성공을 crate·Maven Central·Pages 게시 성공으로
  간주하지 않는다. 각 게시와 설치 소비자 확인은 기존 분리된 절차를 따른다.
- 0.7.1에는 파일 출력 보호뿐 아니라 native·Go·JDBC의 SQLite PK nullability
  보정도 포함된다. RELEASE-NOTES의 수집 동작 불변 문구를 바로잡았다.
  이 날짜는 준비 기록이며 실제 공개 날짜나 게시 완료를 뜻하지 않는다.

## 0.7.1 릴리스 준비 (2026-10-11, release/0.7.1)

- 입력·스냅샷 보호 및 컨테이너 host UID 보정 이후의 patch 버전이다.
- Rust workspace·내부 의존·Cargo.lock의 자기 패키지 6개와 JDBC probe,
  Maven 도구/수동 게시 기본값, 설치 예제를 0.7.1로 맞췄다. 외부 의존은 갱신하지 않았다.
- 원본 fix 브랜치를 보존한 별도 worktree에서 준비했으며, 태그·crate·바이너리·
  Maven 게시는 하지 않았다. PR35 CI/머지 뒤 릴리스 PR과 태그를 진행한다.
- release workflow의 버전 guard를 PR/nonpublishing 모드로 실행했고 Cargo
  metadata·locked/offline CLI build·fmt·CLI 버전/skill·scan/query·facts/traversal
  버전 표기, 컨테이너 명령 회귀 4개를 확인했다. 6개 crate를 offline으로
  package(`--no-verify`)한 뒤 정규화 manifest·라이선스·번들 skill을 대조했다.
  외부 lock 항목은 모두 동일하다. 기존 `yoke-derive 0.8.3` yanked 경고는
  유지되며 Gradle 부재로 JDBC JAR·Maven artifact 빌드는 CI 확인 대상이다.
- 원본 수정의 원격 `2302809e21ce1613201c6b70777c0c82e03c4e6b`
  [CI 실행](https://github.com/ictechgy/schemagraph/actions/runs/38157944067)에서
  container consumer smoke는 통과했다. DB/package 본 작업은 확인 당시 진행 중이며,
  이 원본 수정 검증과 별도로 0.7.1 릴리스 산출물 CI는 아직 실행·확인 전이다.

_최종 갱신: 2026-09-25 KST · Claude_

## 파일 출력 보호 보강 (2026-10-11, fix/atomic-snapshot-output)

- `scan`·`merge`·`facts`·`openlineage` 파일 출력을 임시 파일 기록·동기화 후
  원자적 교체로 바꿨다. 기존 파일 권한은 유지하고, 새 파일은 Unix에서 private다.
- catalog·graph·원본 SQLite DB(`file:` URI·인코딩 포함)·수집된 `--sql-dir`
  파일과 출력의 충돌을 쓰기 전에 거절한다. SQL 파일 집합은 source 수집기를
  공유하며, 실제 출력 파일의 canonical 경로와 Unix hard link도 대조한다.
- symlink·특수 파일·읽기 전용 출력과 대소문자만 다른 산출물 이름을 거절한다.
  `scan --emit-document`는 두 파일을 모두 준비한 뒤 공개한다. 두 rename을
  하나의 트랜잭션이나 전원 장애까지 견디는 다중 파일 커밋으로 주장하지 않는다.
- Rust workspace 440개 테스트·fmt·실제 SQLite JSON 계약·경쟁 CLI·취소 검증을
  실행했다. 이전 main으로 catalog 보존 회귀가 실패하는 것을 확인했다.
  `clippy --all-targets`는 통과하나 기존 경고가 있어 `-D warnings`는 실패한다
  (첫 기존 지적: `core/src/merge.rs`의 needless borrow). 수집 의미론·DB 드라이버·
  probe는 바꾸지 않았고 외부 DB 컨테이너 fixture는 이번 환경에서 실행하지 않았다.
- 공개 배포·태그·버전 변경은 이 브랜치에서 하지 않았다. 변경된 CLI 파일 계약은
  ANALYSIS.md의 “Preserve snapshots when updating files”에 있다.
- 컨테이너 smoke의 host bind-mount 경로는 `--user <host uid>:<host gid>`로
  실행한다. 기본 65532 사용자는 컨테이너 안에서 0600 파일 쓰기·읽기를 별도로
  검증하며 private 기본값은 유지한다. Docker가 없는 로컬에서 실행 구성 회귀
  4개와 실제 host CLI 0600 scan/query를 확인했고 컨테이너 end-to-end는 CI에서 확인한다.

## 목표 (v0.6.0 시점)

경쟁 재조사(2026-09-25)에서 정한 보강 네 가지를 구현·리뷰·머지하고 **v0.6.0으로 공개
배포·설치 검증까지 완료**했다. 남은 승인 작업은 없다. 다음 제품 작업은 미정이다.

## 현재 상태 (v0.6.0 시점)

- 프로젝트: schemagraph 저장소 루트. main은 깨끗하며 이 문서 갱신 PR 외 진행 중인 브랜치는 없다.
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

## Blocker와 열린 질문 (v0.6.0 시점)

없음. 추가 게시나 실패 CI 재실행이 남아 있지 않다.

- (2026-11-03 처리) CI의 `verify-bridge-facts.py`가 isthmus-cli 0.14.0으로 `--isthmus --require-isthmus` 검증을 한다.
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
schemagraph 저장소 루트에서 HANDOFF.md와 AGENTS.md를 읽어줘.
발행본은 0.9.0이고, RELEASE-NOTES의 Unreleased 변경은 아직 발행 전이야(다음 crate 릴리스는 0.10.0).
먼저 git 상태를 확인하고 로컬 변경을 보존한 뒤, 내가 추가로 요청하는 작업부터 이어가줘. 배포는 반복하지 마.
```
