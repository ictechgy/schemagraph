# HANDOFF.md

## 현재 상태 (2026-11-22, 라운드 3 작업 브랜치)

- 발행본은 **0.9.0**이다. 태그 `v0.9.0` = `fad8e9d`([#58](https://github.com/ictechgy/schemagraph/pull/58)).
  그 커밋의 [CI](https://github.com/ictechgy/schemagraph/actions/runs/41583763434)·
  [SQL Server/Oracle](https://github.com/ictechgy/schemagraph/actions/runs/41583763488)·
  [IBM](https://github.com/ictechgy/schemagraph/actions/runs/41583763549)·
  [DML](https://github.com/ictechgy/schemagraph/actions/runs/41583763689) 검증이 모두 성공했다.
- [PR #59](https://github.com/ictechgy/schemagraph/pull/59)(`bdac8a6`, 2026-11-09)와
  [PR #61](https://github.com/ictechgy/schemagraph/pull/61)(`b8e4054`, 2026-11-16)이 `main`으로 스쿼시 머지됐다.
  아직 발행하지 않았다. RELEASE-NOTES `# Unreleased`가 미발행 변경 목록의 정본이다: facts의 스키마 필터·교환
  불가 식별자 범위 공백과 SQLite 예약 관계 선언, COPY 테이블·컬럼 간선, SQLite 수집기 3종 수정(네이티브·Go·JDBC),
  대소문자 접기 방언의 관계·routine 해석과 몸체 캐시 지문, 서버 대소문자 규칙 수집(`context.lower_case_table_names`·
  `collation`), 프로브 메시지 영어화, JDBC 파티션·외부 테이블과 방언별(대소문자 구분) 시스템 스키마,
  CI의 isthmus 0.14.0 실검증, SQLite ASCII 식별자 접기, 컴파일되지 않는 SQLite 뷰, 그리고 아래 라운드 3 항목.
- 채널별 게시 현황은 RELEASE-NOTES.md 끝의 "Publication channels" 표가 정본이다.
  crates.io에는 0.7.1·0.8.0·0.8.1이 없고, Maven Central에는 0.7.1이 없다(2026-11-03 조회).
  GitHub Release v0.1.0–v0.3.0에는 첨부 아카이브가 없다(2026-11-09 조회). 누락 버전은 다시 게시하지 않는다.
- isthmus `compatibility.json`은 아직 schemagraph 0.7.0을 고정한다. 갱신은 isthmus 저장소의 일이다.
- 브랜치 `ccr-37904670-hxdjsx`는 `origin/main` `b8e4054`에서 다시 시작한 라운드 3 작업이며
  [PR #62](https://github.com/ictechgy/schemagraph/pull/62)다. 아래 "라운드 3" 절의 네 커밋(`1c8d976`·`86c9e98`·`0206bea`·
  `d91236b`), 독립 리뷰 반영 세 커밋(`5be9173`·`d872048`·`f9c700b`), HANDOFF 커밋(`0161c96`), PR #62 2차 리뷰 반영
  (`6144529`·`982941d`·`5699c2d`, push됨), 3차(델타) 리뷰 반영 커밋 `9528ca3`(parser)과 그 HANDOFF 커밋 `8fb92cc`, 그리고 4차(대항 행렬) 리뷰 반영
  `fbee2c3`·`60589e9`(parser)와 그 HANDOFF 커밋(아래 절, 아직 push 전)이 있다.

### 라운드 3 (2026-11-22)

HANDOFF 후속 후보 4번 목록에서 사용자 결정이 필요 없는 정확성·위생 항목을 골랐다. 근거와 결과의 정본은
RELEASE-NOTES `# Unreleased`의 해당 항목이다. 회귀 테스트는 모두 수정을 되돌리거나 변이시킨 사본에서 실패하는 것을
확인했다.

- parser — 앞에 WITH가 붙은 DML(`WITH big AS (…) INSERT|UPDATE|DELETE …`, 모든 방언). sqlparser는 이 문장을
  `Statement::Query { body: SetExpr::Insert|Update|Delete }`로 감싼다. 재현(수정 전 CLI): SQLite `--sql-dir`의
  `WITH big AS (SELECT id, total FROM orders WHERE total > 10) INSERT INTO log (order_id, amount) SELECT id, total FROM big`는
  `reads orders`·컬럼 reads만 남고 `writes log`가 없었으며 `partial` + `SG_SET_UNSUPPORTED`(한계 문구는 "view main.query_…"),
  같은 몸체의 PostgreSQL 함수는 `writes log`·`reads orders`만 남기고 컬럼 효과 없이 `complete`였다. 수정:
  `query_dml_statement`로 감싼 DML을 꺼내 `is_select`(DELETE 포함)·`is_column_dml_statement`(INSERT·UPDATE)·
  `qualified_write_targets`(3부분 대상)가 보게 했고, `DmlAnalyzer`가 그 WITH의 CTE를 `Binder::bind_ctes`(SELECT와 같은
  CTE 규칙, `Binder::query`에서 뽑아 냄)로 해석해 INSERT·UPDATE의 원천에 보이게 한다(sqlparser 0.58은 `WITH … MERGE`를
  파싱하지 못해 MERGE에는 해당하지 않는다). 대상이 그 문장의 CTE
  이름일 때의 처리는 리뷰 반영(아래)에서 방언별로 바꿨다.
  수정 후 같은 CLI 재현에서 `archive.sql`이 `plain.sql`(WITH 없는 같은 문장)과 같은 `writes log`·`log.order_id`·
  `log.amount`, `derives-from`, `complete`를 낸다. 테스트 5개(`with_prefixed_*`, `scope_tests.rs`): INSERT(PostgreSQL 쿼리
  파일·프로시저, SQLite·SQL Server 쿼리 파일), UPDATE … FROM CTE, SQLite DELETE 쿼리 파일, CTE 대상(직접·`UPDATE n … FROM
  named n` 승격), 3부분 대상. 변이 6종(각 변경 하나씩 되돌림)을 모두 잡는다. 결과는 간선 추가뿐이다(좁힘 아님).
- source — 앱 등록 모듈을 쓰는 SQLite 가상 테이블. 재현(수정 전): `writable_schema`로 `CREATE VIRTUAL TABLE vt USING
  app_mod(a, b)` 행을 넣은 DB를 `scan`하면 `catalog read failed: … (code: 1) no such module: app_mod`, 종료 코드 2.
  같은 DB에서 Go 프로브(`sqlite columns source text was not collected (…)`)와 JDBC 프로브(`main.vt: getColumns failed`)는
  이미 테이블을 남기고 카탈로그를 불완전으로 표시했다(둘 다 이 세션에서 실행). 수정: `sqlite.rs`의 `UnreadableColumns`가
  컴파일 불가 뷰와 같은 분류(SQLITE_ERROR 계열만)로 DDL이 `CREATE VIRTUAL TABLE`로 시작하는 테이블만 남기고 한계를 따로
  센다. 일반 테이블·다른 오류는 그대로 종료 코드 2다. 수정 후 같은 DB는 종료 코드 0, 한계 2개(그 테이블을 읽는 뷰
  `v_vt`는 컴파일 불가 뷰로), `catalog_complete: false`, facts에 `catalog-coverage:`. 그 테이블을 읽는 `SELECT *`는
  `SG_WILDCARD_UNKNOWN`으로 `partial`이다(쿼리 파일·뷰 둘 다 확인). FTS5처럼 내장 모듈은 이전처럼 읽힌다. 테스트 3개
  (통합·키워드 판정·실제 SQLite 오류 경계), 변이 3종을 잡는다. scan이 아예 실패하던 입력이라 출력은 늘기만 한다.
- probe-go — SQLite 객체별 수집 실패 문구에 `schema.object:`를 붙였다(컬럼·PK 인덱스·FK·인덱스·인덱스 컬럼). 재현(수정
  전): 같은 SQLite 메시지로 실패한 뷰 두 개가 `sortedDistinct`로 한 문구가 되어 어느 뷰가 몇 개 빠졌는지 사라졌다
  (가상 테이블 DB에서도 `vt`·`v_vt`가 한 문구). 테스트 `TestSQLiteColumnFailuresNameTheObject`(수정 전 실패 확인).
  SUPPORT-MATRIX 수집기 차이 표를 고치고 가상 테이블 행을 더했다.
- ci — `Scripts/verify-cancellation.py`가 실패한 검사 이름과 표준 라이브러리 밖의 가장 안쪽 함수·줄을 적는다
  (`failed in check_mcp (send_json line …): could not write MCP request: [Errno 32] Broken pipe`). 재현: 수정 전 스크립트에
  BrokenPipeError를 주입하면 `cancellation process regression failed: [Errno 32] Broken pipe`만 나왔다.
  `Scripts/test-verify-cancellation.py`(6개, 수정 전 스크립트에서 모두 실패)를 CI의 측정 불변식 단계에 더했다.
  실제 스크립트는 이 세션에서 `ok`다.

### 라운드 3 독립 리뷰 반영 (2026-11-22)

GLM `packet-ask`는 이 세션에서 쓸 수 없어 `/code-review high`(독립 Claude 리뷰)를 `origin/main...HEAD`에 돌렸다.
지적 8개를 실제 DB·문서·테스트로 확인한 뒤 처리했다.

- 반영 `5be9173`(important 2개): PostgreSQL·SQLite에서 WITH 이름과 같은 DML 대상은 실제 테이블이다(PostgreSQL
  `parse_clause.c` `setTargetTable`의 "CTEs don't hide tables (for this purpose)" 주석을 REL_16_STABLE 원문으로 확인하고,
  PostgreSQL 16.15와 SQLite 3.53.1에서 INSERT·UPDATE·DELETE가 실제 `audit`를 바꾸는 것을 실행해 확인). 그 두 방언은
  대상을 CTE 없이 찾는다. SQL Server는 MicrosoftDocs `with-common-table-expression-transact-sql.md` 원문("Any reference to
  expression_name in the query uses the common table expression and not the base object")대로 CTE를 통해 쓰므로, 두
  방언 밖에서는 그 이름으로 가는 객체 단위 `writes`를 남기지 않고 `SG_DML_TARGET_SHAPE` 하나로 센다. 좁힘: 그 방언들의
  routine·trigger가 이전부터 남기던 CTE 이름 쓰기 간선(없는 관계면 "missing" 한계, 같은 이름 테이블이 있으면 거짓
  간선, 수정 전 CLI의 SQL Server 프로시저 재현에서 `writes dbo.shadow -> dbo.log`)이 사라지고 진단으로 바뀐다. 이때의
  CTE 이름 비교(ASCII 대소문자 무시)는 2차 리뷰에서 문서의 대소문자 규칙을 따르게 고쳤다(아래).
- 반영 `d872048`(nit): Go 문구의 객체 이름 제어문자를 `\u{..}`로 이스케이프한다.
- 반영 `f9c700b`(nit 3개): 실패 위치를 표준 라이브러리 밖 프레임으로, 정리 실패가 검사 실패를 가리지 않게,
  RELEASE-NOTES 예시를 실제 문구로.
- 보류(pre_existing 2개): CTE 원천 조회의 SQL Server 대소문자 무시(대소문자 접기 방언 후속 후보에 포함)와
  데이터 수정 CTE(`WITH ins AS (INSERT … RETURNING …) SELECT …`, 아래 후속 후보).

### PR #62 리뷰(2차) 반영 (2026-11-22)

리뷰는 모델 출력이라 리뷰어 도구(`run.py`·`cases.py`, main·branch 빌드)로 주장마다 다시 재현했고, main·branch·수정본을
같은 사례 파일(cases1–7과 추가 사례)로 돌려 main에만 있던 간선을 찾았다.

- should-fix 1(절차형 블록): `IF 1 = 1 WITH audit AS (…) UPDATE audit …`(SQL Server)와 MySQL `IF … THEN WITH t AS (…)
  UPDATE t …; END IF`에서 거짓 `writes s.audit`·`writes s.t`가 남았다. 부모 `Statement::If`의 쓰기 대상 수집이 안쪽 문장까지
  내려가는데 CTE 이름 판정은 최상위 `Statement::Query`만 봤다. `split_write_targets`가 visitor로 감싼 DML을 만날 때마다 그
  안쪽 DML의 주소와 CTE 이름을 기억해 그 DML의 대상만 가른다(`IF` 안의 다른 문장이 같은 이름의 실제 테이블에 쓰면 그 쓰기는
  남는다).
- should-fix 2(새 회귀): `WITH … DELETE`를 DML 경로로 옮기면서 DELETE를 컬럼 분석기로 보내지 않아, 읽기 경로가 남기던 CTE
  몸체의 컬럼 읽기(PostgreSQL `reads s.audit.id`, SQL Server 중복 삭제 관용구의 `customers.id`·`name`, MySQL 다중 테이블
  DELETE의 `orders.id`)가 사라졌다. 앞에 WITH가 있으면 DELETE도 `DmlAnalyzer`로 보내 CTE를 해석한다. DELETE 자체의 WHERE
  컬럼 읽기는 WITH 없는 DELETE처럼 남기지 않으므로 상태는 `complete`다(아래 후속 후보). 수정 후 리뷰 사례 전체에서 main에만
  있는 간선은 문서화한 좁힘(SQL Server CTE 이름 대상의 `writes`)뿐이다.
- nit 3(collation): CTE 이름 비교를 문서의 관계 이름 규칙(`relation_names_fold_case`: SQL Server `_CI` collation, MySQL
  `lower_case_table_names`≠0, Oracle·DB2)에 맞췄다. 파싱 단계는 대소문자 무시로 후보(`CteWrite`, 철자 일치 여부 포함)만 가르고,
  적용 단계(`settle_cte_writes`)가 `ci`로 판정해 CTE가 아니면 평범한 쓰기로 되돌린다. 컬럼 단위(`dml_target_naming_cte`)도
  같은 `fold_relation_case`를 쓴다. 그래서 CS collation·collation 없음에서 `WITH Dupes … DELETE FROM dupes`의 `writes s.dupes`가
  main처럼 남는다(3차에서 collation 없음은 쓰기를 남기되 진단·부분 분석으로 바꿨다). SQL Server 문서(MicrosoftDocs `collation-and-unicode-support.md`: 데이터베이스 collation은 "all metadata in
  the database … and any other strings used in the database")를 근거로 삼았지만 CTE 이름을 직접 말하지는 않아 실서버 확인이
  남았다.
- 방언 규칙(`CteTargetRule`, 2차 반영 중 좁힘 근거를 다시 따져 바꿈): 1차 반영은 PostgreSQL·SQLite 밖의 모든 방언에서 CTE
  이름 대상의 쓰기 간선을 지웠지만, 근거가 있는 방언은 SQL Server(문서)뿐이었다. MySQL은 mysql-server 8.0
  `mysql-test/include/with_non_recursive.inc`·`r/with_non_recursive.result`(GitHub 원문)로 확인했다 — CTE가 같은 이름의
  임시 테이블을 가리고(`QN shadows tmp table`), CTE를 UPDATE·DELETE 대상으로 쓰면 `ER_NON_UPDATABLE_TABLE`("The target table
  qn of the DELETE is not updatable"). 그래서 `Table`(PostgreSQL·SQLite), `Cte`(SQL Server·MySQL: 쓰기 간선 대신 진단),
  `Unknown`(MariaDB·Oracle·DB2·Informix·H2·external: 객체 단위 쓰기는 main처럼 남기고 컬럼만 추측하지 않으며 진단 문구는
  `its target columns are not resolved`, 3차에서 DELETE·대소문자 판정과 문구를 고침)으로 나눴다. 판정은 문서 방언 이름이 있는 적용 단계(`settle_cte_writes`)에서 한다.
- nit 4: 진단 문구에 대상 이름을 싣는다(`DML target 'dupes' names a WITH query; …`, 제어문자 이스케이프). 객체·컬럼 단위가
  같은 문구를 쓰고 대상마다 한 번만 센다 — 두 대상이면 진단 두 개다.
- nit 5(Go): `oneLine`이 공백을 접은 뒤 남은 제어문자를 `\u{..}`로 바꾼다. SQLite 이유 문구(`no such table: main.gone\x1b[31m`)를
  포함해 Go 프로브의 모든 한계·오류 문구에 적용된다.
- nit 6: MERGE 경로의 `target_ctes`·`self.ctes`는 sqlparser 0.58이 `WITH … MERGE`를 파싱하지 못해 실행될 수 없었다 — main처럼
  빈 CTE로 되돌리고 HANDOFF 문구를 고쳤다. RELEASE-NOTES에 데이터 수정 WITH 질의를 가진 WITH-DML(`WITH del AS (DELETE …)
  INSERT …`)이 routine에서 complete→partial(`SG_SET_UNSUPPORTED`, 바깥 INSERT의 컬럼 쓰기는 추가)로 바뀐다는 사실을 적었다.
- 회귀 테스트: 새 `cte_target_diagnostics_name_each_target`, `sqlserver_cte_target_names_follow_the_collation`,
  `with_prefixed_delete_keeps_the_cte_column_reads`, 바꾼 `sqlserver_dml_through_a_cte_is_partial_without_guessed_targets`(절차형
  블록 두 사례·이름 확인), Go `TestSQLiteColumnFailureReasonEscapesControlCharacters`. `0161c96`의 parser 코드에서 새·바꾼 Rust
  테스트 4개가 실패하고, 변이 6종(적용 판정 항상 CTE·철자 일치만, WITH DELETE 미전달, 이름 없는 문구, 컬럼 단위 항상 접기,
  최상위만 가르기)을 각각 잡는다. 방언 규칙을 바꾼 뒤 `unverified_dialects_keep_the_object_write_of_a_cte_named_target`을
  더했고, 최종 코드에서 변이 8종(Unknown→Cte, MySQL→Unknown, 적용 판정 두 가지, 컬럼 단위 접기, 최상위만 가르기, WITH
  DELETE 미전달, 이름 없는 문구)을 모두 잡으며 `0161c96` 코드에서는 새·바꾼 테스트 5개가 실패한다. Go 테스트는 이스케이프를
  되돌리면 실패한다.
- 리뷰 사례 cases1–7(+CS·CI collation)을 main과 최종 바이너리로 돌려 비교했다. main에만 있는 간선은 SQL Server·MySQL에서 CTE
  이름 대상으로 가던 `writes` 9건뿐이다(문서화한 좁힘).

### PR #62 리뷰(3차, 델타 `0161c96..5699c2d`) 반영 (2026-11-22)

원칙: 방언 규칙이나 이름의 대소문자 규칙이 증명되지 않으면 쓰기 간선은 main처럼 남기되 `complete`를 주장하지 않는다 —
대상마다 이름 붙은 `SG_DML_TARGET_SHAPE`, 부분 분석, 추측한 컬럼 효과 없음. 판정은 새 모듈 `parser/src/cte_target.rs`
(`Rule`·`NameCase`·`Match`·`Outcome`·`Context`)에 모아 객체 단위(`settle_cte_writes`)와 컬럼 단위(`with_statement`)가
같은 판정과 같은 문구를 쓴다. 리뷰어 도구(`run2.py`·`cmp3.py`, c1–c10)로 재현했다.

- should-fix 1: 확인하지 않은 방언(MariaDB·`external` 등)의 `WITH audit AS (…) DELETE FROM audit`가 `writes s.audit`와
  `complete`, 진단 없음이었다(UPDATE만 컬럼 단위가 진단을 남겼다). 이제 `Unknown` 방언의 CTE 이름 대상은 객체 단위 적용이
  쓰기를 남기면서 진단을 붙인다(`…; this dialect's rule for such targets is unverified; the write edge is kept, but target columns
  are not resolved`).
- should-fix 2: 대소문자만 다른 대상(`WITH Audit … UPDATE audit`)을 collation이 없는 SQL Server·MariaDB에서 실제 테이블로
  단정해 `writes s.audit`·`s.audit.name`과 `complete`를 냈다(SQL Server 기본 collation은 `_CI`라 거짓일 수 있다). 이제
  `NameCase`가 증명할 때만 판정한다 — SQL Server `_CI`(터키어·아제르바이잔어·CP1254 제외)는 무시, `_CS`·`_BIN`·`_BIN2`는 구분,
  MySQL `lower_case_table_names` 0은 구분·1·2는 무시(mysql-server 8.0 `PT_common_table_expr` 생성자가 lctn≠0이면 CTE 이름을
  소문자로 바꾸고 `match_table_ref`가 memcmp로 비교, GitHub 원문 확인). 그 밖(collation 없음, 터키어 `_CI`, lctn 없음, ASCII 밖
  글자만 대소문자가 다름)은 쓰기를 남기고 진단·부분 분석이다(`…; the names differ only in letter case and the document does not
  prove the case rule; …`).
- nit 3: WITH DELETE를 컬럼 분석기로 보내는 판정(`retains_for_column_analysis`)을 컬럼 DML 판정과 분리했다. 절차형 블록 안의
  WITH DELETE가 다시 WITH 없는 DELETE처럼 `SG_DML_CONTROL_FLOW` 없이 complete다(PostgreSQL `IF … THEN`, MySQL `CASE`).
- nit 4: 대소문자를 무시하는 문서에서는 진단에 접은 이름을 실어 `dupes`·`DUPES`를 한 번만 센다. 문구를 방언별로 나눴다 —
  MySQL은 "rejects a WITH query as an UPDATE or DELETE target (ER_NON_UPDATABLE_TABLE)", SQL Server는 "writes through it to a
  base relation that is not resolved".
- 회귀 테스트: 새 `mysql_cte_target_names_follow_lower_case_table_names`, `with_prefixed_delete_in_control_flow_is_not_column_dml`,
  `cte_target_diagnostics_fold_names_under_a_case_insensitive_rule`, 바꾼 `sqlserver_cte_target_names_follow_the_collation`(collation
  없음·터키어는 부분 분석, BIN2, ASCII 밖 대소문자)·`unverified_dialects_keep_the_object_write_of_a_cte_named_target`(DELETE·쿼리
  파일·`external`·대소문자만 다름). 5개 모두 `5699c2d` 코드에서 실패하고, 변이 9종(Unknown→Table, Unproven 진단 생략, 모르는 대소문자
  →Table, 터키어 접기, MySQL 대소문자 모름, 접지 않은 표시, WITH DELETE를 컬럼 DML로 셈, ASCII 밖 대소문자→CTE, 컬럼 단위 판정
  무시)을 각각 잡는다.

### PR #62 리뷰(4차, 대항 행렬 7,050칸) 반영 (2026-11-22)

리뷰는 모델 출력이라 리뷰어 도구(`tools/one.py`·`matrix.py`·`analyze.py`)와 `8fb92cc` 바이너리로 주장마다 다시 재현했다.
원칙은 3차와 같다 — 증명되지 않으면 쓰기는 남기고 `complete`를 주장하지 않는다. 커밋: `fbee2c3`(parser 이름 비교·SQL Server),
`60589e9`(parser 별칭 DELETE), 그리고 이 HANDOFF.

- should-fix 1(어말 시그마, `5699c2d`부터): `name_match`가 `str::to_lowercase`로 비교해 `"ΑΣ".to_lowercase()`가 `"ας"`가 되고,
  CTE `ασ`와 대상 `ΑΣ`(대소문자만 다름)를 다른 이름으로 단정했다. 재현(수정 전): SQL Server `_CI_AS`·`Greek_CI_AS`, MySQL lctn=1,
  MariaDB·`external`(context 없음)에서 `writes s.ΑΣ`·`writes s.ΑΣ.name`·`complete`. 수정: 이름 비교를 collation 재구현 없이
  `cte_target::name_match`에 둔다 — 바이트가 같으면 같은 이름(`Exact`), 둘 다 ASCII면 증명된 대소문자 규칙(`AsciiCase`), 한쪽이라도
  ASCII 밖 글자가 있고 바이트가 다르면 `NonAscii`로, 바이트 비교 규칙(MySQL lctn=0, mysql-server 8.0 `match_table_ref`의 memcmp)에서만
  다른 이름으로 증명되고 접는 규칙·모르는 규칙에서는 `Unproven`이다. 수정 후 같은 재현은 `writes s.ΑΣ`만 남고 `partial`,
  `DML target 'ΑΣ' may name the WITH query 'ασ'; the names are not byte-equal and contain non-ASCII characters, and whether the
  comparison folds them (letter case, accents, character width) is not proven; …`(MariaDB·`external`은 방언 미확인 문구).
- nit 3(전각·악센트): 같은 규칙이 덮는다. `WITH ａｕｄｉｔ … UPDATE audit`(`_CI_AS`·`_CS_AS`)와 `WITH äudit … UPDATE audit`(`_CI_AI`)가
  `writes s.audit.name`·`complete`에서 쓰기만 남긴 `partial`이 됐다.
- 열린 질문 4(SQL Server CTE 이름의 collation): Microsoft 문서에 CTE 이름(질의 범위 이름)을 어느 collation이 비교하는지 말하는
  문장이 없다. learn.microsoft.com은 이 세션에서 DNS가 풀리지 않아 같은 페이지의 원본(`MicrosoftDocs/sql-docs` `live` 브랜치,
  raw.githubusercontent.com)을 읽었다: `docs/t-sql/statements/collations.md`(ms.date 03/03/2025: 데이터베이스 안 객체 식별자는
  데이터베이스 collation, "The identifiers for variables, GOTO labels, temporary stored procedures, and temporary tables are in the
  default collation of the server instance"), `docs/relational-databases/databases/contained-database-collations.md`(10/01/2025:
  비포함 DB에서 변수·GOTO 레이블·커서 이름은 인스턴스 collation, 포함 DB는 `CATALOG_DEFAULT` = `Latin1_General_100_CI_AS_WS_KS_SC`),
  `docs/relational-databases/collations/collation-and-unicode-support.md`(01/07/2026: 데이터베이스 collation이 "variable names"에도
  쓰인다고 해 앞 문서와 엇갈림, Azure SQL Database는 `CATALOG_COLLATION`), `database-identifiers.md`(04/08/2026),
  `with-common-table-expression-transact-sql.md`(04/10/2026: collation 언급 없음). 그래서 SQL Server의 `name_case`는 늘 `Unknown`이다
  — 철자가 정확히 같은 대상만 `Cte`이고, 대소문자만 다른 대상은 어느 collation에서도 쓰기를 남긴 `partial`(`…; the names differ only
  in letter case and the case rule for WITH names is not proven (SQL Server does not document which collation compares them); …`)이다.
  3차에서 `_CI`는 쓰기를 지웠고 `_CS`·`_BIN2`는 컬럼 쓰기와 `complete`를 냈다. 문서가 싣는 collation은
  `DATABASEPROPERTYEX(DB_NAME(), 'Collation')`(Go·JDBC)뿐이라 인스턴스·카탈로그 collation도 알 수 없다. 이 결정으로 `_BIN`·`_BIN2`가
  식별자를 코드 포인트로 비교하는지 확인할 필요도 없어졌다. 관계 이름(`relation_names_fold_case`)은 문서가 데이터베이스 collation을
  명시하므로 그대로 둔다.
- 표시·중복 제거(`Context::message`): CTE로 증명됐거나 철자가 같은 대상만 "names a WITH query"라 쓰고, 증명된 대소문자 무시 규칙
  (MySQL 1·2)에서는 글자 단위 `char::to_lowercase`로 접어 `dupes`·`DUPES`와 `ΑΣ`·`ασ`를 한 번만 센다(`str::to_lowercase`는 어말
  시그마로 둘을 갈랐다). 철자가 다른데 증명되지 않은 후보는 `may name the WITH query '<CTE 철자>'`와 두 철자를 그대로 실어 철자마다
  센다(같은 이름인지 모르므로). 바이트 비교 규칙(MySQL 0)에서는 접지 않는다.
- nit 2(별칭 DELETE, `5699c2d`부터): `WITH c AS (…) DELETE c FROM audit c JOIN orders o ON c.id = o.id`(SQL Server, MySQL lctn=0)가
  `c`를 CTE로 판정해 `audit` 쓰기를 지우고 `SG_DML_TARGET_SHAPE`를 냈다. WITH 없는 같은 문장은 main부터 `s.c`가 없는 관계라는 거짓 한계와
  `reads s.audit`만 남겼다. 근거: SQL Server DELETE 문서(`docs/t-sql/statements/delete-transact-sql.md`)의 *table_alias* "The alias specified
  in the FROM table_source clause representing the table or view from which the rows are to be deleted", mysql-server 8.0
  `sql/sql_parse.cc` `Query_block::add_table_to_list`가 다중 테이블 DELETE 별칭 목록(`TL_OPTION_ALIAS`)을 CTE로 찾지 않는다("those are
  only references into the FROM list, and any CTEs referenced by the aliases will be resolved when we later resolve the FROM list").
  수정: `write_targets_shallow`의 DELETE(`delete_write_targets`)가 대상 목록 이름을 FROM 절 관계(조인 포함)의 별칭으로 먼저 풀고
  (`aliased_relation`, `promoted_update_target`과 같은 ASCII 대소문자 무시·후보 하나 규칙), 그 관계 이름으로 CTE 판정을 한다. 수정 후 두
  재현 모두 `writes s.audit`·`complete`이고, 별칭이 가리키는 FROM 관계가 CTE면(`WITH audit … DELETE x FROM audit x …`) 이전처럼 CTE를
  통한 DELETE다. MySQL `DELETE FROM c USING audit c …` 형식은 바꾸지 않았다(후속 후보).
- 회귀 테스트: 새 `non_ascii_cte_target_names_are_unproven_under_folding_rules`(시그마·전각·악센트·`étAPE`/`étape`, SQL Server 4개
  context·MySQL 1·2·없음·MariaDB·`external`, 대조로 MySQL 0·SQLite), `sqlserver_case_only_cte_targets_are_unproven_under_every_collation`
  (이전 `sqlserver_cte_target_names_follow_the_collation`을 대체, collation 6종과 정확 철자), `delete_through_a_from_alias_writes_the_aliased_table`
  (SQL Server·MySQL, 조인한 별칭, WITH 없는 문장, CTE를 가리키는 별칭), 바꾼 `cte_target_diagnostics_fold_names_under_a_case_insensitive_rule`
  (MySQL 1의 `dupes`·`ΑΣ`/`ασ` 한 번, MySQL 0은 두 번)과 `unverified_dialects_keep_the_object_write_of_a_cte_named_target`(대소문자만 다른
  철자는 "may name"). 5개 모두 `8fb92cc` 코드(소스만 되돌리고 테스트 유지)에서 실패한다. 변이 11종(NonAscii를 `to_lowercase` 같음으로,
  SQL Server collation으로 대소문자 판정, DELETE 별칭 미해석, 표시에 `str::to_lowercase`, 규칙과 무관하게 접기, 미증명 문구에 "names",
  바이트 규칙이 ASCII 밖 차이를 증명 못 함, ASCII 접기에 ASCII 검사 없음, 조인 별칭 미해석, 접는 규칙이 NonAscii를 CTE로, 미증명 쓰기
  삭제)을 각각 잡는다.
- 행렬: 리뷰어 행렬에 이름 쌍 4개(시그마 `ασ`/`ΑΣ`, 전각, 악센트, `日本`/`audit`)와 `ΑΣ` 테이블을 더하고 기대 규칙을 바꿨다(SQL Server
  대소문자 규칙 늘 모름, `NonAscii` 범주, 별칭 DELETE를 정보가 아니라 판정 대상으로). 12,690칸을 새 바이너리로 두 번씩 돌려 오류 0,
  비결정 0, 위반 0이고, 정보 범주는 DB2 파싱 실패 270칸뿐이다(리뷰어 원래 행렬에서도 같은 비율 150칸, 기존). `8fb92cc` 바이너리는 같은 행렬과 같은 기대 규칙에서
  거짓 `complete` 3,672칸·추측한 컬럼 효과 2,776칸·진단 누락 6,191칸·지워진 쓰기 140칸(SQL Server `_CI`의 대소문자만 다른
  대상)이었다. 거짓 `complete`는 시그마·전각·악센트가 각 857칸, `Äudit`/`äudit` 122칸, SQL Server `_CS`·`_BIN2`의 대소문자만
  다른 대상 122칸이고, 나머지 857칸은 새 보수 판정이 더한 `日本`/`audit`이다. 도구는
  리뷰어 도구의 사본이다(세션 scratchpad, 저장소 밖). 간선 판정 함수를 고친 뒤에는 저장된 간선으로 다시 셌다.
- 범위 메모: `ασ`가 아닌 전혀 다른 ASCII 밖 이름(`WITH 日本 AS (…) UPDATE audit`)도 접는·모르는 규칙에서는 `may name`·`partial`이다 —
  collation 표 없이 다르다고 증명할 수 없어서다(의도한 보수 판정, MySQL 0·PostgreSQL·SQLite는 영향 없음).

### 라운드 3 검증 (리뷰 반영 뒤 최종 바이너리)

- Rust 1.96.0: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, 워크스페이스
  테스트 725개 통과(이전 707개 + parser 15 + source 3; 4차 리뷰 반영 뒤 재실행). Go: `gofmt -l`, `go vet`, `go test ./...`
  (Go 코드는 3차 이후 바뀌지 않았다).
- 4차 리뷰 반영 뒤 다시 돌린 것: CI 측정 불변식·시작 실패 경로 Python 테스트 11개와 `test-dml-producers.py`,
  `test-capture-migration.py`, `verify-contracts.py`(jsonschema 4.23.0·sqlglot 30.18.0), `verify-competitive.py`,
  `verify-cancellation.py`, SQLite 골든 diff 0(Python sqlite3 3.53.1로 `basic.sql` 생성, 이 환경에 `sqlite3` CLI가 없다),
  `verify-document-versions.py`, `verify-sqlite-nullability.py`(네이티브·Go·JDBC). PostgreSQL 실DB 검증은 3차 바이너리로 돌린
  아래 결과이고 4차에서는 다시 돌리지 않았다(PostgreSQL·SQLite는 `Table` 규칙이고 다중 테이블 DELETE 문법이 없어
  4차 변경이 출력을 바꾸지 않는다).
- CI 측정 불변식 단계의 Python 테스트 11개와 `test-dml-producers.py`, `test-capture-migration.py`, `verify-contracts.py`
  (jsonschema 4.23.0·sqlglot 30.18.0), `verify-competitive.py`, `verify-cancellation.py`, SQLite 골든 diff 0과
  `verify-document-versions.py`, `verify-sqlite-nullability.py`(네이티브·Go·JDBC).
- 실제 PostgreSQL 16.15(로컬 바이너리, root로는 initdb가 거부되어 별도 일반 계정과 `/tmp` 작업 디렉터리로 실행하고
  지웠다): `verify-accuracy.py --sqlglot --suite all --strict`(실패 0), `verify-fixtures.sh`의 PG 골든 단계를 같은
  명령으로 재현(골든과 같음), `verify-bridge-facts.py --require-isthmus`(isthmus-cli 0.14.0, SQLite·PostgreSQL 사례 모두
  `ok` — 이전 세션은 PostgreSQL 사례를 못 돌렸다), `verify-catalog-dependencies.py`·`verify-openlineage.py`·
  `verify-schema-lint.py`·`verify-unused.py`·`verify-collection-scope.py`(네이티브·Go).
- DML 정확도(SQLite·PostgreSQL): 고정된 v0.4.3 릴리스 엔진·Go·JDBC로 `verify-dml-producers.py` 기준선을 만들고(sha256
  확인) 이 브랜치로 `verify-dml-replay.py` — 둘 다 `failures: 0`.
- 돌리지 못한 것: Docker DB fixture(MySQL·MariaDB·SQL Server·Oracle·IBM, 데몬 없음)와 그에 딸린 정확도·TLS 검사,
  `verify-review-action.py`(SARIF 스키마 호스트가 프록시 403), `verify-html.py`(headless Chrome이 이 환경에서 시간 초과),
  `verify-external-imports.py`(dbt), JDBC `gradle check`(Kotlin 코드 변경 없음, 기존 jar로 교차 수집기 검사만),
  `cargo package`, GLM 리뷰.

### SQLite 식별자 ASCII 접기 (2026-11-15, `1299676`)

PR #59 리뷰가 남긴 후속 후보를 처리했다. 근거와 결과의 정본은 RELEASE-NOTES `# Unreleased`의 해당 항목이다.

- 근거: SQLite는 식별자를 `sqlite3_stricmp`와 같은 규칙으로 비교한다([c3ref/stricmp.html](https://www.sqlite.org/c3ref/stricmp.html)
  — 'the same definition of "case independence" that SQLite uses internally when comparing identifiers').
  그 비교는 US-ASCII 글자만 접는다(SQLite 소스의 `sqlite3UpperToLower` 주석). 이 세션은 sqlite.org에 닿지 못해
  (프록시 403·DNS 실패) 페이지 원문을 그 페이지를 생성하는 공식 헤더(`sqlite3.h` 3.45.1, CAPI3REF "String
  Comparison")와 amalgamation 3.46.0 소스로 확인했고, Python sqlite3 3.53.1로 실제 동작을 확인했다
  (`ÉTUDIANTS`만 있으면 `étudiants`는 `no such table`, 켈빈 기호 테이블과 `k`는 다른 이름, `Prénom` 컬럼에
  `PRÉNOM`은 `no such column`, `ÉTUDIANTS`와 `étudiants`는 한 스키마에 함께 만들 수 있다).
- parser: `scope.rs`의 `catalog_key`·`name()`·`column_key`가 SQLite에서 `to_ascii_lowercase`로 접는다. 관계·컬럼·
  별칭·CTE 이름과 몸체 캐시 지문(같은 키의 `tables` 색인)이 한 규칙을 쓴다. 객체 단위(`vertex_hit`)는
  이미 ASCII였다. 트리거·DML 쿼리 파일의 객체 단위 CTE 비교는 리뷰 반영에서 맞췄다(아래 절). routine 호출은
  접힌 이름과 카탈로그 철자가 정확히 같을 때만 잇는다(후속 후보). MySQL/MariaDB 컬럼 이름과 다른 방언은 바꾸지 않았다. 수집기 3종은 이미 ASCII다.
- 회귀 테스트(수정 전 7개 모두 실패 확인): 리뷰 사례 3종(`ÉTUDIANTS`/`étudiants`, 켈빈 기호, 트리거의 객체·컬럼
  단위 불일치), 두 변형 공존 시 정확한 테이블로 해석, 컬럼·routine 이름, ASCII 변형은 계속 일치, 캐시 cold/warm
  일치와 지문(무관한 `ÉTUDIANTS` 변화는 지문 불변, 진짜 후보 `étudiants` 생성은 지문 변화).
  `catalog_key`·`name()`·`column_key`를 하나씩 되돌린 부분 변이도 각각 테스트가 잡는다.
- 실제 SQLite 대조: 엔진 간선을 SQLite authorizer가 보고한 테이블·컬럼 접근과 몸체 11개(뷰·트리거·`--sql-dir`)에서
  비교했다. 수정 전 9개가 어긋났고(거짓 reads 4건: 앱 SQL `étudiants`·`k`, 트리거 컬럼 단위 read·write / 두 변형
  공존 시 뷰 5개 간선 누락) 수정 후 11개 모두 일치한다. CLI `--cache-dir`의 cold/warm 출력도 카탈로그 변화 3단계에서
  바이트 단위로 같았다. SQLite 골든(`Fixtures/sqlite`)은 식별자가 ASCII라 바뀌지 않았다(Python sqlite3로 재생성해 diff 0).
- 좁힘: 거짓 간선 제거는 SQLite 문서 의미론에 근거한 좁힘이다. `impact`가 줄고, `dead`(뷰·routine 대상)는 그런
  간선으로만 의존받던 뷰를 새로 보고할 수 있다. 반대로 두 변형 공존 사례는 간선이 늘어 보고가 줄 수 있다.
  SQLite 카탈로그에는 사용 통계가 없어 `unused`는 영향이 없다.

### 컴파일되지 않는 SQLite 뷰 (2026-11-15, `dfdeecd`)

- 문제: SQLite는 없는 테이블·컬럼을 읽거나 앱이 등록하는 함수를 부르는 뷰의 생성을 허용하지만 그 뷰를
  컴파일하지 못한다. 네이티브 reader는 그 뷰의 `PRAGMA table_xinfo` 오류를 그대로 올려 scan 전체가 종료 코드 2로
  끝났다(ASCII 접기 작업의 실제 SQLite 대조 중 발견).
- 수정: 뷰의 table_xinfo가 SQLITE_ERROR 계열(주 결과 코드 1과 그 확장 코드, 예: 없는 collation의 257)로 실패할 때만
  뷰와 원문을 두고 컬럼을 비운다. BUSY·LOCKED·IOERR·CORRUPT·NOMEM·CANTOPEN·NOTADB 등 다른 SQLite 오류와 테이블·연결
  오류는 그대로 scan을 끝낸다(종료 코드 2). 처음 커밋(`dfdeecd`)은 모든 `sqlx::Error::Database`를 삼켜 잠금 경합에서
  멀쩡한 뷰의 컬럼을 비웠다 — 리뷰 r2 blocker로 좁혔다(아래 절). 스키마마다 한계 하나가 뷰 수와
  `이름 (SQLite 이유)`를 최대 10개 적는다(제어문자 이스케이프, 조각마다 120자 절단). 이 한계는 카탈로그를 불완전으로
  만들어 facts에 `catalog-coverage:`가 붙고, isthmus 0.14.0은 그 뷰의 컬럼 사용을
  `column-use-without-decl-unverified` 경고로 낸다. 부작용으로 그 DB의 다른 미선언 관계 사용도 모두
  `relation-use-without-decl-unverified` 경고로 낮아진다(둘 다 isthmus 0.14.0으로 확인).
- 검증: 회귀 테스트(`uncompilable_views_are_kept_without_columns_and_counted`, 수정 전 `no such function: app_fn`로
  read 실패 확인), 문구 상한 테스트, CLI scan 종료 코드 0과 graph limitations 노출, 실제 SQLite 대조에 깨진 뷰 사례
  3개 추가(14개 몸체 모두 일치).
- 다른 수집기: Go 프로브(`bestEffort`)와 JDBC 프로브(`getColumns failed`)는 같은 DB에서 이미 뷰를 남기고 카탈로그를
  불완전으로 표시했다(이 세션에서 Go 프로브를 빌드하고 JDBC jar을 Gradle `--offline`으로 다시 만들어 실행해 확인).
  문구 차이는 SUPPORT-MATRIX 수집기 차이 표에 적었다. Go 문구는 뷰 이름을 적지 않고 "source text was not collected"라
  사실과 어긋나지만 실패가 아니라 이번 범위에 넣지 않았다.

### PR #61 독립 리뷰(r2) 반영 (2026-11-15)

리뷰 원문은 모델 출력이라 주장마다 실제 SQLite·CLI·테스트로 재현한 뒤 반영했다. 회귀 테스트는 모두 수정 전에
실패하는 것을 확인했다. 커밋: `09230ac`(분류), `5463012`(문구 이스케이프), `25418a7`(컬럼 없는 객체),
`3240f30`(전이 테이블), `453cb8b`(CTE), `6bd2cef`(RELEASE-NOTES·CATALOG.md), 그리고 이 HANDOFF.

- blocker 1(분류): `sqlite.rs` `uncompilable_view_reason`·`is_sqlite_error_code`가 뷰 + SQLITE_ERROR 계열만 컴파일
  불가로 본다. 테스트: 코드 분류(1·257 참, 5·6·7·10·11·14·26과 확장 코드 261·262·266·267 거짓), 실제 SQLite 오류로
  뷰/테이블/sqlx 오류 경계, 없는 collation 뷰(257)가 같은 문구로 남음, 그리고 `a_locked_view_read_still_ends_the_scan`
  — 다른 연결이 그 뷰의 table_xinfo 직전에만 EXCLUSIVE 잠금을 쥐는 결정적 통합 테스트(풀 `before_acquire` 훅,
  busy_timeout 0). 분류를 되돌리면 이 테스트가 실패한다. 리뷰의 CLI 재현(뷰 4000개, 6.5초 잠금)은 수정 후 2/2
  종료 코드 2(`code: 5`)다. 기존 컴파일 불가 테스트 문구는 그대로 통과한다.
- should-fix 2: `scope.rs` `columns_unknown` — SQLite 객체의 컬럼이 0개면 Binder가 알 수 없는 관계로 본다(SQLite
  테이블·뷰는 컬럼 0개가 불가능). 깨진 뷰를 읽는 `SELECT *`는 `partial` + `SG_WILDCARD_UNKNOWN`이 된다. PostgreSQL
  0컬럼 테이블은 그대로 완전 분석인지 같은 입력으로 함께 검사한다.
- should-fix 3·nit 3: 트리거 전이 테이블(NEW/OLD)을 `catalog_key`로 찾는다. `"Pedidos"` 트리거의 거짓
  `relation 's.Pedidos' is missing`과 빠진 `derives-from`이 고쳐진다. SQLite NEW/OLD 이름 정규화도 ASCII로 맞췄다.
- should-fix 4: `lib.rs` `CollectRules`에 `sqlite`(방언 판정)를 두고 `body_relations::collect_reads`가 CTE 이름을
  ASCII로 접어 비교한다. 리뷰가 제안한 trigger 전용 플래그(`sqlite_trigger`)로는 DML 쿼리 파일이 고쳐지지 않는다 —
  수정 전 테스트에서 쿼리 파일도 `Reads s.recent` 거짓 간선을 냈다. 실제 SQLite에서 그 트리거는 CTE 값(7)을 넣는다.
- nit 1: 한계 문구의 뷰 이름·SQLite 메시지를 `bridge_facts::printable`(재사용, `pub(crate)`로 연다)로 이스케이프하고
  조각마다 120자에서 자른다.
- nit 2·nit 4·문서: RELEASE-NOTES(컴파일 불가 뷰 항목, PR #59 대소문자 항목, CTE·전이 테이블 항목), CATALOG.md
  (SQLite·SQL Server 문단). nit 2의 두 주장(CI collation 다중 후보에서도 "a case-insensitive collation is not assumed",
  lctn=1에서 SELECT 쿼리 파일은 정확 비교·DML 쿼리 파일만 객체 단위로 접힘)은 SQL Server·MySQL 문서로 확인했다.
  CATALOG의 "routine 이름도 접힌다"는 과장이라 좁혔다(아래 후속 후보).
- 골든·캐시: `Fixtures/sqlite/basic.sql`을 Python sqlite3로 다시 만들어 scan한 결과가 골든과 diff 0. CLI
  `--cache-dir` cold/warm 출력이 캐시 없는 scan과 카탈로그 4단계(깨진 뷰가 컴파일됐다가 다시 깨짐 포함)에서
  바이트 단위로 같다.
- 검증: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, 워크스페이스
  테스트 707개 통과(parser 216+20+9+7, source 122+7+6+7, cli 통합 78 + bin 70, core 91, analysis 29, export 45 —
  디스크 때문에 cli 통합 테스트는 바이너리 하나씩 돌리고 지웠다), `verify-document-versions.py`(SQLite 문서),
  `test-capture-migration.py`(7개), `verify-sqlite-nullability.py --engine`, `verify-bridge-facts.py`의 SQLite 두
  사례(isthmus-cli 0.14.0, 계약 자기 비교 `no-changes-observed`). PostgreSQL 사례·Docker fixture·프로브 테스트·GLM
  리뷰는 이번에도 돌리지 못했다.

### CATALOG.md SQL Server 문단 (2026-11-15, `bb4c916`)

- "documents do not carry it" 문구를 PR #59 이후 동작으로 고치고 context 필드 목록에 `lower_case_table_names`·
  `collation`을 더했다. 문구는 `--document`로 만든 SQL Server(CI collation)·MySQL(lctn=1) 문서를 scan해 확인한
  실제 동작을 적는다(아래 후속 후보 참고).

### PR #59 리뷰에서 처리한 것

GLM 리뷰(`packet-ask`)를 이 세션에서 쓸 수 없어, 같은 체크리스트로 독립된 Claude 리뷰를 대신 돌렸다.
PR #59는 GLM 리뷰 없이 머지됐다.

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

## 다음 할 일 (2026-11-22)

1. 다음 릴리스는 **0.10.0**이다 — `CollectionContext`에 공개 필드가 늘었다. [RELEASING.md](RELEASING.md) 절차로
   GitHub Release·crates.io·Maven Central을 **각각** 게시하고, 채널 표를 실제 조회 결과로 갱신한다.
   crates.io 누락분(0.7.1·0.8.0·0.8.1)은 다시 게시하지 않고 0.10.0이 그 변경을 담는다.
2. isthmus 쪽 `compatibility.json`의 schemagraph 고정을 올리는 일은 isthmus 저장소에 맡긴다.
3. 라운드 3 브랜치: push·PR 뒤 CI를 확인하고 머지한다. 이 세션에서 돌리지 못한 검증은 PR 본문의 Verification 절에
   적었다(Docker DB fixture·DML 정확도·JDBC JUnit·GLM 리뷰 등).
4. 후속 후보(관찰했지만 고치지 않음):
   - 대소문자 접기 방언(MySQL `lower_case_table_names` 1·2, SQL Server `_CI` collation)은 routine·trigger 몸체의
     객체 단위 간선만 대소문자만 다른 관계로 잇는다. 뷰 몸체와 컬럼 단위 간선(Binder)은 정확 비교라 같은 참조가
     뷰에서는 간선 없이, routine에서는 테이블 간선만 있고 컬럼 간선 없이 남는다. SQL Server 미해석 메시지는 CI
     collation이 있어도 "a case-insensitive collation is not assumed"라고 쓴다. RELEASE-NOTES `# Unreleased`의
     PR #59 항목은 과장("relation names in bodies match …")을 고쳐 지금 범위를 적었다. 엔진 수정은 별도 PR로 한다
     (지문이 미묘해 지난 라운드 blocker가 났다). 고치려면 Binder(`scope.rs`의
     `CatalogIndex::tables`·`relation_key`)가 같은 단일 후보 규칙을 따르게 하고 지문을 함께 확인해야 한다.
     라운드 3에서도 지문 검증까지 한 번에 끝낼 범위가 아니라 손대지 않았다.
   - SQLite routine 호출의 마지막 정점 조회는 정확 비교(`lookup(false)`)라, 호출 이름을 ASCII 소문자로 접은 철자가
     카탈로그 이름과 같을 때만 잇는다. `ECHO` routine을 `echo(...)`로, `ÉCHO`를 `Écho(...)`로 부르면 잇지 못한다
     (`ECHO(...)`로 `echo`는 잇지만, ASCII 대문자가 든 이름의 routine `ECHO`는 `ECHO(...)`로도 잇지 못한다 — r3 재검증에서 확인). 수집기는 SQLite routine을 내지 않고(경로 이름의 query routine뿐) 실사용 영향은
     없다고 봤다. CATALOG.md·RELEASE-NOTES는 이 범위로 적었다.
   - SQL Server에서 DML 대상이 그 문장의 CTE 이름이면(`WITH c AS (SELECT … FROM t) UPDATE c …`, 중복 삭제 관용구) 이제
     `SG_DML_TARGET_SHAPE`로 부분 분석이고 실제로 쓰이는 원본 테이블 `t`에는 간선이 없다. CTE가 단일 테이블을 읽는
     갱신 가능한 모양일 때 그 테이블로 쓰기를 옮길 수 있지만, SQL Server의 갱신 가능 CTE 규칙을 실서버로 확인해야 한다.
     MySQL·DB2 등은 이 모양의 의미를 확인하지 않아 같은 보수 규칙을 쓴다.
   - DELETE의 WHERE·USING·조인 컬럼 읽기는 DELETE 앞에 WITH가 있든 없든 남기지 않는다(`DmlAnalyzer`가 DELETE를 해석하지
     않는다, main부터). UPDATE는 WHERE 컬럼을 읽기로 남기므로 비대칭이다. 넣으려면 DELETE의 대상·USING·조인·WHERE를 Binder로
     해석하고, DELETE가 든 SQL Server·Oracle fixture 골든(`Fixtures/mssql`·`Fixtures/oracle`, Docker에서만 생성)을 실DB로
     다시 확인해야 한다.
   - SQL Server에서 CTE 이름을 어느 collation이 비교하는지(데이터베이스·인스턴스·포함 DB 카탈로그) 문서가 없어 대소문자만 다른
     대상은 판정하지 않는다(4차). 실서버에서 데이터베이스·인스턴스 collation을 엇갈리게(CI/CS) 두고 `WITH Audit … UPDATE audit`가
     CTE를 통해 쓰는지 확인하면, 그리고 프로브가 인스턴스 collation(`SERVERPROPERTY('Collation')`)도 수집하면 판정을 넓힐 수 있다.
     CTE 읽기는 대소문자 규칙을 따르지 않는다 — `_CI`에서 `WITH Recent AS (…) … FROM recent`가 같은 이름의 실제 테이블
     `recent`를 읽는 간선을 낸다(`body_relations::collect_reads`와 Binder `factor`가 정확 비교, main부터; 3차 리뷰가 짚고 main
     바이너리로 확인). 대소문자 접기 방언 항목과 함께 고친다.
   - (main부터, 3차 리뷰가 짚고 main 바이너리로 확인) T-SQL `BEGIN TRY … END TRY BEGIN CATCH … END CATCH`를 세미콜론 없이 쓰면
     CATCH 안의 문장(`UPDATE log …`)이 빠진 채 `complete`다. MySQL 다중 테이블 `UPDATE a JOIN b … SET b.x = …`는 `b`를 쓰기가
     아니라 읽기로 남기고(`SG_DML_TARGET_SHAPE`로 부분 분석), 쉼표 형식 `UPDATE a, b SET …`은 파싱하지 못한다(`SG_BODY_PARTIAL`).
   - 데이터 수정 CTE(PostgreSQL `WITH ins AS (INSERT INTO log … RETURNING …) SELECT … FROM ins`): 쿼리 파일은 `is_select`가
     참이라 읽기 경로로 가서 `writes log`가 없고 `SG_SET_UNSUPPORTED`로 부분 분석이다. routine은 객체 단위 `writes`·`reads`만
     남기고 컬럼 효과 없이 `complete`다(라운드 3 CLI로 확인). 고치려면 CTE 몸체의 DML을 `DmlAnalyzer`가 분석하고
     RETURNING 출력을 바깥 질의의 원천으로 이어야 한다. 쿼리 파일만 DML 경로로 보내면 같은 거짓 complete가 되므로
     둘을 함께 고친다.
   - (main부터, 4차 작업 중 확인) PostgreSQL 미인용 식별자 접기(`scope.rs` `name()`·`name_for_dialect`)가 `to_lowercase`라 ASCII 밖
     글자도 접는다. PostgreSQL은 다중 바이트 인코딩(UTF8)에서 ASCII만 접는다(REL_16_STABLE `src/backend/parser/scansup.c`
     `downcase_identifier`: 높은 비트 글자는 `enc_is_single_byte`일 때만 `tolower`). main 바이너리에서 PostgreSQL 프로시저
     `UPDATE ΑΣ SET name = 'x'`가 테이블 `ΑΣ`를 두고 `s.ας`가 없는 관계라는 한계를 낸다. 같은 이름의 `ας` 테이블이 있으면 거짓 간선이 된다.
   - 다중 테이블 DELETE의 MySQL `DELETE FROM c USING audit c …` 형식은 대상(`FROM` 목록)의 별칭을 `USING` 관계로 풀지 않는다(main부터).
     PostgreSQL의 `DELETE FROM t USING …`과 같은 AST라 방언을 보고 갈라야 해 4차 범위에서 뺐다.
   - `--sql-dir` 쿼리 파일의 컬럼 분석 한계 문구는 `view main.query_…: SG_…`처럼 객체 종류를 `view`로 적는다
     (`record_body`/`enrich_view` 경로의 문구). 의미는 맞지만 쿼리 파일을 뷰로 부른다.

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
- 기능마다 `/code-review high` → 지적 검증 → 반영/보류 근거를 PR 코멘트로 남김 → CI → 머지.
  지금은 squash merge다: PR #53–#59는 `main`의 first-parent 이력에 PR당 단일 부모 커밋 하나로 들어갔다
  (#51·#52와 그 전 PR 다수는 merge commit이었다). `git log --first-parent origin/main`으로 확인한다.
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
4. 릴리스 절차: 버전·RELEASE-NOTES PR → CI → squash 머지 → 병합 트리 = PR 헤드 확인 → 병합 커밋 main CI →
   lightweight 태그 push(GitHub Release, 본문을 RELEASE-NOTES 항목으로 교체) → `cargo publish --workspace --locked`
   (별도 target) → `verify-crates.py` → registry 설치 + `verify-installed.py` → Pages → Central → repo1 조회 →
   `verify-public.py`(openjdk@17을 PATH에) → 근거 정리 → HANDOFF 완료 PR.

## 재개 프롬프트

```text
schemagraph 저장소 루트에서 HANDOFF.md와 AGENTS.md를 읽어줘.
발행본은 0.9.0이고, RELEASE-NOTES의 Unreleased 변경은 아직 발행 전이야(다음 crate 릴리스는 0.10.0).
먼저 git 상태를 확인하고 로컬 변경을 보존한 뒤, 내가 추가로 요청하는 작업부터 이어가줘. 배포는 반복하지 마.
```
