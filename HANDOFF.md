# HANDOFF.md

_최종 갱신: 2026-09-25 KST · Claude_

## 목표

경쟁 재조사(2026-09-25)에서 정한 보강 네 가지(#24~#27)를 구현·리뷰·머지하고
**v0.6.0으로 공개 배포·설치 검증까지 완료**했다. 남은 승인 작업은 없다.

## 현재 상태

- 프로젝트: `/Users/jinhongan/Desktop/schemagraph`.
- 공개 버전: [v0.6.0](https://github.com/ictechgy/schemagraph/releases/tag/v0.6.0) (2026-09-25).
  태그·배포 소스·6개 crate·Go 바이너리의 내장 커밋은 `e5f3b9cdfe453b6c6ee872e1283fd6a18af592b2`
  ([PR #29](https://github.com/ictechgy/schemagraph/pull/29) 머지 커밋). crates.io 6개, GitHub Release
  ([빌드](https://github.com/ictechgy/schemagraph/actions/runs/36138711491)), Pages
  ([실행](https://github.com/ictechgy/schemagraph/actions/runs/36139205607)), Maven Central
  ([실행](https://github.com/ictechgy/schemagraph/actions/runs/36139607863), deployment
  `be128a33-79b8-446f-9cd5-ecd8459411c4` **PUBLISHED**). 근거는 `verification/v0.6.0-20260925/`
  (`plan.json` complete, `public-crates.json`, `installed-features/result.json` 15개 검사,
  `public-verification.json`, `published-crates/`). 공개 CLI·Go·JAR로 lint·unused도 세 수집기 검증했다.
- 이전 공개 버전: [v0.5.1](https://github.com/ictechgy/schemagraph/releases/tag/v0.5.1) (2026-09-25).
  태그·배포 소스·6개 crate·Go 바이너리의 내장 커밋은 `fce98ec61dacabb99fa9e626cc17760771eb0554`
  ([PR #22](https://github.com/ictechgy/schemagraph/pull/22) 머지 커밋). 이전 v0.5.0(`94d1db8`)은 보존했다.
- v0.5.0 이후 v0.5.1에 포함된 변경:
  - [PR #17](https://github.com/ictechgy/schemagraph/pull/17): README 마스코트 아이콘(`icon.png`).
  - [PR #18](https://github.com/ictechgy/schemagraph/pull/18): `facts --document` 명령.
    카탈로그의 table/view/materialized-view와 컬럼을 isthmus bridge-facts v1
    `relation-decl`로 내보낸다(`engine/source/src/bridge_facts.rs`). 계약 정본은
    `../isthmus/docs/GRAPH-EXCHANGE.md`다.
  - [PR #19](https://github.com/ictechgy/schemagraph/pull/19): 이 HANDOFF 정리.
  - [PR #20](https://github.com/ictechgy/schemagraph/pull/20): 아래 "facts 검증과 MV 수정" 참고.
- isthmus 쪽 명령 표기 수정([isthmus#113](https://github.com/ictechgy/isthmus/pull/113),
  `facts --graph` → `facts --document`)도 머지됐다.
- v0.5.1 이후 v0.6.0에 포함된 변경(각 PR에 리뷰 처리 내역 코멘트가 있다):
  - [#24](https://github.com/ictechgy/schemagraph/pull/24): `search`(점진 공개)와 MCP `search`·`dead`·`cycles`·
    `lint`·`stats`·스냅샷 resources. `serve`는 명시한 `--config/--retain/--as-of`만 읽는다.
  - [#25](https://github.com/ictechgy/schemagraph/pull/25): lint `fk-type-mismatch`(blocking)·
    `table-without-primary-key`·`duplicate-index`(둘 다 advisory, `blockingCount`로 strict 판정),
    PG native·Go의 `pk_position` 미수집 결함 수정과 엔진 보정, `schema_metadata.catalog_complete`.
  - [#26](https://github.com/ictechgy/schemagraph/pull/26): `unused`, `usage.scans`(테이블 스캔 수 — 빈 채로
    폴링되는 테이블 판별), 파티션 부모 usage 제외, `track_counts=off`면 테이블·인덱스 usage 미수집.
  - [#27](https://github.com/ictechgy/schemagraph/pull/27): `openlineage`(RunEvent 2-0-2). INDIRECT는 뷰의
    JOIN·FILTER만, 부분 분석은 `schemagraphAnalysis` job facet, 공식 스키마는 `schemas/openlineage`에 고정.
- 새 CI 단계: `verify-schema-lint.py`, `verify-unused.py`, `verify-openlineage.py`(모두 실제 PG, 앞의 둘은
  native·Go·JDBC). 수집기 변경으로 JDBC JAR·Go 프로브도 달라졌다.
- 로컬 Colima는 이번 fixture 검증을 위해 켰다가 작업 종료 시 다시 껐다(원래 꺼져 있었다).

## 완료한 일

- **v0.5.1 배포** ([PR #22](https://github.com/ictechgy/schemagraph/pull/22) → 태그 `v0.5.1`):
  [crates.io](https://crates.io/crates/schemagraph-cli/0.5.1) 6개 crate, GitHub Linux x86_64/macOS arm64
  CLI·Go·JDBC JAR([릴리스 빌드](https://github.com/ictechgy/schemagraph/actions/runs/36078769834)),
  [Pages Maven](https://github.com/ictechgy/schemagraph/actions/runs/36079124114),
  [Maven Central](https://central.sonatype.com/artifact/io.github.ictechgy/schemagraph-probe/0.5.1)
  ([실행](https://github.com/ictechgy/schemagraph/actions/runs/36079380750), deployment
  `3599605e-7a80-4660-a464-e9d750831def` **PUBLISHED**). 릴리스 본문은 RELEASE-NOTES 0.5.1 항목이다.

- [PR #14](https://github.com/ictechgy/schemagraph/pull/14): 정책·기준선·만료 예외·SARIF/Action,
  정적 DML 컬럼 계보·임시 심벌, dbt/query-log import, 제한 권한 PG 수집,
  Docker·지원표·추가 성능 측정, 의존 구조별 캐시 재사용을 구현하고 검증했다.
  최종 리뷰에서 UNION SELECT INTO 누락도 수정했다.
- [PR #15](https://github.com/ictechgy/schemagraph/pull/15): 버전·설치 문서를 0.5.0으로 맞추고
  CI 통과 후 배포했다. [PR #16](https://github.com/ictechgy/schemagraph/pull/16)은 완료 기록이다.
- [crates.io](https://crates.io/crates/schemagraph-cli/0.5.0)의 6개 crate,
  GitHub의 Linux x86_64/macOS arm64 CLI·Go 및 JDBC JAR,
  [Pages Maven](https://ictechgy.github.io/schemagraph/maven/),
  [Maven Central](https://central.sonatype.com/artifact/io.github.ictechgy/schemagraph-probe/0.5.0)을 게시했다.
  Central deployment `8d7ddc68-e0ac-4d8c-a06c-bdf65bbb51b0`는 **PUBLISHED**다.

## facts 검증과 MV 수정 (#20, 2026-09-25 · v0.5.1로 배포)

- **결함 (v0.5.0 공개본에도 있음):** native·Go 수집기가 PG materialized view 컬럼을 비워 두었다.
  공유 가시성 검사(`sql/visibility-postgres.sql`)는 MV 컬럼을 세므로, MV가 있는 DB는 소유자 scan에서도
  `uncollected columns` limitation과 `catalog_complete: false`가 되었다. 그 결과 `review`는 `unverified`,
  `facts`는 `catalog-coverage`로 isthmus 미선언 error가 경고로 강등됐다. JDBC는 원래 MV 컬럼을 읽었다.
- **수정:** 두 수집기가 공유하는 `sql/columns-postgres.sql`(engine/source·probe-go 사본, CI `cmp`).
  MV 컬럼은 `pg_attribute`에서 `information_schema`와 같은 권한 조건·data_type·is_nullable 표기로 읽는다.
- `facts`의 `generatedAt`은 시계가 1970년 이전·9999년 이후면 조용히 값을 만들지 않고 실패한다.
- `Scripts/verify-bridge-facts.py`(CI 단계 있음): 기대값을 DB 시스템 카탈로그에서 직접 만들고
  같은 scan 그래프 정점 id와 양방향 대조한다. `--isthmus <dist/cli/main.js>`를 주면 실제 isthmus check로
  소비까지 확인한다. isthmus persistence 조인은 npm 0.9.0에 없어 **CI는 `status: partial`로 이 단계를 건너뛴다.**
  로컬 소비 검증은 isthmus main `b6a0eec`를 `git archive`로 scratchpad에 풀고 `node_modules`를 symlink해
  `node scripts/build.mjs`로 빌드해 수행했다(사용자 isthmus 작업 트리는 건드리지 않음).
- 알려진 비대칭(범위 밖): MV 이름은 `pg_matviews`라 권한과 무관하게 보이고, MV 컬럼은 권한 규칙을 따른다.
  `SUPPORT-MATRIX.md`에 명시했다.
- 리뷰에서 보류한 2건: 컬럼 SQL 문자열의 객체별 재생성(측정 근거 없음), Go 번들 쿼리 로딩 중복(기존 패턴).

## 주요 파일과 근거 위치

| 경로 | 먼저 볼 내용 |
| --- | --- |
| [AGENTS.md](AGENTS.md), [DESIGN.md](DESIGN.md) | 작업 규칙·설계·에이전트 출력/호환 계약 |
| [RELEASE-NOTES.md](RELEASE-NOTES.md), [INSTALLATION.md](INSTALLATION.md), [MAVEN.md](MAVEN.md) | v0.5.0 변경·설치·게시 절차 |
| [ANALYSIS.md](ANALYSIS.md), [REVIEW-ACTION.md](REVIEW-ACTION.md), [EXTERNAL-SQL.md](EXTERNAL-SQL.md) | 분석·정책·Action·import의 현재 계약 |
| [ACCURACY.md](ACCURACY.md), [DML-ACCURACY.md](DML-ACCURACY.md) | 독립 기대값·첫 실패·수정 후 결과·표본 한계 |
| [SUPPORT-MATRIX.md](SUPPORT-MATRIX.md), [PERFORMANCE.md](PERFORMANCE.md), [CATALOG.md](CATALOG.md) | 검증된 지원 범위·측정 경계·document 계약 |
| `engine/{core,source,parser,analysis,export,cli}/`, `probe/`, `probe-go/` | Rust 엔진과 JVM/Go 추출기 |
| `.github/workflows/`, `Scripts/verify-*.py`, `Scripts/verify-fixtures.sh` | 실행 가능한 CI·회귀·실DB 검증 |

로컬 근거 루트는 `~/Library/Application Support/schemagraph/verification/`이다.

- `v0.5.1-20260925/`: `plan.json`(complete), `publish.log`, `public-crates.json`,
  `installed-features/result.json`(registry 설치본 12개 검사), `public-verification.json`,
  `registry-bridge-facts-isthmus.json`, 검증 스크립트 `verify-{crates,installed,public}.py`.
  `registry-install/bin/schemagraph`는 crates.io에서 별도 설치한 0.5.1이다.
- `v0.5.0-20260922/`: `plan.json`(complete), `release-source-ci.json`, `public-crates.json`,
  `installed-features/result.json`, `public-verification.json`, `central-publication.json`.
  `registry-install/bin/schemagraph`는 crates.io에서 별도로 설치해 검증한 0.5.0이다.
- `competitive-followups-20260922/`: 경쟁 보강의 첫 실패·실DB 입력·독립 기대값·최종 리뷰 근거.
  `premerge-review.md`, `premerge-verified-checks.json`, `plan.json`부터 확인한다.
- `v0.4.3-20260922/registry-install/bin/schemagraph`는 DML 비교 기준이므로 보존한다.
- 과거 전체 이력은 [정리 전 HANDOFF](https://github.com/ictechgy/schemagraph/blob/739ac6170286d1f768062406ef5c699661f30630/HANDOFF.md)에 남아 있다.
  오래된 “미완료·미배포·커밋 예정” 문구는 당시 상태이며 현재 할 일이 아니다.

## 중요한 결정과 경계

- **확인된 사실:** 그래프 의미론은 Rust 엔진에만 둔다. core 외부 의존성 금지,
  결정적 JSON, reader가 부여한 ID만 사용, `dead`는 삭제 허가가 아니라 도달성 사실이다.
  프로브 필드 변경 시 수집·merge·NDJSON 경로를 함께 검증한다.
- DML 기대값은 구현 전에 `776ebe8`에서 고정했다.
  corpus SHA-256: `fd129759650617b06c81ab826fb11627f5315dc72ebc1470dbdaabb10bd41d4b`.
  `.github/workflows/dml-accuracy.yml`의 **0.4.3 다운로드·체크섬은 의도된 baseline**이다.
- 0.5.0의 public Rust parser/cache 타입 변경 때문에 minor 버전을 올렸다.
  catalog v1/v2·graph v2 계약은 유지하며 오래된 캐시는 자동 무효화한다.
  동적 SQL·분기·불확실한 문맥은 추측하지 않고 partial/limitation으로 남긴다.
- NDJSON이어도 Rust 내부 카탈로그·그래프는 전체 메모리를 사용한다.
  캐시는 SQL 분석 재사용이며 영속 증분 그래프 엔진은 아니다. 측정값은 SLA가 아니다.
  모든 멤버 ID에 kind를 붙이는 변경은 호환성 때문에 보류한다.
- Pages/Central은 **기존 릴리스 태그에서 수동 게시**한다. main 머지는 게시 트리거가 아니다.
  기존 버전·태그·JAR·POM을 덮어쓰지 않는다. 기존 인증·서명 설정을 재사용하며 비밀 파일은 읽지 않는다.
- **미정:** 다음 구현 범위. 추가 warehouse·플랫폼 지원 등을 승인된 잔여 과제로 간주하지 않는다.

## 검증

이번 정리는 문서만 변경했다. 아래는 이미 완료한 릴리스 검증이며 실행 소스가 같으면 재사용한다.
현재 문서는 `git diff --check`, 로컬 참조·근거 파일·고정 corpus 해시·비밀값 패턴 검사와
HANDOFF만 바뀌었는지 확인하는 검사를 통과했다. 런타임 검사는 이번 정리에서 반복하지 않았다.

- 릴리스 커밋의 [CI](https://github.com/ictechgy/schemagraph/actions/runs/35720225067),
  [DML](https://github.com/ictechgy/schemagraph/actions/runs/35720225200),
  [SQL Server/Oracle](https://github.com/ictechgy/schemagraph/actions/runs/35720225125),
  [IBM](https://github.com/ictechgy/schemagraph/actions/runs/35720225150),
  [릴리스 빌드](https://github.com/ictechgy/schemagraph/actions/runs/35720281202): 모두 성공.
- `cargo publish --manifest-path engine/Cargo.toml --workspace --dry-run --locked`:
  별도 target 디렉터리에서 6개 패키지 빌드 통과. 실제 게시 후 공개 체크섬·로컬 바이트·내장 소스 커밋 일치.
- 공개 설치본으로 `verify-contracts.py`, `verify-competitive.py`, `verify-review-action.py`,
  `verify-external-imports.py`, `verify-dml-replay.py`/`verify-dml-accuracy.py` 통과.
  실제 dbt SQL 실행과 4-DB 보존 입력을 검사했다. SQLite/PG/Oracle의 uncached/cold/warm 전체 그래프도 일치.
- 공개 macOS CLI·Go·JAR의 catalog 계약·Chinook/Pagila 정확도·취소·URL 입력 검사 통과.
  SQL Server/Oracle 기존 18개씩의 보존 입력도 통과했다. **보존 입력 재분석은 새 실DB 수집이 아니다.**
- Linux/macOS 아카이브 체크섬, Maven 5개 payload의 PGP 서명·Pages 바이트,
  GitHub all JAR 일치 확인. 빈 Gradle 캐시의 Central-only Java 17 소비자가 실제 H2 테이블·컬럼·PK 수집.
  검증용 임시 DB·다운로드·공개키 keyring·Java 소비자는 정리했다.

## Blocker와 열린 질문

없음. v0.6.0 배포 완료 상태이며 추가 게시나 실패 CI 재실행이 남아 있지 않다. 다음 제품 작업은 미정이다.
남은 경쟁 공백과 하지 않을 것은 [COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md)의 2026-09-25 절에 있다.
isthmus의 persistence 조인이 npm에 배포되면 CI의 `verify-bridge-facts.py`에 `--isthmus`를 붙일 수 있다.

## 효과가 있었던 방법

- 분석 전에 기대값을 고정하고 첫 실패와 수정 후 결과를 분리 보존했다.
- 게시 성공 뒤 공개 파일·별도 설치본·빈 캐시 소비자로 검증해 소스와 배포물을 연결했다.
- 동일 입력의 성공 근거를 재사용하고, 문서만 바뀐 경우 실DB 검사를 반복하지 않았다.

## 피해야 할 재작업

- Pages 첫 게시의 HTTP 503은 [같은 실행의 두 번째 시도](https://github.com/ictechgy/schemagraph/actions/runs/35720293785)에서 해결됐다. 새 버전 게시로 해결할 일이 아니다.
- `cargo test`만으로 CLI가 갱신된다고 가정하지 않는다. 필요하면 먼저 build하되 검증 중 바이너리를 교체하지 않는다.
  패키징 target을 분리하고, 이번 Cargo 게시 아카이브는 `package/tmp-crate/*.crate`에서 확인했다.
- PR의 2~3초 `fail`은 push 실행이 저장소 동시 실행 설정으로 취소된 것이다.
  `gh run list --commit <sha>`로 같은 SHA의 `pull_request` 실행 성공을 확인한다.
- fixture 종료 코드만으로 전체 성공을 선언하지 않는다. stderr의 건너뜀/주의도 확인한다.
  임시 DB는 작업 소유 label·ID로만 정리하고 다른 프로젝트 컨테이너를 건드리지 않는다.

## 다음 단계

1. `git status --short`, `git branch --show-current`로 위 상태와 로컬 HANDOFF 변경을 확인한다.
2. 다음 요청이 문서 반영이면 diff·문서 참조를 검증해 개발 브랜치에서 처리한다. 현재 배포는 반복하지 않는다.
3. 새 기능 요청이면 해당 정본 문서와 코드부터 읽고 범위를 정한다. 기존 승인에 미완료 구현은 없다.
   수집기(native·Go·JDBC)를 바꾸면 PG fixture golden과 세 수집기 검증(`verify-collection-scope.py` 등)을 함께 돌린다.
4. 다음 릴리스는 v0.5.1 절차를 따른다: 버전·문서 PR → CI → merge commit 머지 → 병합 커밋 main CI →
   lightweight 태그 push(GitHub Release) → `cargo publish --workspace`(별도 target) → Pages → Central →
   `verify-{crates,installed,public}.py`. 최신 근거 스크립트는 `v0.6.0-20260925/`에 있다.

## 재개 프롬프트

```text
/Users/jinhongan/Desktop/schemagraph에서 HANDOFF.md와 AGENTS.md를 읽어줘.
v0.6.0(search·MCP 확장, lint 규칙, unused, openlineage) 배포·공개 설치 검증은 완료됐어.
먼저 git 상태를 확인하고 로컬 변경을 보존한 뒤, 내가 추가로 요청하는 작업부터 이어가줘.
```
