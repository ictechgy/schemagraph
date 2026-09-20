# DESIGN.md — schemagraph 설계 정본

> 이 문서가 설계의 정본입니다. 바뀌면 코드와 함께 고칩니다.

## 한 문장 설계

**그래프가 산출물이고, 나머지는 전부 그 위의 질의다.**

새 기능을 넣을 때 "이것도 그래프 질의로 표현되는가"를 먼저 묻는다.

## 이 도구가 파는 것

schemagraph는 여러 DB의 카탈로그와 SQL 의존성을 재현 가능한 그래프 파일로 만들고,
변경 영향과 분석 한계를 로컬에서 질의하는 개발자·에이전트용 도구다.
SchemaCrawler·tbls·Azimutt 등에도 의존성·lint·탐색 기능이 있다. 기능의 독점성을
주장하지 않고 해석 범위·근거·실DB 검증으로 분석 품질을 설명한다.
비교 근거와 후속 개발 제안은 [COMPETITIVE-ANALYSIS.md](COMPETITIVE-ANALYSIS.md)에 있다.

- "이 컬럼/테이블을 바꾸거나 지우면 뭐가 깨지는가" — `impact`
- "순환 의존이 있는가" — `cycles` (FK 순환 = 삭제 순서·배치 데드락 분석)
- "아무도 쓰지 않는 객체가 있는가" — `dead` (사용 통계를 증거로 첨부)
- "팀 규칙을 어기는 의존이 있는가" — `rules`

집중하는 특성 두 개:

1. **간선의 깊이.** 선언된 FK만이 아니라 view/routine/trigger의 SQL 몸체를 파싱해
   `reads`·`writes`·`calls` 간선을 만든다. 방언별 실제 수집·해석 범위를 명시한다.
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
- **query**: `--sql-dir`로 명시한 애플리케이션 SQL 파일. DB routine이라고
  주장하지 않고 보존 루트로 취급한다.

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

그래프 v2의 `analysis`는 객체별 파싱 범위·`complete`/`partial`/`unsupported`
상태·진단·몸체 해시를 보존한다. 외부 SQL query 정점은 분석 `source`에
상대 파일 경로를 싣고, 절대 경로나 원문 SQL은 그래프에 복사하지 않는다.
`origins`는 실제 간선의 `(from,to,kind)`에만 연결된 몸체 해시·role·원문 위치다.
출력 소비자는 정점·간선·origin endpoint가 실제 그래프에 있는지 확인해야 하며,
없는 정점을 복구하거나 간선을 추론해서는 안 된다.

카탈로그 reader가 제공한 컬럼·인덱스·FK 사실은 core의 `SchemaMetadata`로
보존하고 graph v2의 선택적 `schema_metadata` maps로 내보낸다. maps의 키와
값은 실제 graph 정점 ID를 가리키며, export codec은 kind·parent·table 관계를
검증해 유령 metadata를 거부한다. 인덱스의 ordered prefix, predicate 유무,
완전성, FK 컬럼 대응은 `lint`가 이 메타데이터로 검사한다. metadata가 없거나
불완전하면 `confirmed` 판정을 내리지 않고 `unverified`와 limitation을 보고한다.

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
schemagraph scan <jdbc-url> --sql-dir queries --query-schema app  # 외부 SQL query root 추가
schemagraph graph --format mermaid|json|dot|html [--level schema|object|column]
schemagraph query <객체> [--depth N]           # 누가 쓰나·무엇을 쓰나 (JSON, 에이전트용)
schemagraph impact <객체>                      # 바꾸면/지우면 뭐가 깨지나 — query의 역방향 전이 클로저
schemagraph cycles [--level object|column]     # FK 순환 — 삭제 순서·데드락 분석
schemagraph dead                               # 도달 불가/무사용 후보 — state는 그래프 사실
schemagraph rules                              # 레이어·규칙 검사
schemagraph stats                              # 수집된 사용 통계 열람 (그래프 위의 질의)
schemagraph diff <old.json> <new.json>         # 마이그레이션 전후 델타
schemagraph merge <catalog-a.json> <catalog-b.json>  # source namespace를 보존해 병합
schemagraph lint                                # FK ordered-prefix와 구조화 진단 검사
schemagraph serve --graph graph.json            # 고정 그래프를 읽기 전용 MCP stdio로 제공
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
- **분석 상태의 의미를 좁게 읽는다.** `complete`는 기록된 분석 범위가
  완료됐다는 뜻이지 실행 시점 SQL 전체가 안전하다는 뜻이 아니다. `partial`이나
  `unsupported`에서는 빈 간선 목록을 부재의 증명으로 읽지 않는다.
- **`truncated`는 결과 제한과 탐색 중단을 구분한다.** 결과 개수만 잘린 경우에도
  `truncated`를 싣고, depth·visited·examined edge 예산으로 탐색을 끝낸 경우에는
  `truncationReasons`와 `complete=false`를 함께 보고한다.
- **HTML은 오프라인 산출물이다.** `graph --format html`은 외부 asset·network를
  사용하지 않고, 첫 화면에는 전체 그래프를 그리지 않는다. 선택된 정점의 직접
  이웃만 제한해 표시하며 이름·근거·origin은 DOM `textContent`로 출력한다.

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
- **P5**: 판정 도구 + 프로브 확장 — **완료**:
  - `diff`: document↔document·graph↔graph 구조 델타(추가/제거 정점·간선,
    몸체 변경), `--strict`는 델타 시 1.
  - `plpgsql`/`plsql` 문장 추출: 절차형 스캐폴딩(BEGIN/IF/FOR/LOOP/DECLARE)을
    걷어내고 보이는 SQL만 문장 단위로 파싱 — 한 문장의 실패가 전체를 죽이지
    않고, 못 건진 구문(동적 SQL 식 등)은 limitation으로 센다.
    `SELECT … INTO`/`RETURNING … INTO`의 변수는 참조로 오인하지 않는다.
  - `inferred` 간선(opt-in `--inferred`): 미선언 `*_id` 컬럼의 이름 규칙으로
    같은 스키마 `id` 보유 테이블을 추정 — 선언 FK 우선, 모호 후보는 limitation.
    `EvidenceLayer::inferred`라 의존성 질의(`is_dependency()`가 거짓)에 섞이지 않는다.
  - Oracle PACKAGE 수확: PACKAGE+PACKAGE BODY를 `package` 정점 하나로 병합
    (BODY 우선), 멤버별 귀속 미지원은 limitation으로 신고.
  - NDJSON 전송: 프로브 `--format ndjson` + 엔진 `scan --document` 자동 감지.
    레이아웃은 `document` 헤더 → `schema` 행 → `object`/`routine` 행.
  - Go 프로브(`probe-go/`): go-ora로 Oracle만 커버하는 단일 정적 바이너리 —
    JVM도 외부 ojdbc도 없이 JVM 프로브와 그래프 완전 패리티(실검증).

각 단계의 완료 조건은 실제 DB fixture로 양방향 검증하는 스크립트다
(계열의 verify-fixtures 전통 — 도구가 실제로 발견한 결함이 단위 테스트를 통과한 뒤
드러난 전과가 있다).

- **P6 로컬 통합 경로**: `--sql-dir`는 수집된 단일 schema에 상대 SQL 파일을
  `query_<hex-relative-path>` routine으로 추가한다. 파일 원문은 parser 입력으로만
  쓰고, query는 애플리케이션 사용 root라는 사실만 보존한다. `--cache-dir`는
  실행 파일 fingerprint·graph version·몸체를 제외한 전체 카탈로그 구조·context·
  dependencies를 namespace로 삼고, 몸체별 parser 효과만 checksum된 private entry로
  저장한다. cache 오류는 분석 실패가 아니라 stderr 경고와 miss다.
  `merge`는 각 document의 명시적 `context.source_id`를 `source::id` namespace로
  보존하고, 외부 dependency 대상이 하나로 결정될 때만 `depends-on`을 만든다.
  여러 후보·미수집 대상은 간선을 만들지 않고 limitation으로 남긴다.

## 경쟁 대비 위치

| 툴 | 그들의 끝 | 우리가 더 가는 곳 |
|----|-----------|-------------------|
| SchemaCrawler | 다이어그램 + lint + grep | 판정 질의·몸체 간선·에이전트 계약 |
| tbls | 문서 + CI lint | 그래프 질의·impact·통계 증거 |
| Azimutt | GUI 탐색 + 추정 간선 | CLI 판정·판정/추정 간선 분리 |
| DataGrip/DBeaver | IDE ERD | headless·CI·에이전트 소비 |

## 호환 계약과 확장 경계

- **catalog document v1/v2** — 기본 출력과 내부 모델은 v1을 유지한다.
  v2는 `reader`를 `producer.name`으로 바꾸고 `required_features`를 요구한다.
  현재 기능은 `usage-v1`·`package-members-v1`이며, 모르는 필수 기능은 거부한다.
  지원 버전은 `document-capabilities`로 조회한다. 같은 버전의 선택 필드는
  추가할 수 있지만 무시된 경로를 `limitations`에 신고한다. 필드의 삭제·의미
  변경은 새 전송 버전 또는 소비자가 명시적으로 이해하는 기능 계약이 필요하다.
  두 버전은 같은 내부 catalog로 정규화되므로 graph 버전과 정점 id는 바뀌지
  않는다. 상세 전송 명세와 이행 예시는 [CATALOG.md](CATALOG.md)에 있다.
- **SQL 텍스트 평가** — 리터럴, 방언별 상수 연결, PostgreSQL의 명확한 builtin
  `format()` 일부 형식, 직선 구간의 안전한 텍스트 변수만 평가한다. 분기·루프·
  미모델링 대입·형 변환·외부 효과가 개입하면 값을 무효화하고 한계를 남긴다.
  출력 크기·재귀 깊이·인자 수를 제한하며, 변수에 의존하는 문자열 접두부를
  완성된 SQL로 판정하지 않는다. 완전한 절차형 인터프리터나 임의 함수 실행은
  분석기의 역할이 아니다. 문법 기준은
  [PostgreSQL EXECUTE](https://www.postgresql.org/docs/16/plpgsql-statements.html#PLPGSQL-STATEMENTS-EXECUTING-DYN),
  [Oracle EXECUTE IMMEDIATE](https://docs.oracle.com/en/database/oracle/oracle-database/19/lnpls/EXECUTE-IMMEDIATE-statement.html),
  [SQL Server EXECUTE](https://learn.microsoft.com/en-us/sql/t-sql/language-elements/execute-transact-sql?view=sql-server-ver16)다.
- **프로브 전송** — JSON·NDJSON을 지원한다. JDBC·Go는 스키마 단위로 NDJSON을
  방출한다. Rust는 레코드별로 읽어 전송 Value를 바로 해제하고, 그래프 JSON은
  도메인을 빌려 출력한다. 교차 참조를 위한 내부 카탈로그와 그래프는 여전히
  전체 메모리에 보관한다. Go의 JSON 경로도 전체 문서를 유지한다.
  v2 NDJSON은 마지막 limitations 레코드를 요구해 중간에 끊긴 전송을 거부한다.
  v1의 트레일러 없는 옛 형식은 계속 허용한다.
- **Oracle 패키지 멤버** — 카탈로그의 `member_of`로 정점을 나누고 raw body를
  멤버에 붙인다. 문자열·주석의 가짜 헤더, 로컬 서브프로그램, 패키지 초기화
  블록을 멤버 경계로 오인하지 않으며, 경계를 확정하지 못하면 원문 귀속을
  추측하지 않고 limitation으로 신고한다. 그래프 간선 해석은 엔진에만 있다.
- **Go 프로브** — SQLite·PostgreSQL·MySQL/MariaDB·Oracle·SQL Server를 지원한다.
  CGO 없이 빌드하며 카탈로그와 원문만 수집한다. 통계 비활성화와 미수집은
  실제 관측으로 보고하고, 관측된 0과 구분한다.
- **Db2 LUW·Informix** — JDBC로 방언별 catalog와 SQL/SPL 원문·routine
  시그니처를 수집한다. 불투명 타입의 표현 변환은 DB의 메타데이터 cast에 맡기고,
  트리거·routine의 SQL 해석은 Rust에서 수행한다. 실제 이미지·드라이버를 고정한
  전용 fixture가 전송 네 조합과 필요한 간선·생기면 안 되는 간선을 확인한다.
- **Maven 배포** — Maven Central과 GitHub Pages에 같은 JAR·POM 바이트를 게시한다.
  Central 소비자는 별도 저장소 URL을 추가할 필요가 없다. 게시자는 계정·namespace
  인증과 PGP 서명을 준비한다. 과거 버전은 보존하고 게시된 파일의 다른 바이트로
  덮어쓰기를 거부한다. Central 업로드 전에 Pages와 파일 내용이 일치하는지
  비교한다. 절차는 [MAVEN.md](MAVEN.md)에 있다.
- **명시적으로 보류한 범위** — 멤버 id에 항상 kind를 붙이는 안은 호환성
  때문에 보류한다. 현재는 충돌할 때만 `@kind`를 쓴다. 전체 파이프라인의
  고정 메모리 상한은 아직 보장하지 않는다. 새 방언과 더 큰 스케일의 분석은
  실제 사용 사례와 [PERFORMANCE.md](PERFORMANCE.md)의 측정을 기준으로 확장한다.
