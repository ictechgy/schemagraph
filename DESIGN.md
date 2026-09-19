# DESIGN.md — schemagraph 설계 정본

> 이 문서가 설계의 정본입니다. 바뀌면 코드와 함께 고칩니다.

## 한 문장 설계

**그래프가 산출물이고, 나머지는 전부 그 위의 질의다.**

새 기능을 넣을 때 "이것도 그래프 질의로 표현되는가"를 먼저 묻는다.

## 이 도구가 파는 것

기존 범용 DB 툴(SchemaSpy·SchemaCrawler·tbls·Azimutt·DBeaver)은 스키마를 읽어
다이어그램과 문서에서 멈춘다. schemagraph는 스키마를 의존성 그래프로 만들고,
그 위에 **판정 질의**를 얹는다.

- "이 컬럼/테이블을 바꾸거나 지우면 뭐가 깨지는가" — `impact`
- "순환 의존이 있는가" — `cycles` (FK 순환 = 삭제 순서·배치 데드락 분석)
- "아무도 쓰지 않는 객체가 있는가" — `dead` (사용 통계를 증거로 첨부)
- "팀 규칙을 어기는 의존이 있는가" — `rules`

차별점 두 개:

1. **간선의 깊이.** 선언된 FK만이 아니라 view/routine/trigger의 SQL 몸체를 파싱해
   `reads`·`writes`·`calls` 간선을 만든다. 기존 범용 툴이 전부 멈추는 지점이다.
2. **판정 계약.** 출력은 사람용 문서가 아니라 코딩 에이전트가 소비하는 결정적 JSON이다.
   그래프 사실과 판정을 분리한다("도달 불가"라고 말할 뿐 "지워도 된다"고는 말하지 않는다).

## 세 개의 증거 계층

| 계층 | 근거 | 생산물 | 한계 |
|------|------|--------|------|
| 카탈로그 | sqlx 네이티브·JDBC `DatabaseMetaData` + DB별 카탈로그 SQL | 선언된 구조: 테이블·컬럼·PK·FK·인덱스·제약 | 선언되지 않은 의존은 못 봄 |
| 몸체 파싱 | sqlparser-rs(엔진 내장, 다방언) | view·routine·trigger 본문의 `reads`·`writes`·`calls` 간선 | 파싱 실패 본문 = 못 본 의존 → `limitations`에 실측 기록 |
| 사용 통계 | `pg_stat_*`·`sys`·`performance_schema` 등 | `dead` 판정의 증거 | 통계 리셋 이후만 유효. 단독 판정 금지 |

세 계층은 같은 그래프에 다른 `evidence`로 붙는다. 간선은 자기가 어느 계층에서
왔는지 항상 밝힌다.

## 그래프 모델

### 정점

- **container**: `schema`
- **object**: `table`, `view`, `materialized-view`, `sequence`, `type`, `synonym`
- **member**: `column`(1급 정점 — impact 질의의 핵심), `index`, `constraint`, `trigger`
- **executable**: `function`, `procedure`, `package`(Oracle 계열)

정점 id는 `schema`, `schema.object`, `schema.object.member` 형식이다.
이름이 kind를 넘나들며 충돌할 수 있다(MySQL의 FK 자동 인덱스는 컬럼
이름을 그대로 쓰고, 함수와 프로시저는 이름을 공유할 수 있다) — base
id가 다른 kind에 점유됐으면 나중 정점은 `name@kind`로 분리된다
(`orders.sku@index`). `@`는 SQL 식별자에 못 쓰는 문자라 추가 충돌이
거의 없고, 분리는 항상 limitation으로 신고된다.

### 간선 종류

- `references` — 선언된 FK. object 레벨과 column 레벨 둘 다 만든다.
- `reads`, `writes` — 몸체 파싱으로 유도. view→테이블, routine→테이블/컬럼.
- `calls` — routine→routine.
- `fires` — trigger→발화 테이블, trigger→routine.
- `uses-sequence` — 컬럼 기본값(serial/identity)·routine의 `nextval`.
- `uses-type` — 컬럼→UDT/enum.
- `contains` — schema→object→member 소유 관계. **담는 관계는 쓰는 관계가 아니다.**
  `dependsOn`에 섞지 않는다. 그렇다고 빼면 테이블의 `dependsOn`이 비어
  "아무것도 의존하지 않는다"로 읽힌다 — 별도 필드로 준다.
- `inferred` — 선언 FK가 없을 때 이름 규칙으로 추정한 간선(Azimutt식).
  `confidence`를 낮게 매기고, 선언된 간선과 같은 필드에 섞지 않는다.
  추정 간선은 판정이 아니라 탐색 보조다.

## 어댑터 등급 — "JDBC 전부"가 성립하는 방식

엔진이 소비하는 것은 DB가 아니라 **catalog document** 하나다. 그 문서를 만드는
경로가 등급으로 갈린다.

- **Native (내장)**: sqlx로 PG·MySQL·SQLite를 직접 읽는다. 외부 프로세스 없이
  동작하는 기본 경로.
- **Tier 0 Generic 프로브**: `DatabaseMetaData`만으로 table·column·PK·FK·index.
  JDBC 드라이버만 있으면 어떤 DB든 동작한다. 이것이 breadth의 바닥.
- **Tier 1 Rich 프로브**: DB별 카탈로그 SQL(`pg_catalog`, `information_schema`+`sys`,
  `sqlite_master`)로 view·routine·trigger·sequence와 **몸체 원문**까지 읽는다.
  실서버 검증이 끝난 Tier 1 방언: PG·MySQL·SQLite·**MSSQL**(`sys.objects`+
  `sys.sql_modules`)·**Oracle**(`ALL_*`+`ALL_SOURCE`).
- **Tier 2 Stats**: 사용 통계(`pg_stat_*`, `sys`·`performance_schema`) 수집.
  네이티브 경로와 프로브가 같은 document 필드에 싣는다.

첫 Native·Tier 1 대상은 Postgres·MySQL·SQLite. 나머지 DB는 Tier 0 프로브로
자동 동작하고 수요에 따라 승격한다. 어댑터가 없는 DB가 "지원 안 함"이 아니라
"얕게 지원"이 되는 구조다.

## 명령

```
schemagraph scan <jdbc-url> [-o graph.json]   # 카탈로그+몸체 파싱 → 그래프 산출물
schemagraph graph --format mermaid|json|dot [--level schema|object|column]
schemagraph query <객체> [--depth N]           # 누가 쓰나·무엇을 쓰나 (JSON, 에이전트용)
schemagraph impact <객체>                      # 바꾸면/지우면 뭐가 깨지나 — query의 역방향 전이 클로저
schemagraph cycles [--level object|column]     # FK 순환 — 삭제 순서·데드락 분석
schemagraph dead                               # 도달 불가/무사용 후보 — state는 그래프 사실
schemagraph rules                              # 레이어·규칙 검사
schemagraph stats                              # 수집된 사용 통계 열람 (그래프 위의 질의)
schemagraph diff <old.json> <new.json>         # 마이그레이션 전후 델타
schemagraph skill                              # 에이전트 스킬 설치 (계열 전통)
```

## roots 문제 — dead의 정직한 설계

DB에는 `main()`이 없다. 다른 객체가 참조하지 않는 테이블이 죽은 게 아니라
가장 핫한 테이블일 수 있다. 그래서:

- roots는 설정으로 선언한다. 앱이 만지는 진입 표면을 glob으로(`retain: ["app_*", ...]`).
- 사용 통계 스냅샷을 증거로 첨부한다(후보의 `usage: {since, reads, writes}` —
  통계는 `since` 이후만 유효하고, 없는 usage는 "미수집"이지 0이 아니다).
- 출력은 `state: unreachable` 같은 **그래프 사실 + evidence 목록**이다.
  "지워도 됨" 판정은 영원히 없다. 확신이 없으면 살리는 쪽을 고르고 이유를 남긴다.

## 에이전트 출력 계약

- **결정적 JSON.** 키 정렬·정렬된 배열. 같은 입력이 같은 파일이어야 diff와 캐시가 성립한다.
- **`limitations`는 모든 응답에 실측으로 싣는다.** 파싱 실패 routine 수, 통계 부재,
  드라이버가 노출하지 않은 카탈로그 종류 — 그 DB에서 실제로 세어서 만든다.
  `notFound`에도 싣는다. 없는 것과 못 보는 것을 소비자가 구분해야 한다.
- **잘렸으면 잘렸다고**(`truncated`), **몇 걸음인지**(`depth`), **어느 레벨인지**(`level`) 쓴다.
- **이웃에 닿는 간선은 전부 준다**(`edges: ["reads","references"]`). 하나만 고르면
  나머지 관계가 사라진다.
- **값이 없는 선택 필드는 키가 빠진다.**
- **억제된 판정은 그렇다고 표시한다** — 실제로 보고되었을 정점에만.

## 모듈 레이아웃

```
engine/                # Rust workspace — CLI와 모든 판정
├── core               # 그래프 도메인 — 외부 의존성 0
├── parser             # sqlparser-rs 기반 몸체 → reads/writes/calls 간선
├── analysis           # 질의 엔진 (impact·cycles·dead·rules)
├── export             # json·mermaid·dot
├── source             # CatalogSource — native | probe, 같은 document를 뱉는다
└── cli                # clap 엔트리
probe/                 # Kotlin — JDBC로 카탈로그+몸체 원문을 읽어 document 출력 (P2~)
```

`core`에 외부 의존성 금지. sqlx와 프로세스 spawn은 `source` 안에서만,
sqlparser-rs는 `parser` 안에서만 쓴다. 엔진은 DB를 직접 만지지 않고
**catalog document만 소비**한다 — 테스트가 파일 fixture로 끝나는 구조.

## 언어 결정: Rust 코어 + 추출 프로브 복합

- **구조**: 그래프·파싱·판정·CLI는 전부 Rust(정적 단일 바이너리). DB 접속만 별도
  프로세스인 **프로브**가 담당해 표준 document(JSON, 버전 필드)를 뱉고,
  엔진은 그것만 소비한다. CodeQL의 extractor 패턴과 같은 모양이고,
  isthmus의 GRAPH-EXCHANGE 계약과도 같은 격이다. 경계는 FFI가 아니라
  프로세스 + document다.
- **왜 이기는가**: 차별점인 몸체 파싱은 sqlparser-rs가 최강(20+ 방언),
  배포는 네이티브 단일 바이너리, 드라이버 커버리지는 JVM 프로브가 JDBC로
  "전부"를 받친다. 각 계층이 자기 최강 생태계에 있는 복합.
- **비용**: 툴체인이 둘(Rust + JVM), document 계약의 버전 관리, 이국적 DB
  사용자에게 jar 사이드카. 첫 둘은 계약을 fixture 테스트로 고정해 상쇄한다
  (계열의 verify-fixtures 전통 그대로).
- **스테이지 접근**: P0~P1은 Rust + sqlx(PG·MySQL·SQLite 네이티브)만 —
  프로세스 하나로 그래프 모델을 검증한다. P2에서 document 스키마를 고정하고
  Kotlin JVM 프로브를 붙여 "JDBC 전부"가 성립한다. 복합 비용은 수요가
  증명된 뒤에만 지불한다.
- **파싱은 단일 소스**: 몸체 파싱은 Rust 엔진에만 둔다. 프로브는 원문 텍스트를
  옮기는 멍청한 추출기다. 파싱 실패는 `limitations` 실측으로 신고한다.
  JSqlParser 폴백은 필요해지면 프로브가 "파싱된 참조"를 증거로 싣는 형태로
  검토하되, 그래프 의미론의 권위는 항상 엔진 한 곳에 둔다.
- **검토했다가 뺀 것**:
  - Kotlin 단일(JDBC + JSqlParser): 툴체인 하나로 끝나나 파서 천장이 낮고
    jar 배포 UX가 약함. native-image는 JDBC reflection 메타데이터 관리 비용이 크다.
  - Go 프로브 대안: go-ora로 Oracle까지 네이티브 커버가 가능해 "JVM 없는
    전체 배포"가 필요해지면 JVM 프로브 대신/함께 검토. DB2급에서 끊긴다.
  - Rust 단일 + odbc-api: ODBC도 사실상 전 DB를 커버하나 사용자 측 ODBC
    드라이버 설치가 전제라 배포 마찰이 크다.
  - Python(sqlglot 최강): 배포·타입 계약이 계열 철학과 어긋남.

## 단계

- **P0**: core 그래프 모델 + 네이티브 경로(PG·MySQL·SQLite:
  table·column·PK·FK·index) + `scan`/`graph`/`query`/`cycles` + 결정적 JSON.
- **P1**: view·routine·trigger·sequence 수집 + sqlparser-rs 몸체 파싱 →
  `reads`/`writes`/`calls` 간선 + `impact`.
- **P2**: catalog document 스키마 고정(프로브 프로토콜) + Kotlin JVM 프로브
  (Tier 0 Generic → "JDBC 전부" 성립) + `rules` — **완료**:
  `probe/`가 `DatabaseMetaData` + best-effort 몸체 쿼리로 document를 뱉고,
  `scan --document`가 먹는다. MySQL에서 네이티브와 간선 완전 일치를 확인.
- **P3**: `stats` + `dead` 증거 — **완료**: `Usage{since,reads,writes,
  total_ms,self_ms}`가 그래프 정점의 별도 맵에 실리고(시간 필드는 additive
  확장이라 document 버전 유지), PG(`pg_stat_user_tables`/`_indexes` +
  `pg_stat_database.stats_reset`, 미리셋 시 postmaster 기동 시각 폴백)와
  MySQL(`sys.schema_table_statistics` + `sys.schema_unused_indexes`,
  uptime 역산으로 since) 네이티브·프로브 양쪽이 수확한다. routine은
  `pg_stat_user_functions`의 `calls`→reads, `total_time`/`self_time`→
  total_ms/self_ms로 싣는다(오버로드는 귀속 불가 — 이름 유일할 때만;
  MySQL엔 per-routine 통계 소스가 없어 미수확). `track_functions=none`·
  `performance_schema=OFF`처럼 통계 자체가 꺼진 환경은 0행이 아니라
  미수집으로 limitation 신고. `dead` 후보는 usage를 증거로 싣는다.
  `skill`·mermaid 다듬기까지 완료.
- **P4**: MSSQL·Oracle Tier 1 프로브 — **완료**: `sys.objects`+
  `sys.sql_modules`(MSSQL, INFORMATION_SCHEMA의 4000자 절단 회피),
  `ALL_*`+`ALL_SOURCE`(Oracle)로 몸체까지 수확하고 실서버로 검증했다
  (Azure SQL Edge, gvenzl/oracle-free — 둘 다 arm64 지원). MSSQL의 JDBC
  메타 오분류·`;N` 접미사, Oracle의 PUBLIC 커서 고갈·대소문자 접힘
  해석(ci, oracle 한정) 등이 실검증에서 드러나 고쳤다.
  다음: `diff`, `inferred` 간선, Go 프로브("JVM 없는 배포" 수요가 생기면).

각 단계의 완료 조건은 실제 DB fixture로 양방향 검증하는 스크립트다
(계열의 verify-fixtures 전통 — 도구가 실제로 발견한 결함이 단위 테스트를 통과한 뒤
드러난 전과가 있다).

## 경쟁 대비 위치

| 툴 | 그들의 끝 | 우리가 더 가는 곳 |
|----|-----------|-------------------|
| SchemaCrawler | 다이어그램 + lint + grep | 판정 질의·몸체 간선·에이전트 계약 |
| tbls | 문서 + CI lint | 그래프 질의·impact·통계 증거 |
| Azimutt | GUI 탐색 + 추정 간선 | CLI 판정·판정/추정 간선 분리 |
| DataGrip/DBeaver | IDE ERD | headless·CI·에이전트 소비 |

## 미결 사항

- **catalog document 스키마** — version 1로 고정됨(`document.rs`의
  `DOCUMENT_VERSION`). 필드 추가는 하위호환으로, 이름 변경·의미 변경은 버전을 올린다.
- **routine 몸체 파싱 커버리지** — `language=sql`은 파싱됨. plpgsql·pl/sql 등은
  limitation으로 보고 중. 파서 개선 기여 vs 프로브 측 "파싱된 참조" 증거로 결정.
- **프로브 전송 방식** — 현재 파일(`-o`) 또는 stdout(`-o -`). 큰 스키마의
  스트리밍 NDJSON은 수요가 생기면.
- **crates.io / Maven Central 이름** — `schemagraph`는 둘 다 비어 있음
  (2026-09-19 확인). 선점·퍼블리시는 아직이다.
- **`inferred` 간선 휴리스틱 세부** — Azimutt 방식 벤치마크 후 결정.
- **멤버 id 공간 v2** — 모든 멤버 id에 kind를 박는 안은 보류(호환 깨짐).
  현행: 충돌 시에만 `@kind` 접미사.
- **Oracle package routine 귀속** — PACKAGE BODY의 멤버는 OBJECT_NAME이
  패키지라 독립 routine과 귀속이 다르다. 독립 PROCEDURE/FUNCTION만 수확 중.
- **프로브 Tier 1 추가 확장** — DB2, Informix 등의 몸체 소스는 수요별로.
