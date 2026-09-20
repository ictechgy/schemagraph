# schemagraph

이 문서는 한국어 참고 번역입니다. README의 정본은 [영어 원문](README.md)이며,
내용이 다를 때는 영어 원문을 기준으로 합니다.

schemagraph는 데이터베이스 스키마의 의존성 그래프를 만들어 스키마 변경의
영향을 분석합니다. 선언된 외래 키와 함께 뷰·루틴·트리거 본문에서 추출한
`reads`, `writes`, `calls` 의존성을 수집합니다.

**그래프가 산출물이고, 분석은 그 위의 질의입니다.** 한 번 스캔한 뒤에는 DB에
다시 접속하지 않고 의존성 조회, 영향 추적, 순환 탐지, 아키텍처 규칙 검사,
스냅샷 비교를 수행할 수 있습니다. 스크립트와 코딩 에이전트를 위한 결정적
JSON을 출력하며, Mermaid와 Graphviz DOT 다이어그램도 지원합니다.

## 빠른 시작

Rust와 Cargo로 CLI를 설치합니다.

```sh
cargo install schemagraph-cli --version 0.1.0 --locked
```

### 소스에서 빌드

Rust와 Cargo가 설치된 환경에서 저장소를 복제하고 CLI를 설치합니다.

```sh
git clone https://github.com/ictechgy/schemagraph.git
cd schemagraph
cargo install --path engine/cli --locked
```

### 샘플 DB 실행

`schemagraph`가 `PATH`에 있고 `sqlite3` CLI가 설치되어 있으면 포함된 fixture로
시작할 수 있습니다. 다음 예제는 새 임시 디렉터리에 샘플 DB를 만듭니다.

```sh
sg_demo_dir="$(mktemp -d)"
sqlite3 "$sg_demo_dir/shop.db" < Fixtures/sqlite/basic.sql

schemagraph scan "sqlite:$sg_demo_dir/shop.db" -o "$sg_demo_dir/shop.graph.json"
schemagraph query main.orders --graph "$sg_demo_dir/shop.graph.json"
schemagraph impact main.customers --graph "$sg_demo_dir/shop.graph.json"
schemagraph graph --graph "$sg_demo_dir/shop.graph.json" --format mermaid
```

fixture에는 외래 키 체인, 뷰, 트리거, 의존성 순환이 포함되어 있습니다.
`query main.orders`는 `main.customers`에 대한 의존성을 보여주고,
`impact main.customers`는 이를 사용하는 뷰와 트리거까지 추적합니다.

## 지원 데이터베이스

| 데이터베이스 | 네이티브 reader | JDBC 프로브 | Go 프로브 |
| --- | --- | --- | --- |
| SQLite | 지원 | 드라이버 번들 | — |
| PostgreSQL | 지원 | 드라이버 번들 | — |
| MySQL / MariaDB | 지원 | 외부 MySQL 드라이버 | — |
| SQL Server | — | 드라이버 번들 | 지원 |
| Oracle | — | 외부 Oracle 드라이버 | 지원 |
| H2 | — | 드라이버 번들 | — |
| 기타 JDBC 데이터베이스 | — | 호환 드라이버로 기본 메타데이터 수집 | — |

네이티브 reader는 `sqlite:PATH`, `postgres://…` 또는 `postgresql://…`,
`mysql://…` URL을 받습니다. MariaDB도 `mysql://`를 사용하며, MySQL X Protocol
(`mysqlx://`)은 지원하지 않습니다. JDBC URL은 프로브에 전달하고, CLI는
프로브의 출력을 `scan --document`로 읽습니다.

JDBC 기본 수집 범위는 드라이버가 제공하는 스키마·테이블·컬럼·키·인덱스·루틴
메타데이터입니다. 본문과 사용 통계의 수집 범위는 DB 방언, 권한, 사용 가능한
카탈로그에 따라 달라집니다. 수집하지 못한 정보는 `limitations`에 보고합니다.

## 명령

`scan`의 기본 출력은 `graph.json`입니다. 그래프를 조회하거나 렌더링하는
명령도 기본적으로 이 파일을 읽습니다. 다른 스냅샷은 `--graph <path>`로
지정합니다.

| 명령 | 용도 |
| --- | --- |
| `scan <url>` | 카탈로그를 수집해 의존성 그래프를 만듭니다. |
| `scan --document <path>` | JSON 또는 NDJSON catalog document에서 그래프를 만듭니다. |
| `graph --format mermaid\|json\|dot` | 그래프를 렌더링합니다. `--level schema\|object\|column`으로 단위를 선택합니다. |
| `query <name> --depth N` | 의존 대상과 의존자를 근거 간선과 함께 조회합니다. |
| `impact <name>` | 변경의 영향을 받을 수 있는 의존자를 추적합니다. |
| `cycles` | 의존성 순환과 자기루프를 보고합니다. |
| `dead` | DB 그래프 안에 남은 의존자가 없는 뷰·루틴 후보를 보고합니다. |
| `stats` | 수집된 사용 통계와 수집 범위를 보여줍니다. |
| `rules --config <path>` | TOML 규칙으로 의존 간선을 검사합니다. |
| `diff <old> <new>` | JSON 형식의 그래프 스냅샷 두 개 또는 catalog document 두 개를 비교합니다. |
| `skill` | 코딩 에이전트를 위한 출력 계약과 사용 안내를 출력합니다. |

`cycles`, `dead`, `rules`, `diff`는 `--strict`를 지원합니다. 보고서를 출력하면서
각각 순환·후보·규칙 위반·차이를 발견하면 종료 코드 **1**을 반환합니다.
`query`와 `impact`도 이름을 해석하지 못하면 **1**을 반환합니다. 사용법·엔진
오류는 **2**입니다. 전체 옵션은 `schemagraph <command> --help`로 확인합니다.

`scan --inferred`는 선언되지 않은 `*_id` 참조를 이름 규칙으로 추정해 추가합니다.
선언된 외래 키가 우선하며, 모호한 일치는 `limitations`에 보고합니다. 추정
간선은 의존성 증거와 분리되어 의존성 질의나 규칙 검사에 영향을 주지 않습니다.

## 프로브

### JDBC: Kotlin / JVM

Gradle과 JDK 17로 빌드합니다. 다음 예제는 기존 SQLite DB를 스캔합니다.

```sh
gradle -p probe shadowJar
java -jar probe/build/libs/schemagraph-probe-all.jar \
    --url jdbc:sqlite:/path/to/database.db -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

PostgreSQL·H2·SQLite·SQL Server 드라이버는 번들에 포함됩니다. 다른 드라이버는
`--driver /path/to/driver.jar`로 전달하고, 자동 탐색이 실패하면
`--driver-class`를 지정합니다. MySQL과 Oracle 드라이버는 외부에서 제공합니다.

DB 사용자는 `--user`, 비밀번호는 `SG_DB_PASSWORD` 환경 변수로 전달합니다.
`--schema app,reporting`은 수집 범위를 지정한 스키마로 제한합니다. 기본값은
시스템 스키마를 제외한 전체입니다.

### Go: Oracle과 SQL Server

Go 프로브는 `go-ora`와 `go-mssqldb`를 사용하며 JVM이나 JDBC jar 없이 실행됩니다.
[probe-go/go.mod](probe-go/go.mod)에 맞는 Go 도구 체인으로 빌드합니다.

```sh
(cd probe-go && go build -o schemagraph-probe-go .)
./probe-go/schemagraph-probe-go --url "$SG_DATABASE_URL" -o catalog.json
schemagraph scan --document catalog.json -o graph.json
```

`SG_DATABASE_URL`에 Oracle(`oracle://…`) 또는 SQL Server(`sqlserver://…`)
접속 URL을 설정합니다. `--schema`와 `--format json|ndjson`도 지원합니다.
fixture 검증은 Oracle과 SQL Server에서 Go 프로브와 JDBC 프로브가 만든
그래프를 비교합니다.

### 문서 형식

두 프로브는 기본적으로 JSON을 출력하며 `--format ndjson`도 지원합니다.
`scan --document`는 두 형식을 자동으로 구분합니다. NDJSON은 `document` 헤더,
`schema` / `object` / `routine` 레코드, 마지막 `limitations` 레코드로 구성됩니다.

JDBC 프로브는 스키마 단위로 NDJSON을 출력합니다. 현재 Go 프로브는 전체 문서를
수집한 뒤 직렬화하고, Rust CLI는 입력 파일 전체를 읽은 뒤 그래프를 구성합니다.
따라서 NDJSON을 사용해도 전체 처리 과정의 메모리 사용량이 일정하게 제한되지는
않습니다.

## 결과 읽기

보고서는 그래프에 수집된 의존성을 설명합니다. 애플리케이션 쿼리는 그래프에
없으므로 `dead` 후보는 **삭제 권고가 아닙니다**. 테이블은 `dead` 후보에 포함되지
않으며, 뷰·물리화된 뷰·함수·프로시저·패키지가 후보가 될 수 있습니다.

- **근거와 한계:** 관측된 수집·파싱 누락은 `limitations`에 실립니다. 미지원
  루틴 언어, 해석하지 못한 참조, 모호한 이름은 보고하되 가짜 대상 정점을 만들지
  않습니다.
- **부분 결과:** `query`, `impact`, `dead`는 결과 제한으로 잘리면 `truncated`를
  보고합니다. 완전한 결과로 해석하기 전에 `limitations`와 함께 확인해야 합니다.
- **출력의 일관성:** 같은 입력 문서는 같은 그래프를 만듭니다. 실제 DB를 다시
  스캔하면 카탈로그와 사용 통계의 변화에 따라 출력이 달라질 수 있습니다.
- **본문 분석 범위:** 뷰·트리거에서 테이블·컬럼 의존성을 추출합니다. SQL 루틴,
  PL/pgSQL, PL/SQL, T-SQL은 본문 파싱이나 문장 추출을 지원하며 Oracle 패키지
  멤버도 포함합니다. 동적 SQL은 PostgreSQL dollar-quote, Oracle q-quote,
  T-SQL `EXEC(N'…')` / `EXECUTE(N'…')`처럼 전체 명령이 보이는 형태를 지원합니다.
  커서·루프·결과 반환 구문에서도 리터럴 명령과 바인딩 함수 호출을 복구합니다.
  연결식이나 변수로 조립하는 명령은 한계로 보고하며, 문자열 앞부분만 완성된
  명령으로 판정하지 않습니다. 중첩 컬럼 스코프의 분석 한계도 보고합니다.

### 사용 통계

PostgreSQL에서는 테이블·인덱스·루틴 통계를 수집합니다. MySQL과 MariaDB에서는
통계 카탈로그를 사용할 수 있을 때 테이블 통계와 미사용 인덱스 관측치를
수집합니다. `stats`는 관측치를 나열하고, `dead`는 후보별로 수집된 통계를
근거에 포함합니다.

`usage`가 없으면 관측치를 수집하지 못했다는 뜻입니다. `usage`가 있고
`reads: 0`이면 관측 기간에 0이 기록되었다는 뜻입니다. `since` 시각과 함께
해석해야 하며, 통계만으로 미사용을 확정할 수는 없습니다.
`track_functions=none`, `performance_schema=OFF`처럼 통계가 비활성화된 경우는
`limitations`에 보고합니다.

PostgreSQL 루틴의 `reads`는 호출 수이며, `total_ms`와 `self_ms` 실행 시간이
선택적으로 포함됩니다. 스냅샷 비교는 사용 통계의 변화를 제외하므로 카운터
증가가 스키마 변경으로 보고되지 않습니다.

## 아키텍처 규칙

`rules`는 기본적으로 `schemagraph.toml`을 읽습니다. 각 `[[rule]]`은 `from`과
`to` 글롭에 일치하는 의존 간선을 금지합니다. `*`는 점을 포함한 임의의 문자열,
`?`는 한 글자에 일치합니다.

```toml
[[rule]]
name = "reporting must not write to core"
from = "reporting.*"
to = "core.*"
kinds = ["writes"]

[[rule]]
name = "views must not call routines"
from = "*.order_totals"
to = "*"
kinds = ["calls"]
```

`kinds`를 생략하면 모든 의존 간선 종류를 검사합니다.
`schemagraph rules --graph graph.json --config schemagraph.toml --strict`로
실행하면 CI 검사에 사용할 수 있습니다. 보고서에는 규칙 이름과 위반 간선이
나오며, `checked: 0`은 검사한 규칙이 없다는 뜻입니다.

## 동작 구조

```text
Database ── native reader (Rust / sqlx) ──┐
Database ── JDBC probe (Kotlin) ──────────┼── catalog document ── graph ── queries
Database ── Go probe ────────────────────┘   versioned contract
```

DB 접근은 네이티브 source 어댑터와 프로브가 담당합니다. 프로브는 카탈로그
메타데이터와 SQL 원문을 수집하고, 그래프 구성·본문 파싱·의존성 분석은 Rust에서
수행합니다. 그래프 core에는 외부 의존성이 없으며 DB 없이 파일 fixture로
검증할 수 있습니다.

## 개발

[CI 워크플로](.github/workflows/ci.yml)는 push·pull request·수동 실행에서
Rust 포맷·엔진 빌드와 테스트·6개 크레이트 패키징·두 프로브 빌드·전체 DB
fixture를 검증합니다. 건너뛴 검사가 있으면 CI는 실패합니다. 패키지에 포함된
스킬 문서와 라이선스도 저장소 원본과 일치해야 합니다.

저장소 루트에서 실행합니다.

```sh
cargo fmt --manifest-path engine/Cargo.toml --all -- --check
cargo build --manifest-path engine/Cargo.toml --locked
cargo test --manifest-path engine/Cargo.toml --locked
Scripts/verify-fixtures.sh
```

fixture 스크립트는 SQLite, 로컬 PostgreSQL 도구, Docker, 프로브 도구 체인으로
DB 통합 검증을 수행합니다. 외부 MySQL·Oracle JDBC jar은 `SG_MYSQL_JAR`와
`SG_ORACLE_JAR`로 전달할 수 있습니다. 실행할 수 없는 검사는 건너뜀 경고를
출력하므로, 종료 코드가 0이어도 모든 DB를 검증한 것은 아닐 수 있습니다.
스크립트를 시작하기 전에 CLI를 빌드하고 검증이 끝날 때까지 바이너리를
교체하지 마세요.

P0–P6 단계는 구현되어 있습니다. 다음 작업은 절차형 SQL 분석 범위 보강,
Go 프로브 지원 방언 확장, additive 필드를 넘어서는 문서 버전
협상입니다.

설계와 출력 계약은 [DESIGN.md](DESIGN.md), 구현 상태와 검증 기록은
[HANDOFF.md](HANDOFF.md), 기여 지침은 [AGENTS.md](AGENTS.md)를 참고하세요.
이 유지보수 문서들은 한국어로 작성되어 있습니다.

## 라이선스

[MIT](LICENSE-MIT) 또는 [Apache-2.0](LICENSE-APACHE) 중 하나를 선택해 사용할 수
있습니다. 외부 드라이버에는 각자의 라이선스가 적용됩니다.
