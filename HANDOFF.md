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
- 2026-09-19: **P2 완료 — Kotlin/JDBC 프로브** (커밋 예정). `probe/`가
  `DatabaseMetaData`(스키마·테이블·컬럼·PK·FK·인덱스·routine) + best-effort
  몸체 쿼리(INFORMATION_SCHEMA, Oracle은 ALL_*)로 버전 달린 catalog
  document를 뱉고, `schemagraph scan --document`가 먹는다. 번들 드라이버는
  허용 라이선스만(pgjdbc·H2·sqlite-jdbc), 나머지는 `--driver`로 주입.
  MySQL 실검증에서 네이티브 reader와 의존성 간선 22개 완전 일치. PG는
  trigger calls까지 패리티. H2 임베디드로 서버 없는 end-to-end 검증이
  verify-fixtures.sh에 들어갔다.

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

## 검증 명령

```bash
cd engine && cargo build && cargo test        # 빌드 + 46개 테스트
Scripts/verify-fixtures.sh                    # SQLite + PG + MySQL + JDBC probe 양방향 검증
# PG는 initdb로, MySQL은 docker로 임시 인스턴스를 자동 프로비전한다. 직접 지정:
#   SG_PG_URL=postgres://user@host/db Scripts/verify-fixtures.sh      (폐기용 DB만!)
#   SG_MYSQL_URL=mysql://user@host/db SG_MYSQL_CONTAINER=<이름> ...   (컨테이너면 docker exec로 적용)
# probe는 java를 자동 탐색하고 jar이 없으면 gradle로 빌드한다. H2 임베디드가
# 기본이고, PG가 떠 있으면 pgjdbc로 네이티브 패리티를 확인한다. MySQL까지:
#   SG_MYSQL_JAR=/path/mysql-connector-j.jar ...
```

## 다음 할 일

1. `stats` — pg_stat·information_schema.table_statistics 같은 사용 통계를
   증거 계층으로 붙여 `dead` 후보의 근거를 강화한다 (P3).
2. MariaDB 검증 — reader는 같은 information_schema지만 시퀀스·
   tx_read_only 등 차이가 있어 별도 fixture가 필요하다.
3. `skill` — 에이전트용 스킬 문서 출력 (P3).
4. 프로브 보강 — MSSQL `sys.*` 몸체 소스, Oracle ALL_* 실서버 검증,
   SQLite JDBC의 스키마 귀속 확인.

## 미결

DESIGN.md "미결 사항" 절 참조: document 스키마 세부, routine 파싱 커버리지,
프로브 전송 방식, crates.io/Maven 이름 선점 확인, inferred 휴리스틱.
