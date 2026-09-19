# HANDOFF.md

> 세션을 이어받을 때 이 파일부터 읽으세요. 진행 상태와 다음 할 일이 여기 있습니다.

## 지금 상태

- 2026-09-19: **P0 완료** (`651fb12`). `engine/` Rust 워크스페이스에 core·source·
  analysis·export·cli 크레이트. SQLite 네이티브 reader가 카탈로그를 읽어
  그래프를 만들고, `scan`/`graph`/`query`/`cycles`가 동작한다.
- 2026-09-19: **P1 완료** (`56869e0`, `f2170bb` + dead 후속). `parser` 크레이트가
  sqlparser-rs로 view 본문을 파싱해 `reads` 간선을 만들고(객체·멤버 둘 다),
  trigger 본문을 파싱해 `writes`·`reads`·`NEW./OLD.` member 간선을 만든다.
  `impact` 명령이 역방향 전이 클로저를, `dead`가 DB 내부 도달성 후보를 보고한다.
- 2026-09-19: **PG 네이티브 reader + routine 파싱 완료** (`2dc4123`).
  `source::read`가 URL 스킴으로 디스패치한다. PG 리더는 read-only 세션으로
  스키마·테이블·컬럼·제약·인덱스·trigger·시퀀스·routine을 수집한다.
  파서는 PG trigger의 `EXECUTE FUNCTION`을 껍질에서 직접 채취해 첫 `calls`
  간선을 만들고, `language=sql` routine 몸체의 `AS $$…$$` 껍질을 벗겨
  reads/writes를 파싱한다. plpgsql 등 미지원 언어는 limitation으로 보고한다.
- 2026-09-19: **MySQL 네이티브 reader 완료** (`c36b6f8`). information_schema로
  스키마·테이블·컬럼·제약·인덱스·trigger·routine을 읽는다 — PG와 달리
  카탈로그가 information_schema 하나로 모인다. URL에 DB가 있으면 그것만,
  없으면 시스템 스키마를 뺀 전부를 읽는다. `mysqlx://`(X Protocol)는
  명시적 미지원 오류다. verify-fixtures.sh가 docker로 임시 MySQL을 띄워
  골든까지 검증한다(없으면 건너뛰고 안내).
- 2026-09-19: **몸체 내부 호출 + rules 명령 완료** (`f60fbc7`, `9985c97`).
  routine/trigger 몸체의 `Expr::Function`·`Statement::Call`이 calls 간선이
  된다(내장 함수 필터로 노이즈 차단). `schemagraph rules --config
  schemagraph.toml`이 `from`/`to` 글롭 규칙을 간선 단위로 검사해 위반을
  보고하고 `--strict`로 CI 게이트가 된다.
- 2026-09-19: **P2 완료 — Kotlin/JDBC 프로브** (`62010f2`, `b147f34`). `probe/`가
  `DatabaseMetaData`(스키마·테이블·컬럼·PK·FK·인덱스·routine) + best-effort
  몸체 쿼리(INFORMATION_SCHEMA, Oracle은 ALL_*)로 버전 달린 catalog
  document를 뱉고, `schemagraph scan --document`가 먹는다. 번들 드라이버는
  허용 라이선스만(pgjdbc·H2·sqlite-jdbc), 나머지는 `--driver`로 주입.
  MySQL 실검증에서 네이티브 reader와 의존성 간선 22개 완전 일치. PG는
  trigger calls까지 패리티. H2 임베디드로 서버 없는 end-to-end 검증이
  verify-fixtures.sh에 들어갔다.
- 2026-09-19: **P3 stats 증거 계층 완료** (커밋 예정). `Usage{since,reads,
  writes}`가 `Graph.usage` 맵에 실리고(정점 식별자는 순수 유지),
  `project()`가 멤버 관측치를 조상으로 합산한다(since는 가장 이른 것).
  document 계약엔 `ObjectDoc.usage`·`IndexDoc.usage` 선택 필드로 —
  additive라 버전은 v1 유지. 네이티브 PG(`pg_stat_user_tables`/`_indexes`,
  since는 `pg_stat_database.stats_reset`, NULL이면 postmaster 기동 시각
  폴백)와 MySQL(`sys.schema_table_statistics` + `sys.schema_unused_indexes`,
  since는 Uptime 역산)이 수확하고, 프로브도 같은 방언이면 수확한다.
  `dead` 후보는 usage를 증거로 싣고, `stats` 명령이 수집된 통계를 보고한다
  (관측 정점만 목록 + totals로 미수집 비율 표시). 골든은 usage 값을
  정규화해 저장 — 시각·카운트는 환경 의존이라 비교 불가.
- 2026-09-19: **P3 잔여 + 견고성 라운드** (`40d932f`..`e6db7d7`). 멤버·
  routine id 충돌을 `@kind` 접미사로 근본 해결하고 파서 해석을 맞췄다.
  `pg_stat_user_functions`로 routine 호출 수를 usage에 실었다(이름이
  유일할 때만 — 오버로드는 funcname으로 귀속 불가). `track_functions=
  none`·`performance_schema=OFF` 같이 통계 자체가 꺼진 환경은 0행이
  아니라 미수집으로 limitation 신고. 프로브의 `ROUTINE_TYPE` NULL NPE
  (집계 함수 행이 수확을 중단) 수정. MariaDB 11.4로 네이티브+프로브
  실검증(ordinal_position 부호 차이 수정). mermaid는 kind 도형·스키마
  subgraph·결정적 n0.. 노드 id로 다듬고, `skill` 명령이 에이전트 계약
  문서를 출력한다.
- **멤버·routine id 충돌은 `@kind` 접미사로 분리한다** (`40d932f`) — base
  id가 다른 kind에 점유됐으면 `name@kind`로 옮기고 limitation으로 신고.
  MySQL의 FK 자동 인덱스가 컬럼과 이름을 공유해 드랍되던 것이 이제
  `order_items.order_id@index`로 산다. usage도 분리된 정점에 붙는다.
  파서는 `resolve_renamed`로 분리된 정점을 찾는다 — 유령 id로 간선을
  만들지 않는다. 모든 멤버 id에 kind를 박는 v2는 **보류 결정**(호환 깨짐).
- 2026-09-19: **P4 프로브 보강 완료** (커밋 예정). MSSQL·Oracle·SQLite
  프로브를 실서버로 검증하고 MariaDB fixture를 자동화했다.
  - **MSSQL** — 몸체는 `sys.sql_modules.definition`(INFORMATION_SCHEMA는
    4000자 절단), routine은 `sys.objects`+`sys.parameters`. JDBC 메타는
    프로시저를 함수로 오분류하고 `;N`(numbered procedure) 접미사를 붙여
    meta 호출을 건너뛰고 정규화했다. mssql-jdbc(MIT)를 번들. 검증 이미지는
    Azure SQL Edge — 공식 2022 이미지는 ARM QEMU에서 죽고 Edge는
    arm64/amd64 모두 돈다.
  - **Oracle** — `ALL_*`+`ALL_SOURCE` 수확, `ALL_ARGUMENTS` 시그니처.
    PUBLIC 시노님 슈도스키마는 ORA-01000 커서 고갈을 일으켜 방언 한정
    억제(isSystem)로 막고, Database Vault(DVF/DVSYS)도 억제. view의
    getIndexInfo는 ORA-20000이라 인덱스는 table/matview만 수확. routine은
    plpgsql과 같은 계약으로 `plsql` limitation. **대소문자 해석(ci)**이
    새로 생겼다 — Oracle 미인용 식별자는 대문자 접힘이라 몸체의 소문자
    참조를 대문자 정점으로 연결한다(대소문자 구분 방언에 켜면 다른
    객체를 가리키므로 oracle에만). 모호한 대소문자 충돌은 추측하지
    않고 miss+note. ojdbc는 OTN이라 `--driver` 외부 공급. 검증은
    gvenzl/oracle-free(arm64 지원 — XE엔 ARM 빌드 없음).
  - **SQLite JDBC** — TABLE_SCHEM/CAT이 null이라 `main` 귀속 + `conn.
    isReadOnly` 거부 드라이버 건너뜀 + `sqlite_master` 몸체 → 네이티브와
    정점·간선 완전 패리티(67/67).
  - **routine 통계 확장** — `Usage`에 `total_ms`/`self_ms`(선택 f64)
    additive 추가. PG는 `pg_stat_user_functions.total_time`/`self_time`을
    수확, MySQL은 per-routine 통계 소스가 없어 미수확 유지.
  - **MariaDB 자동 fixture** — verify-fixtures.sh가 mariadb:11.4를 PFS
    OFF(기본값, limitation 검증)→ON(수확 검증) 두 단계로 띄우고 전용
    골든을 비교한다(int(11) 표기·sys 뷰 범위가 MySQL과 달라 공유 골든
    불가). 프로브 경로도 mysql 드라이버로 검증.
  - fixture 적용 도구 `Scripts/ApplySql.java` — sqlcmd 없는 이미지를 위해
    GO·`/` 구분자로 나눠 JDBC로 실행한다(단일 파일 소스 실행, JDK 11+).
    ServiceLoader로 드라이버를 찾으므로 `-cp`로 드라이버 jar을 받는다 —
    번들 드라이버는 fat jar을, Oracle은 ojdbc를 넘긴다.
  - **검증 인프라 결함 두 개 수정** — shadow `mergeServiceFiles()`가
    `META-INF/services/java.sql.Driver`를 첫 항목(postgresql)만 남기고
    합치지 못해 `java -cp fat.jar` 경로의 ServiceLoader가 드라이버를
    못 찾았다. probe/src/main/resources에 번들 드라이버 4개를 직접
    선언해 해결. verify-fixtures.sh는 소유 docker 컨테이너를 EXIT
    트랩까지 살려뒀는데, 4GiB급 docker VM에서 mysql+maria+mssql+oracle
    동시 기동으로 Oracle이 OOMKilled로 죽었다 — 각 섹션이 끝날 때
    컨테이너를 정리하게 바꿨다.
- 2026-09-19: **P5 완료 — 판정 도구 + 프로브 확장**.
  - `diff`(`fedb998`) — graph↔graph·document↔document 구조 델타
    (added/removed vertices·edges + bodyChanged 등), `--strict` 델타 시 1.
  - `plpgsql`/`plsql` 문장 추출(커밋 예정) — 절차형 스캐폴딩을 걷어내고
    보이는 SQL만 문장 단위 파싱. 한 문장 실패가 전체를 죽이지 않고
    미추출은 limitation 카운트. `SELECT/RETURNING … INTO`의 변수는
    참조로 오인하지 않게 INTO를 벗긴다. `plpython3u` 등 나머지는
    여전히 limitation. 검증 스크립트의 "plpgsql 미지원" 기대는 실제
    간선 assertion으로 갱신됐다 — 바이너리와 스크립트의 기대가 어긋나
    실행 중 재빌드로 오탐이 난 적 있다(검증 실행 중 `cargo build`로
    바이너리 교체 금지).
  - Oracle PACKAGE 수확 — PACKAGE+BODY를 `package` 정점으로 병합,
    멤버별 귀속 미지원은 limitation 신고.
  - `inferred` 간선(`scan --inferred`) — 미선언 `*_id`의 이름 규칙 추정.
    선언 FK 우선·모호 후보는 limitation, `EvidenceLayer::inferred`라
    `is_dependency()`가 거짓 — impact·cycles·dead에 섞이지 않는다.
    sqlite fixture에 `shipments.customer_id`를 넣어 검증.
  - NDJSON 전송 — 프로브 `--format ndjson`, 엔진 `scan --document`가
    `type:"document"` 첫 행으로 자동 감지. 알 수 없는 레코드 타입은
    건너뛰지 않고 거부. source::ndjson이 읽기/쓰기 양방향.
  - document v2 정책 — 같은 버전의 미지 필드는 받되 무시된 경로를
    limitations에 신고(`unknown_field_paths`, additive 계약의 정직한 끝).
  - **Go 프로브**(`probe-go/`) — go-ora pure-Go 드라이버로 Oracle 전용
    단일 정적 바이너리. ojdbc(OTN)도 JVM도 없이 JVM 프로브와
    85/85 간선 완전 패리티(실검증). verify-fixtures가 go가 있으면
    Oracle 섹션에서 패리티를 검증한다.

## 확정된 결정

- 이름: `schemagraph`
- 언어: **Rust 엔진 + Kotlin/JVM 프로브 복합** — 버전 달린 catalog document가 경계
- 지원 범위: JDBC가 닿는 모든 DB. Tier 0 Generic 프로브가 바닥이고,
  네이티브 sqlx로 PG·MySQL·SQLite를 먼저 깊게 판다
- 차별점: FK뿐 아니라 view·routine·trigger 몸체 파싱 간선 + 에이전트용 판정 질의

## 구현하면서 정한 것들

- `query` 출력에 `selfEdges` 필드 — 자기 참조 FK는 이웃이 아니라 자기 간선으로
  별도 보고(cycles의 `selfLoop`와 같은 처리). 빈 필드는 키 생략.
- `project()`는 원래 자기루프만 보존한다 — 투영으로 붕괴된 간선과 구분하는 게
  핵심이라 테스트로 고정.
- `add_edge` 병합은 in/out 양쪽 인덱스에 적용 — 방향마다 evidence가 달라지면 안 된다.
- SQLite는 `mode=ro`로 읽기 전용 접속. attached db도 `database_list`로 다 읽는다.
- `cargo test`는 CLI 바이너리를 갱신하지 않을 수 있다 — verify 전 `cargo build` 필수
  (verify-fixtures.sh 주석에도 명시).
- **몸체 파싱은 reader 밖, 엔진의 일** — sqlite.rs는 원문 SQL만 옮기고,
  cli의 scan 경로가 `parser::enrich_from_document`를 불러 간선을 보강한다.
  파서 노트는 그래프 limitations에 합류한다.
- 파서는 sqlparser-rs `visitor` feature의 `visit_relations`/`visit_expressions`를
  쓴다. 테이블 참조는 전 쿼리에서 수집하지만, **컬럼 참조는 최상위 select의
  별칭 맵으로만** 해석한다 — 서브쿼리·CTE가 있으면 `has_nested_scope`를 세우고
  "컬럼 참조 미해석"을 limitations로 남긴다(추측하지 않는 원칙).
- 파싱 실패·없는 대상 참조는 유령 정점을 만들지 않고 notes로 보고한다.
- `impact`는 `query`의 dependents를 무제한 깊이로 펼친 것 — distance가 전파 거리.
  `--max`(기본 1024)로 잘림을 `truncated`로 보고한다.
- **trigger는 BEGIN..END 껍질을 직접 벗긴다** — sqlparser는 CREATE TRIGGER를
  못 파는 방언이 많다. 내부 문장만 파서에 넘기고, 껍질이 없는 몸체는 통째로
  파싱한다(reader가 내부 문장만 저장한 경우). 키워드 탐색은 원문에서
  case-insensitive + 단어 경계 — to_uppercase는 비ASCII 오프셋을 틀어뜨린다.
- `NEW.x`/`OLD.x`는 발사 테이블의 member reads로 해석한다. DML 대상은
  writes, 나머지 관계는 reads — routine 몸체 파싱이 오면 같은 골격을 쓴다.
- `dead`의 후보 kind는 "존재하려면 호출자가 필요한" 것들뿐 — view·
  materialized-view·function·procedure·package. table은 직접 조회,
  trigger는 자동 발사, 나머지는 내부 장치다. fixpoint로 의존자 전부가
  dead인 객체도 연쇄 판정한다. "앱 쿼리는 그래프에 없다"는 고정
  limitation이 항상 실린다 — 삭제 판정 아님의 계약.
- `source::read(url)`이 스킴 디스패치다 — postgres/postgresql→sqlx PG,
  mysql/mysqlx→미지원 오류(또라이 SQLite로 읽지 않는다), 나머지→SQLite.
- PG 리더는 `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`로
  읽기 전용을 강제한다. 카탈로그 `"char"` 컬럼(contype·prokind)은
  `::text`로 캐스팅해야 sqlx 디코딩이 된다. `conkey`는 NULL 가능 →
  `Option<Vec<i16>>`. 시그니처 빈 routine id는 `()`를 붙이지 않는다.
- PG `pg_get_viewdef`는 FROM 전체를 괄호로 감싼 `NestedJoin`을 만든다 —
  별칭 수집이 재귀해야 member reads가 생긴다.
- PG trigger는 `EXECUTE FUNCTION fn()` 호출이라 BEGIN..END가 없다 —
  `extract_execute_targets`가 껍질에서 직접 채취하고, `resolve_routine`이
  정확 id → 이름 매치 순으로 보수적 해석한다(모호하면 간선 생략+notes).
- routine 몸체는 `language=sql`/무표기만 파싱한다. `pg_get_functiondef`의
  `AS $$…$$`·`AS '…'` 껍질은 `extract_as_body`가 벗긴다. plpgsql 등은
  파싱하지 않고 limitation — 몸체 간선이 없다는 사실을 숨기지 않는다.
- **MySQL information_schema는 문자열 컬럼이 binary collation으로 온다** —
  모든 문자열 컬럼에 `CAST(x AS CHAR)`가 필요하고, ENUM 컬럼
  (table_type·constraint_type·routine_type·is_nullable)도 마찬가지다.
  `non_unique`는 INT라 i64로 받는다.
- MySQL `parameters`는 함수 **반환값도 행으로** 준다(ordinal_position=0) —
  시그니처에서 빼야 `fn()` 같은 가짜 인자가 안 생긴다. 같은 이름의
  function/procedure는 시그니처까지 같으면 정점 id가 충돌 — limitation으로
  보고하고 합쳐진다는 사실을 숨기지 않는다.
- MySQL routine 몸체는 `language=sql`로 채운다(카탈로그에 언어 열이 없고
  MySQL 저장 루틴은 SQL뿐이라는 제품 지식). FUNCTION의 RETURN도
  sqlparser가 파싱한다 — 실제로 reads 간선이 나오는지 fixture로 확인했다.
- 읽기 전용: MySQL 8은 `SET SESSION transaction_read_only=1`, MariaDB는
  `tx_read_only` — 둘 다 시도하고 실패해도 스캔은 계속한다.
- **통계가 꺼진 환경은 0행이 아니라 미수집이다** — PG `track_functions=
  none`이면 pg_stat_user_functions가 0행, MariaDB는 `performance_schema=
  OFF`가 기본값이라 sys 뷰가 0행. 둘 다 "관측된 0"이 아니라 미수집이라
  비활성을 감지해 limitation으로 남긴다 — 비어 있는 이유를 숨기지 않는다.
- routine usage는 `pg_stat_user_functions.calls`를 reads에 싣는다 —
  funcname엔 시그니처가 없어 오버로드면 귀속 못 한다(이중 집계 방지로
  이름 유일할 때만). 모호하면 limitation. MySQL엔 routine 통계 뷰가 없어
  미수확.
- **MariaDB는 MySQL과 information_schema 부호가 다르다** —
  `ordinal_position`이 MySQL은 UNSIGNED, MariaDB는 SIGNED로 온다.
  쿼리에서 `CAST(... AS SIGNED)`로 통일해 같은 i64 디코딩을 쓴다.
- 프로브의 `ROUTINE_TYPE` NULL 행(PG 집계 함수)은 건너뛴다 — null-safe로
  받지 않으면 NPE가 수확 루프를 중단시켜 행 순서에 따라 몸체가 빠진다.

## 검증 명령

```bash
cd engine && cargo build && cargo test        # 빌드 + 58개 테스트
Scripts/verify-fixtures.sh                    # SQLite + PG + MySQL + MariaDB + JDBC probe 양방향 검증
# PG는 initdb로, MySQL·MariaDB·MSSQL·Oracle은 docker로 자동 프로비전한다.
#   SG_PG_URL=postgres://user@host/db Scripts/verify-fixtures.sh      (폐기용 DB만!)
#   SG_MYSQL_URL=mysql://user@host/db SG_MYSQL_CONTAINER=<이름> ...
#   SG_MARIADB_URL=mysql://user@host/db ...
#   SG_MSSQL_URL='jdbc:sqlserver://host:port;databaseName=db' ...
# probe는 java를 자동 탐색하고 jar이 없으면 gradle로 빌드한다. MySQL·Oracle
# 드라이버는 라이선스상 외부 공급:
#   SG_MYSQL_JAR=/path/mysql-connector-j.jar SG_ORACLE_JAR=/path/ojdbc11.jar ...
# MSSQL·Oracle fixture 적용은 Scripts/ApplySql.java가 GO·`/` 구분자로 한다.
```

## 다음 할 일

1. 배포 준비 — crates.io `schemagraph`·Maven `schemagraph` 모두 비어
   있음(2026-09-19 확인). 선점·퍼블리시는 아직 하지 않았다.
2. routine 파싱 잔여 — T-SQL 전용 구문(TRY/CATCH·CURSOR), Oracle 패키지
   멤버별 귀속(ALL_ARGUMENTS의 PACKAGE_NAME으로 시그니처 복원).
3. 프로브 측 진짜 스트리밍 — NDJSON은 직렬화만 행 단위다. Extractor를
   행 방출로 고치면 대형 카탈로그도 상수 메모리로 수확된다.
4. Go 프로브 확장 — Oracle만 커버. pure-Go 드라이버가 있는 방언
   (pgwire·mysql·sqlserver·sqlite)으로 수요별 확장.

## 미결

DESIGN.md "미결 사항" 절 참조: document 스키마 v2(additive 너머의 협상),
프로브 Tier 1 방언 확장, 멤버 id v2(보류 결정 유지).
