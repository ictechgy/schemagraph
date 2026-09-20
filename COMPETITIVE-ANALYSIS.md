# 경쟁 도구 조사와 개발 우선순위

조사일: 2026-09-20. schemagraph 기준: v0.3.0 및 커밋
`2ddadf6a27ce6dd3bc9209a53011902f37e920d3`.

**다음 개발의 중심은 컬럼 분석 정확도, 분석 근거 설명, 변경 검토 자동화가 적절하다.**
DB 종류와 다이어그램 형식의 추가만으로는 차별화하기 어렵다. 현재의 순수 그래프
엔진·분리된 프로브·오프라인 분석 구조를 유지하면서, 개발자와 코딩 에이전트가
변경의 영향을 검토하는 데 필요한 증거를 더 정확하게 제공하는 방향을 권한다.

이 문서는 공식 문서·공개 소스와 로컬 구현을 대조한 조사다. 경쟁 제품을 같은
환경에서 실행한 정확도·속도 벤치마크는 아니다. 아래 개발 순서와 작업 크기는
분석자의 제안이며, 확정 일정이나 이미 구현된 기능을 뜻하지 않는다. DB 권한,
방언, 제품 에디션에 따라 경쟁 도구의 실제 수집 범위도 달라진다.

**우선 경쟁 구도에 대한 설명을 고쳐야 한다.**

[DESIGN.md](DESIGN.md#L13)는 기존 도구가 문서·다이어그램에서 멈추고 SQL 몸체
의존성 분석을 하지 않는다고 일반화한다. 현재 공식 자료와 맞지 않는다.
SchemaCrawler는 view·routine 의존성을 수집하고, 순환·인덱스 관련 lint,
의존성·영향 도달 수와 MCP 연동을 제공한다. 중요도 분석은 2026-09-06의
17.15.0 릴리스에도 명시되어 있어 단순한 미래 계획이 아니다.
출처: [메타데이터 수집](https://www.schemacrawler.com/metadata-retrieval.html),
[그래프 지표](https://www.schemacrawler.com/importance.html),
[릴리스 기록](https://www.schemacrawler.com/changes-report.html).

권장 설명은 “여러 DB의 카탈로그와 SQL 의존성을 재현 가능한 그래프 파일로 만들고,
변경 영향과 분석 한계를 로컬에서 질의하는 개발자·에이전트용 도구”다.
독점적인 기능이라는 주장보다 **어떤 입력을 얼마나 정확하게 처리하고, 못 본
범위를 어떻게 설명하는지**를 검증 자료로 보여주는 편이 설득력 있다.

**비교 대상과 배울 점**

문서화 도구, 변경 관리 도구, SQL 계보 분석기, 메타데이터 플랫폼은 서로 대체
범위가 다르다. 아래 표는 같은 제품군이라고 가정한 순위표가 아니라,
schemagraph의 사용 목적과 겹치는 부분을 비교한 것이다.

| 도구 | 공식 자료에서 확인한 기능 | schemagraph에 주는 시사점 |
| --- | --- | --- |
| SchemaCrawler | view·routine 의존성, [lint](https://www.schemacrawler.com/lint.html), [MCP](https://www.schemacrawler.com/mcpserver.html), 그래프 중요도 | 가장 직접적인 비교 대상. 의존성·JSON·에이전트 연동 자체를 독점적 차별점으로 내세우기 어렵다. 근거와 정확도 비교가 필요하다. |
| [tbls](https://github.com/k1LoW/tbls) | CLI 문서화, 스키마 diff, 규칙 검사, 가상 관계, 관점별 출력, 바이너리·컨테이너 배포 | CI에서 설치하고 결과를 검토하는 경험, 기본 규칙과 부분 그래프 출력을 배울 만하다. |
| Azimutt | [검색](https://azimutt.app/docs/search), [테이블 간 경로 탐색](https://azimutt.app/docs/find-path), [스키마 이상 징후 검사](https://azimutt.app/use-cases/analyze) | 전체 ERD보다 관심 객체를 조금씩 펼치는 탐색, 필터, 경로 설명이 유용하다. 분석 기능도 있으므로 단순 시각화 도구로 분류하면 안 된다. |
| [SchemaSpy](https://github.com/schemaspy/schemaspy) | 탐색 가능한 HTML 문서·ERD, JAR·Docker 배포 | 별도 서버 없이 공유할 수 있는 HTML 보고서와 쉬운 실행 경험을 참고할 수 있다. SQL 의미 분석 정확도를 대신 증명해 주는 비교 대상은 아니다. |
| [DBeaver](https://dbeaver.com/docs/dbeaver/ER-Diagrams/) | 테이블·스키마 다이어그램, 사용자 지정 다이어그램, 레이아웃 저장·내보내기 | 익숙한 탐색·필터·부분 보기의 기준점. 범용 DB IDE 전체를 재현할 필요는 없다. 일부 기능은 PRO로 구분된다. |
| [Redgate SQL Dependency Tracker](https://www.red-gate.com/products/sql-dependency-tracker/) | SQL Server 객체 의존성, 여러 DB를 포함한 탐색, 변경 영향, SQL 원문, 오프라인 프로젝트 | 영향 목록에서 실제 경로와 원문으로 이동하는 흐름이 중요하다. SQL Server 중심 상용 제품이라는 범위 차이를 고려해야 한다. |
| [Atlas](https://atlasgo.io/versioned/lint) | 파괴적·비호환 변경 및 잠금 관련 migration 검사, PR/CI 연계 | `diff` 결과를 실제 변경 검토와 연결해야 한다. 현재 문서상 v0.38부터 `migrate lint`는 Pro 기능이다. [기능 구분](https://atlasgo.io/features)도 함께 확인했다. |
| [SQLGlot](https://sqlglot.com/sqlglot/lineage.html) | SQL·스키마·스코프 정보를 이용한 출력 컬럼 계보 API | 객체가 어떤 컬럼을 읽는지와 출력 컬럼이 어느 입력 컬럼에서 유래하는지는 별개다. 후자의 비교 기준으로 적합하다. |
| [SQLLineage](https://sqllineage.readthedocs.io/en/latest/gear_up/metadata.html) | 메타데이터를 이용한 wildcard·비한정 컬럼 해석 | 이미 수집한 컬럼 목록을 SQL 이름 해석에 활용해야 한다. 메타데이터가 없거나 모호한 경우의 한계도 명시한다. |
| [DataHub](https://github.com/datahub-project/datahub/blob/master/docs/lineage/sql_parsing.md) | SQLGlot 기반 계보 분석, CTE·서브쿼리·스키마 기반 `*` 확장, query log와 dbt 등 연동 | DB 내부 객체 밖의 쿼리·작업까지 연결하는 방향을 참고할 수 있다. 다만 자체 문서도 UDF·일부 SQL scripting 등 한계를 명시한다. vendor 정확도 수치는 이번 비교의 실측값으로 사용하지 않았다. |

**현재 구현에서 확인한 강점과 공백**

유지할 기반은 명확하다. native/JDBC/Go의 공통 document, 분리된 Rust 의미 분석,
결정적 출력, `limitations`와 `truncated`, 관측된 0과 미수집의 구분, v1/v2 호환,
실DB fixture가 이미 있다. `diff`, `rules --strict`, `document-capabilities`도
구현되어 있으므로 이를 없는 기능처럼 다시 계획해서는 안 된다.
[README](README.md), [CATALOG](CATALOG.md), [검증 기록](HANDOFF.md).

아래는 정적 코드 확인 결과다. 새로운 입력으로 실행해 재현한 버그 목록과는
구분하며, 변경 시에는 명시한 완료 조건으로 검증해야 한다.

| 확인 사항 | 코드 근거 | 실제 의미 |
| --- | --- | --- |
| 컬럼 해석은 최상위 SELECT 별칭과 두 부분 식별자 중심 | [`collect_query`·`collect_expr_columns`](engine/parser/src/lib.rs#L407) | 비한정 컬럼과 `*`를 스키마로 풀지 않는다. CTE·서브쿼리는 부분 해석을 알리지만 완전한 스코프 기반 컬럼 분석은 아니다. |
| view의 컬럼 간선은 view 객체 → 입력 컬럼 | [`apply_view`](engine/parser/src/lib.rs#L554) | `v.output_column → source.input_column` 형태의 출력 컬럼 계보와 구별해야 한다. |
| evidence는 계층과 설명 문자열, limitations는 문자열 배열 | [`Evidence`·`Graph`](engine/core/src/lib.rs#L163) | 오류 코드, 객체별 분석 상태, SQL 위치, 재현 가능한 근거 식별자를 프로그램이 직접 활용하기 어렵다. |
| 영향 결과는 정점·거리·간선 종류 목록 | [`ImpactReport`](engine/analysis/src/lib.rs#L114) | 어떤 중간 객체를 거쳐 영향이 전파됐는지 설명하는 경로가 결과에 없다. |
| 설계의 retain 루트가 현재 dead API·CLI에 없음 | [설계의 루트](DESIGN.md#L106), [`dead`](engine/analysis/src/lib.rs#L184), [CLI](engine/cli/src/main.rs#L82) | 현재는 DB 내부 의존자 부재 기반 후보 계산이다. 앱에서 직접 쓰는 객체를 설정으로 보존하는 기능이 필요하다. |
| graph diff와 document diff의 정보량이 다름 | [`diff_graph`](engine/analysis/src/lib.rs#L444), [`diff_documents`](engine/source/src/diff.rs#L193) | 타입·nullable·default 등의 변경 검토에는 catalog document를 사용해야 한다. graph diff만으로 모든 스키마 변경을 감지한다고 설명하면 안 된다. |
| 인덱스 모델은 이름·unique·컬럼 목록·usage 중심 | [`IndexDoc`](engine/source/src/document.rs#L90) | partial predicate, expression, include, 정렬 방향까지 고려한 인덱스 중복 판단에는 메타데이터 확장이 필요하다. |
| BFS의 max는 탐색 후 결과를 자르는 방식 | [`bfs`](engine/analysis/src/lib.rs#L246) | `--max`가 작아도 탐색 비용이 그 수로 제한되지는 않는다. 출력 제한과 실행 예산을 별도로 설계해야 한다. |
| CLI 출력은 JSON·Mermaid·DOT, 서버 명령은 없음 | [명령·형식 정의](engine/cli/src/main.rs#L30) | 현재 `skill` 문서 제공과 실제 MCP 서버 제공은 다르다. HTML 탐색기도 별도 기능이다. |
| DB·catalog를 포함하는 식별 범위가 제한적 | [document 타입](engine/source/src/document.rs), [`relation_name_parts`](engine/parser/src/lib.rs#L376) | 세 부분 이상의 테이블 이름은 임의로 연결하지 않고 한계로 남긴다. 여러 DB를 합치려면 식별자 계약부터 정해야 한다. |

**개발 우선순위 제안**

P0는 분석 신뢰성과 설명의 정확성, P1은 실제 사용·CI 도입, P2는 수요와 측정에
따라 확장할 항목이다. S/M/L은 상대적인 변경 범위이며 소요 기간 예측이 아니다.

| 순서 | 우선순위 | 보강 내용 | 범위 | 완료 기준 |
| --- | --- | --- | --- | --- |
| 1 | P0 | 경쟁 설명과 설계/구현 상태 정리 | S | 위 경쟁 일반화를 제거하고, retain·컬럼 계보 등 계획과 구현을 구분한다. |
| 2 | P0 | 스키마·스코프 기반 컬럼 해석 | L | 비한정 컬럼, wildcard, CTE, 파생 테이블, 상관 서브쿼리, UNION의 필요한 간선과 금지된 간선을 검증한다. 모호하면 간선을 확정하지 않는다. |
| 3 | P0 | 구조화된 분석 상태·근거 | M | 객체별 complete/partial/unsupported, 진단 코드, 이유·집계·가능한 원문 위치를 제공한다. 기존 소비자 호환을 검증한다. |
| 4 | P0 | `dead` 보존 루트와 명시적 예외 | M | 앱 진입점으로 선언한 view/routine과 그 의존 대상을 보존한다. usage 부재·0·양수, 루트 미지정, 예외의 영향이 구분된다. |
| 5 | P1 | `explain`·`path`와 영향 경로 | M | A→B→C의 중간 객체·간선 근거가 나오고 다중 경로·순환·동일 길이 경로에서도 결정적 결과와 절단 표시가 유지된다. |
| 6 | P1 | catalog diff와 impact를 합친 변경 검토 | M–L | 삭제·타입·nullable·의존성 변화별로 잠재 영향과 근거를 보고하고, 추가만 있는 변경과 분석 불완전을 구분한다. |
| 7 | P1 | 배포 바이너리와 CI 도입 예제 | M | 우선 지원 대상으로 정한 OS/아키텍처에서 Rust 없이 설치·실행한다. 체크섬, standalone JDBC JAR, 고정 버전 CI 예제로 검증한다. |
| 8 | P1 | DB가 제공하는 의존성 증거 병합 | M–L | DB 카탈로그 결과와 파서 결과를 별도 증거로 유지한다. 권한 부족·불일치·중복·미해석 대상에 대한 동작을 실DB로 확인한다. |
| 9 | P1 | 소수의 유용한 스키마 규칙 | M | FK 인덱스 누락, 미해결 객체, 계층 경계 위반부터 시작한다. 메타데이터가 부족한 복잡한 인덱스를 오판하지 않는다. |
| 10 | P2 | SQL 파일·외부 사용처 입력 | M–L | 파일 경로·쿼리 식별자로 DB 객체의 사용처를 연결한다. 미관측 앱 사용을 미사용으로 취급하지 않는다. |
| 11 | P2 | JSON Schema와 읽기 전용 MCP | M | 고정 스냅샷의 query/impact/explain 결과가 CLI와 일치하고, 잘못된 입력·잘린 결과·허용하지 않은 경로를 명확하게 보고한다. |
| 12 | P2 | 오프라인 HTML 탐색·중요도 | M | 검색, 이웃 펼치기, 영향 경로, 원문 근거를 제공한다. 대형 그래프에서 처음부터 전부 렌더링하지 않는다. |
| 13 | P2 | 실행 예산·증분 분석·대형 그래프 | L | 출력 수와 별개인 탐색 한도·취소를 제공한다. 캐시 적중 시에도 전체 재분석과 결과가 같으며 단일 대형 스키마로 측정한다. |
| 14 | P2 | 여러 DB 식별자와 신규 데이터웨어하우스 | L | 동명 객체를 혼동하지 않고 기존 ID의 호환 경로를 제공한다. 신규 DB는 실제 수요와 실DB 완료 조건이 있을 때 추가한다. |

**가장 먼저 구현할 정확도 개선의 범위**

첫 단계는 이미 수집된 컬럼 목록을 이용해 한 스코프 안의 이름을 정확하게
해석하는 것이다. 예를 들어 `SELECT id FROM orders`는 소스가 명확하면 해석할
수 있다. 반면 `orders`와 `customers`가 모두 `id`를 갖는 조인에서 비한정 `id`는
임의 귀속하면 안 된다. 그다음 CTE·서브쿼리의 이름 공간과 wildcard 확장을 넣고,
마지막으로 출력 컬럼까지 연결하는 계보 모델을 추가하는 순서가 적절하다.
[SQLLineage의 메타데이터 설명](https://sqllineage.readthedocs.io/en/latest/gear_up/metadata.html)과
[SQLGlot의 lineage API](https://sqlglot.com/sqlglot/lineage.html)가 구체적인 비교 기준이다.

값의 유래와 필터·조인 의존성도 구분해야 한다.

```sql
CREATE VIEW v AS
SELECT amount AS total
FROM orders
WHERE deleted_at IS NULL;
```

`v.total` 값의 유래는 `orders.amount`다. 동시에 `orders.deleted_at`의 변경도
뷰의 실행과 결과 집합에 영향을 줄 수 있다. 후자를 버리면 변경 영향 분석이
약해지고, 둘을 같은 종류의 값 계보로 섞으면 설명이 부정확해진다.
DataHub도 필터·정렬 절의 컬럼을 계보에서 제외한다고 명시한다. 따라서 경쟁
분석기의 계보 결과를 그대로 전체 의존성의 정답으로 사용해서는 안 된다.
[DataHub의 계보 범위와 한계](https://github.com/datahub-project/datahub/blob/master/docs/lineage/sql_parsing.md).

새 해석기는 Rust `parser` 안에 두고, 프로브는 원문과 카탈로그 전달을 유지한다.
SQLGlot을 런타임 의존성으로 바로 붙이는 것보다, 지원 범위가 겹치는 사례의
차등 검증 도구로 활용하는 편이 현재 구조에 맞는다. 정답은 경쟁 도구의 출력
하나로 정하지 않고 SQL 의미·실DB 카탈로그·수동 fixture 기대값을 대조한다.

**설명 가능한 분석과 변경 검토**

근거는 단순한 확신 점수보다 `source object`, `body hash`, `statement index`,
가능한 `span`, 해석 상태와 진단 코드가 유용하다. 절차형 SQL을 잘라내거나 동적
SQL을 복원한 경우에는 원문 위치를 유지하는 매핑이 필요하다. 위치를 복구하지
못한 근거에 가짜 줄 번호를 붙이지 않는다.

`explain`은 기존 간선의 evidence를 펼쳐 보여주고, `path`는 중간 객체를 포함한
의존 경로를 제공하는 그래프 질의로 구현할 수 있다. 중요도는 우선 피참조 수·
영향 도달 수처럼 설명 가능한 지표를 사용한다. 복합 점수를 넣더라도 장애
확률이나 삭제 안전도를 뜻한다고 해석하면 안 된다.
[Azimutt 경로 탐색](https://azimutt.app/docs/find-path),
[SchemaCrawler 그래프 지표](https://www.schemacrawler.com/importance.html).

변경 검토의 초기 입력은 마이그레이션 SQL 실행 대신 **변경 전후 catalog
document 두 개**가 적절하다. 이미 있는 document diff로 바뀐 객체를 찾고,
변경 전 그래프에서 삭제 대상의 역방향 영향을 계산한 뒤 새 그래프와 비교한다.
삭제된 객체는 새 그래프에 없으므로 새 스냅샷만으로 조사하면 안 된다.

타입 축소 등은 방언별 규칙과 실제 소비 방식에 따라 위험이 다르다. 초기에는
삭제·nullable 변경·의존 간선 변화를 잠재 영향으로 분류하고, 근거와 미확인 범위를
함께 보고한다. Atlas의 잠금·rewrite 분석까지 같은 수준으로 제공한다고
설명하지 않는다. PR에서는 JSON과 Markdown 요약을 제공하고, 소스 위치가
준비된 뒤 SARIF를 검토한다. 기존 마이그레이션 도구가 만든 전후 DB/스냅샷을
소비하면 별도의 마이그레이션 실행 엔진 없이도 통합할 수 있다.
[Atlas의 migration 검사 범위](https://atlasgo.io/versioned/lint).

비교하는 스냅샷의 DB 식별자, 선택한 스키마, 수집 권한과 완료 상태도 필요하다.
권한 부족이나 필터 변경으로 객체가 빠진 상황을 실제 DROP으로 오해하지 않도록
비교 가능성을 먼저 검사한다. 현재 document diff와 graph diff는 이 검토 기능의
재료이며, 그 자체가 완성된 변경 위험 보고서는 아니다.

**DB 카탈로그와 외부 사용처로 근거 확장**

PostgreSQL의 [pg_depend](https://www.postgresql.org/docs/current/catalog-pg-depend.html),
SQL Server의 [sys.sql_expression_dependencies](https://learn.microsoft.com/en-us/sql/relational-databases/system-catalog-views/sys-sql-expression-dependencies-transact-sql?view=sql-server-ver17),
Oracle의 [ALL_DEPENDENCIES](https://docs.oracle.com/en/database/oracle/oracle-database/19/refrn/ALL_DEPENDENCIES.html)는
추가 증거로 검토할 만하다. 이들 정보도 내부 의존성 종류, 객체 가시성, 다른 DB의
식별자 해석 등 제약이 있으므로 SQL 실행 전체의 완전한 정답은 아니다.

프로브는 DB가 제공한 참조 행과 원래 식별자를 전달하고, 의미 해석·ID 정규화·
간선 병합은 엔진에서 수행한다. 타입이 불명확한 카탈로그 의존성을 무조건
`reads`로 바꾸지 않는다. 파서와 불일치하면 한쪽을 삭제하지 말고 양쪽 근거와
한계를 드러내는 방식이 좋다.

외부 사용처는 정적 SQL 파일을 먼저 지원하고, 이어서 dbt manifest/compiled SQL,
관측 query log 같은 생산자를 선택적으로 붙이는 순서를 권한다. DataHub는 이런
입력들을 통해 계보를 확장한다. schemagraph에서는 애플리케이션의 파일·함수와
연결하는 document 확장이 자매 프로젝트와의 결합에도 맞는다.
[DataHub SQL parsing 연동](https://github.com/datahub-project/datahub/blob/master/docs/lineage/sql_parsing.md).

관측 로그에는 수집 기간·샘플링·누락·스키마 컨텍스트가 필요하다. 통계 수집을 위해
프로덕션 DB 설정을 자동 변경하는 기능은 기본 흐름에 넣지 않는다. 앱 사용처의
입력은 `dead`의 오해를 줄이는 데 유용하지만 관측되지 않은 사용까지 없다고
증명하지는 않는다.

**도입 경험과 성능**

현재 crates.io·Maven Central 배포는 완료됐다. 다음 배포 과제는 다시 레지스트리를
추가하는 것이 아니라 CLI 사전 빌드 파일, 실행 JAR을 찾기 쉬운 릴리스 구성,
간단한 CI 예제다. 지원을 표기한 플랫폼마다 설치·SQLite smoke test를 실행하고,
추가로 Homebrew나 컨테이너가 필요한지는 실제 사용자 환경으로 결정한다.
[tbls 설치 경로](https://github.com/k1LoW/tbls),
[SchemaCrawler 배포 경로](https://www.schemacrawler.com/downloads.html).

MCP는 새로운 판정 엔진 대신 고정된 그래프 파일을 읽는 얇은 인터페이스로
시작한다. `query`, `impact`, `explain`의 의미와 결과는 CLI와 같아야 한다.
기계 검증용 JSON Schema, 계약 버전, 응답 크기 한도, 파일 접근 범위를 함께
정한다. 이미 MCP를 제공하는 도구가 있으므로 MCP 추가만으로 분석 품질이
좋아졌다고 주장해서는 안 된다.
[SchemaCrawler MCP](https://www.schemacrawler.com/mcpserver.html).

HTML 탐색기는 JSON 그래프의 소비자로 구현하면 엔진을 복제하지 않아도 된다.
첫 화면은 검색·선택한 객체·제한된 이웃만 표시하고, 근거와 부분 분석 표시를
같이 보여준다. SQL·객체명은 안전하게 이스케이프하고 오프라인 사용을 검증한다.
서버·로그인·조직 관리 기능보다 공유 가능한 한 개의 분석 보고서가 먼저다.
[SchemaSpy HTML 보고서](https://schemaspy.org/),
[Azimutt 탐색](https://azimutt.app/docs/search).

성능은 이미 개선했고 [PERFORMANCE.md](PERFORMANCE.md)에 재현 가능한 수치가 있다.
이는 이전 schemagraph 버전과의 비교이며 경쟁 제품보다 빠르다는 근거가 아니다.
다음 측정은 조회를 반복할 때의 로딩 비용, 하나의 거대한 스키마, 밀집 의존
그래프, 아주 긴 SQL 몸체를 포함해야 한다. 그래프 내부의 문자열·간선 중복
보관과 파싱 캐시를 먼저 측정하고, 디스크 기반 저장소 도입은 그 결과로 결정한다.

현재 `--max`의 출력 의미를 바꾸지 않고 별도의 탐색 예산과 중단 이유를 추가하는
편이 안전하다. 증분 파싱 캐시의 키에는 SQL 몸체뿐 아니라 방언·파서 버전·관련
스키마 정보도 필요하다. SQL이 같아도 `SELECT *`의 대상 스키마가 달라지면 결과가
달라질 수 있기 때문이다.

**개발을 시작할 때 사용할 검증 기준**

| 검증 축 | 필요한 사례와 지표 |
| --- | --- |
| 이름·스코프 해석 | qualified/unqualified, 동명 컬럼, 인용·대소문자, CTE 별칭, 상관 서브쿼리, wildcard, UNION, JOIN USING, GROUP/HAVING/ORDER/WINDOW |
| 의존성과 값 계보 | 출력 컬럼 유래, 필터·조인 의존성, INSERT SELECT·CTAS, routine/trigger 호출, 동적 SQL의 확정·미확정 경계 |
| 정확도 | DB/객체 종류별 필요한 간선의 재현율과 출력 간선의 정밀도. 미지원·모호한 항목 수를 별도로 공개하고 전체 평균으로 숨기지 않는다. |
| 변경 검토 | 컬럼 삭제·이름/타입/nullable 변경, 무해한 추가, 스캔 권한 감소, 선택 스키마 변경, 부분 수집. 삭제 대상은 이전 그래프에서 추적한다. |
| 보존 루트 | 앱에서 직접 호출하는 routine/view, 루트의 전이 의존성, 루트 없는 순환, usage 없음·0·양수, 만료되거나 대상이 없어진 예외 |
| 에이전트 계약 | 스키마 검증, CLI/MCP 결과 일치, 결과·탐색 절단 구분, 결정적 경로 선택, unknown feature와 불완전 입력 거부 |
| 운영·성능 | OS별 설치, cold/warm 실행 구분, parse/graph/query 시간을 분리한 측정, peak RSS, 취소·제한 도달 시 불완전 상태 |

같은 입력으로 비교할 수 있는 부분만 경쟁 도구와 대조한다. SQL 계보 엔진,
ERD 문서 생성기, 지속 실행하는 메타데이터 플랫폼의 전체 RSS를 한 숫자로
순위 매기면 작업 범위 차이가 섞인다. 첫 비교는 PostgreSQL·SQLite의 공통
SQL 사례에서 시작하고, Oracle·SQL Server·Db2·Informix는 별도 방언 fixture로
확장하는 것이 현실적이다.

실행 순서는 **문서·구현 정합성 확인 → 컬럼 해석·분석 상태·보존 루트 →
근거 경로와 변경 검토 → 설치·CI → 외부 입력/MCP/UI**를 권한다.
사전 빌드 배포처럼 독립적인 작업은 정확도 개선과 병행할 수 있다.
큰 웹 관리 제품, 자동 삭제 권고, 전체 마이그레이션 실행기, 수요가 확인되지 않은
DB 커넥터의 대량 추가는 현재 단계의 우선 투자 대상으로 보지 않는다.
