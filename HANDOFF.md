# HANDOFF.md

> 세션을 이어받을 때 이 파일부터 읽으세요. 진행 상태와 다음 할 일이 여기 있습니다.

## 지금 상태

- 2026-09-21: **공개 PostgreSQL·SQLite 정확도 평가와 발견한 파서 오류 수정 완료**
  (`feature/accuracy-corpus`, 런타임 수정 `ad0e720`). 사용자가 공개 샘플 사용과
  불필요 산출물 정리를 요청했다. [ACCURACY.md](ACCURACY.md)가 영어 정본이다.
  - Pagila v3.1.0·Chinook v1.4.5의 commit·SHA-256·라이선스를 고정했다.
    데이터 행은 보관하지 않으며 수동 검토 SQL 27개와 원본 뷰 8개를 실DB에 적용한다.
    수동 읽기 144개·값 계보 66개와 PostgreSQL 카탈로그 참조 202개를 별도로 대조한다.
    DB가 USING 구문을 재작성해 오류를 숨기지 않도록, 작성한 사례는 실제 DB 출력
    컬럼과 원래 SQL을 조합한 동일 document를 공개 0.4.0·수정본·SQLGlot에 준다.
  - LEFT/RIGHT USING·NATURAL의 보존 측 값 계보, 이름 있는/상속된 WINDOW의
    partition/order 계보를 수정했다. 양쪽 조인 키의 읽기 의존성은 유지한다.
    잘못된 윈도 정의·순환·깊이·전체 확장 한계는 진단하고 출력 계보를 생략한다.
    소스 빌드는 아직 0.4.0으로 표시되며 공개 0.4.0 바이너리는 바뀌지 않았다.
  - 값 계보는 기존 62/66 정답·오탐 4·누락 4에서 66/66·오탐/누락 0으로,
    잘못된 complete 사례는 6→0으로 바뀌었다. SQLGlot 30.18.0의 선택적 비교기는
    61/66·오탐 5·누락 5였다. 독립 기대값을 쓰며 상대 도구 출력은 정답이 아니다.
    비교기 범위·UNKNOWN 타입·LATERAL 미해석 출처와 표본 편향을 문서화했다.
    원본 Pagila 뷰 3개는 custom aggregate 미수집으로 여전히 partial이다.
  - 로컬 Rust 244개 테스트·공개 코퍼스 strict·기존 버전의 strict 실패를 확인했다.
    [PR #3](https://github.com/ictechgy/schemagraph/pull/3)의 검사 소스는 `e1a6452`이며,
    [일반 CI·전체 DB fixture](https://github.com/ictechgy/schemagraph/actions/runs/35550571482),
    [Db2·Informix](https://github.com/ictechgy/schemagraph/actions/runs/35550571493),
    [Linux·macOS·JVM 빌드 및 실행](https://github.com/ictechgy/schemagraph/actions/runs/35550571497)을
    통과했다. CI의 정확도 report도 내려받아 66·144·202개 기대 간선 일치를 확인했다.
    push/PR 중복 실행 중 취소된 push 작업은 실패 검증으로 세지 않는다.
    임시 DB·그래프는 검사 종료 시 자동 정리하고, 중복 빌드 디렉터리 4개
    (4,256,976,896 bytes)는 `~/.Trash/schemagraph-duplicate-builds-*`로 옮겼다.
    휴지통을 비우기 전까지 디스크 여유 공간이 늘었다고 해석하면 안 된다.
    평가 원본 다운로드·비교 가상환경·중복 로그 약 22MB도 별도 휴지통 폴더로 옮겼다.
    전체 비교 결과·Rust 테스트 기록·CI 검증 요약 3개는 저장소 밖
    `~/Library/Application Support/schemagraph/verification/accuracy-20260921`에 보관했다.
    이 완료 기록만 갱신할 때는 위 검사 소스와의 동일성을 확인하고 실행 검증을 재사용한다.

- 2026-09-21: **v0.4.0 경쟁 조사 후속 구현·공개 배포 완료**.
  [GitHub 릴리스](https://github.com/ictechgy/schemagraph/releases/tag/v0.4.0) ·
  [crates.io CLI](https://crates.io/crates/schemagraph-cli/0.4.0) ·
  [Maven Central](https://central.sonatype.com/artifact/io.github.ictechgy/schemagraph-probe/0.4.0) ·
  [공개 Maven 저장소](https://ictechgy.github.io/schemagraph/maven/).
  릴리스 소스와 태그는 `f48bd1c389944ba5d40bfd1c4e85f77a19ac9f94`다.
  [구현 PR #1](https://github.com/ictechgy/schemagraph/pull/1)을 병합했으며,
  배포한 6개 crate의 레지스트리 체크섬·로컬 게시 바이트·내장 소스 커밋 일치를 확인했다.
  - **분석**: 컬럼 스코프/값 계보, 객체별 complete/partial/unsupported 진단,
    원문 해시·위치, `explain`·`path`, 보존 루트·이유/만료가 있는 억제,
    이전 그래프에서 제거 대상을 추적하는 `review`, FK index/PK prefix `lint`.
  - **수집·인터페이스**: 수집 맥락과 선택적 PG/SQL Server/Oracle 카탈로그 근거,
    SQL 파일 query 정점, source namespace `merge`, offline HTML, 고정 그래프 MCP,
    query/impact/review 탐색 예산과 선택적 몸체 분석 캐시를 추가했다.
    graph v2를 쓰고 기존 v1 읽기를 유지한다. 새 ID는 예약 문자를 escape하며
    일반 `schema.table`과 `fn(integer)`는 유지한다. 사용법은 [ANALYSIS.md](ANALYSIS.md).
  - **검증**: [235개 Rust 테스트·일반 DB 통합 CI](https://github.com/ictechgy/schemagraph/actions/runs/35524666271),
    [Db2·Informix 실DB](https://github.com/ictechgy/schemagraph/actions/runs/35523949136),
    Go test/vet·CGO 없는 빌드, JVM 및 Maven/Gradle 소비자, 6개 격리 패키지,
    JSON Schema, 실제 브라우저 검색·선택 검사를 통과했다. 기존 9개 DB fixture와
    PG/SQL Server/Oracle JDBC·Go의 catalog-dependencies v1/v2 JSON/NDJSON를 확인했다.
    네이티브 golden은 기존 정점·간선이 유지되는지 검토한 뒤 새 계보/메타데이터를 반영했다.
  - **배포 검증**: [Linux x86_64·macOS arm64 바이너리 및 standalone JAR](https://github.com/ictechgy/schemagraph/actions/runs/35524772437),
    [Pages Maven](https://github.com/ictechgy/schemagraph/actions/runs/35524666269),
    [Central 게시](https://github.com/ictechgy/schemagraph/actions/runs/35524781356)가 완료됐다.
    Central deployment `1e328452-2e5c-467f-afbf-0cc7eaa8c3c7`의 PUBLISHED와 실제 공개 파일을 확인했다.
    5개 payload의 체크섬·PGP 서명·Pages 바이트, all JAR의 GitHub 바이트가 일치한다.
    별도 crates.io 설치와 공개 macOS 압축파일에서 CLI 회귀를, Central만 사용하는
    별도 Java 소비자에서 H2 테이블·컬럼·PK를, standalone JAR에서 SQLite FK를 검증했다.
    서명키 지문은 기존 `0A98034C6F045509D1EE58EE329F94DC51434A1B`이며 비밀키 백업은 건드리지 않았다.
  - **성능·한계**: [PERFORMANCE.md](PERFORMANCE.md)에 재현 절차와 측정을 남겼다.
    3만 간선 합성 구축은 833→36ms, peak RSS는 32.09→27.70MiB였다.
    짧은 SQL 500개는 캐시가 느리고(122→158ms), 긴 IN 조건에서는 빨랐다(297→194ms).
    캐시는 기본 비활성이다. 미지원 SQL·불완전 수집·모호한 외부 참조는 계속 명시하며,
    CLI/MCP는 동기식이라 진행 중 요청의 협력적 취소는 라이브러리 API에만 있다.
    이번 범위는 기존 9개 DB 강화다. 신규 warehouse는 수요를 확인한 뒤의 후속 항목이다.
  - **CI 후속**: 임시 비밀번호가 하이픈으로 시작할 때 argparse가 오독하던
    검증기 호출을 [PR #2](https://github.com/ictechgy/schemagraph/pull/2)에서 고쳤다.
    [하이픈 비밀번호를 강제한 Db2·Informix 실DB 검사](https://github.com/ictechgy/schemagraph/actions/runs/35525936427)도 통과했다.
    `--option=value`로 전달하고 비밀번호에 하이픈 접두를 넣어 해당 경계를 항상 검사한다.
    이 변경은 fixture 실행기만 수정하며 공개 v0.4.0 런타임 바이트는 그대로 유지한다.
    검증 요약·체크섬·서명·소비자·브라우저 기록 13개는 저장소 밖
    `~/Library/Application Support/schemagraph/verification/v0.4.0-20260921`에 보관했다.

- 2026-09-20: **Maven Central v0.3.0 게시·설치 검증 완료**.
  [`io.github.ictechgy:schemagraph-probe:0.3.0`](https://central.sonatype.com/artifact/io.github.ictechgy/schemagraph-probe/0.3.0)을
  Central에 추가 게시했다. 소비자는 `mavenCentral()`만 사용하면 된다.
  사용자가 Central 게시 진행을 요청했고 `io.github.ictechgy` namespace가
  **Verified**라고 확인했다. 게시용 토큰을 GitHub Actions repository secrets의
  `CENTRAL_TOKEN_USERNAME`·`CENTRAL_TOKEN_PASSWORD`에 직접 등록했다.
  이후 사용자가 전용 PGP 키 생성·보관·공개키 배포를 승인했다. 전용 RSA 서명키를
  암호화해 저장소 밖 사용자 전용 경로에 보관하고 `MAVEN_SIGNING_KEY`·
  `MAVEN_SIGNING_PASSWORD`에 등록했다. 공개키 지문은
  `0A98034C6F045509D1EE58EE329F94DC51434A1B`이며 keyserver.ubuntu.com에서
  다시 내려받은 키로 5개 payload의 실제 서명을 검증했다. 비밀값은 출력하지 않았다.
  수동 Central workflow는 릴리스 태그에서 빌드하고 기존 Pages의 5개 payload와
  바이트가 일치해야 업로드한다. `publish=true`에서만 VALIDATED→PUBLISHED를
  진행한다. actionlint·셸/Python 구문과 동일 바이트 수용, 변조·필수 JAR 누락·
  HTTP/인증정보 포함 URL 거부 검사를 통과했다. 로컬 실제 서명 빌드·Pages와의
  5개 파일 일치도 확인했다. [main 통합 CI](https://github.com/ictechgy/schemagraph/actions/runs/35510942967)와
  [Central 게시](https://github.com/ictechgy/schemagraph/actions/runs/35511249308)가
  통과했다. deployment `232b5aac-1fe2-4557-8df3-95fb231cd1f1`의
  VALIDATED→PUBLISHED를 확인했다. Central에서 익명으로 받은 5개 payload의
  체크섬·서명·Pages와의 바이트 일치를 검증하고, 별도 Maven 저장소를 쓰는
  Central 전용 소비자로 H2 테이블·컬럼·PK를 읽었다. 독립 all JAR도 같은
  카탈로그를 냈다. 게시 도구는 `01208bb`, 빌드한 릴리스 소스는 `6da322e`다.
  이후 설치 문서·README·이 완료 기록만 갱신하며 위 실행 검증을 재사용한다.
- 2026-09-20: **v0.3.0 — 요청한 추가 3건 구현·검증·공개 배포 완료**.
  [CLI](https://crates.io/crates/schemagraph-cli/0.3.0) ·
  [GitHub 릴리스](https://github.com/ictechgy/schemagraph/releases/tag/v0.3.0) ·
  [공개 Maven 저장소](https://ictechgy.github.io/schemagraph/maven/).
  소스·태그·6개 크레이트의 기준 커밋은 `6da322efd877ddebb7bf56248c44a383a6569598`.
  레지스트리 체크섬·로컬 게시 아카이브·소스 커밋의 일치를 확인하고, 별도
  crates.io 설치로 SQLite 골든·문서 왕복·거부 경로·query·impact를 검증했다.
  - **메모리**: Rust의 원문·Value·출력 전체 복제를 줄이고 Go NDJSON을 스키마별로
    수집·방출한다. 1만 객체의 Rust RSS 중앙값은 JSON 633.5→268.8MiB,
    NDJSON 723.5→170.1MiB. 실제 PostgreSQL 4천 테이블에서 Go는 53.4→23.8MiB.
    모든 비교에서 그래프 내용과 usage 키 유무를 보존했다. [측정](PERFORMANCE.md).
  - **Db2 LUW·Informix**: 외부 JDBC 드라이버로 catalog·원문·시그니처를 읽고 SQL/SPL
    해석은 Rust가 담당한다. 실제 루틴·트리거 실행, 필요한/금지된 간선, overload,
    긴 원문 조각과 전송 네 조합을 검증했다. Db2는 24정점·33간선, Informix는
    SQL에서 호출 가능한 public routine을 포함해 543정점·558간선이다. [검증](IBM.md).
    Db2의 초기 연결 종료는 공식 setup 중 재시작 전에 테스트를 시작한 문제였고,
    완료 표시를 기다리게 고쳤다. Informix 환경 로딩·코드셋·opaque 타입·256자
    원문 조각은 실제 카탈로그로 확인해 처리한다.
  - **Maven**: 사용자가 선택한 GitHub Pages에
    `io.github.ictechgy:schemagraph-probe:0.3.0`을 익명 접근 가능한 저장소로 게시했다.
    thin·all·sources·Javadoc·POM과 체크섬을 확인하고, 깨끗한 Maven 저장소에서
    공개 URL만으로 의존성을 받아 H2 테이블·컬럼·PK를 읽었다. 독립 all JAR도
    같은 카탈로그를 냈다. 재게시 시 과거 버전·체크섬·동일 바이트를 보존한다.
    당시 Central 계정은 없었으며 이 배포에 필요하지 않았다. 이후 진행은 위 기록과
    [MAVEN.md](MAVEN.md)를 참고한다.
  Rust 148개 테스트, Go test·vet·CGO 없는 빌드, JVM 테스트, Maven/Gradle 소비자,
  6개 크레이트 패키지 빌드와 실제 DB 전체 검증을 통과했다.
  [릴리스 소스 통합 CI](https://github.com/ictechgy/schemagraph/actions/runs/35509238204) ·
  [IBM 실서버 CI](https://github.com/ictechgy/schemagraph/actions/runs/35509238260) ·
  [Maven 공개 배포](https://github.com/ictechgy/schemagraph/actions/runs/35509238244).
  이 기록 이후의 문서 갱신은 위 실행 코드의 검증을 재사용한다.
- 2026-09-20: **v0.2.0 배포·설치 검증 완료** —
  [crates.io CLI](https://crates.io/crates/schemagraph-cli/0.2.0) ·
  [GitHub 릴리스](https://github.com/ictechgy/schemagraph/releases/tag/v0.2.0) ·
  [최종 CI](https://github.com/ictechgy/schemagraph/actions/runs/35500610906).
  6개 크레이트를 게시하고 레지스트리 체크섬·로컬 게시 아카이브·소스 커밋
  `54549e657446df864b2b4f3d671650f84d009e75`의 일치를 확인했다. `v0.2.0` 태그는
  이 소스를 가리킨다. 레지스트리에서 별도 경로에 설치한 CLI로 버전·스킬·SQLite
  골든·v1/v2·NDJSON 왕복과 거부 경로·query·impact를 검증했다. 최종 CI의
  Rust 132개 테스트와 전체 DB fixture가 통과했고, 생산자 전송 조합 14개와
  건너뜀 0을 로그에서 확인했다. 이 뒤의 HANDOFF 갱신은 문서만 바꾸며 위 검증을
  재사용한다. `main`은 릴리스 소스와 이 완료 기록을 함께 유지한다.
- 2026-09-20: **v0.2.0 로드맵 구현 완료** (`feature/remaining-roadmap`).
  Go 프로브가 SQLite·PostgreSQL·MySQL/MariaDB를 추가해 Oracle·SQL Server와
  함께 5개 방언 계열을 수집한다. `CGO_ENABLED=0` 빌드와 네이티브/JDBC
  그래프 대조를 통과했다. SQL 상수 연결·PG `format` 일부·직선 구간의
  텍스트 변수 추적은 분기·불확실한 대입·지원하지 않는 형 변환에서 무효화하며,
  입력 크기·중첩 한도를 둔다. Oracle 패키지 슬라이서는 인용·주석·로컬 선언을
  구분하고 각 멤버의 실제 END 경계까지만 몸체를 귀속한다.
  catalog v2는 `reader`를 `producer.name`으로 바꾸고 `required_features`를
  요구한다. 기본 출력과 내부 모델은 v1이며, 두 버전은 같은 그래프로 정규화한다.
  v2 NDJSON에는 마지막 limitations 레코드가 필수다. `document-capabilities`,
  엔진·두 프로브의 `--document-version`, JSON↔NDJSON document diff를 추가했다.
  명세는 [CATALOG.md](CATALOG.md), 조합 검증은
  `Scripts/verify-document-versions.py`·`Scripts/verify-probe-versions.py`다.
  Rust 132개 테스트, Go test·vet·CGO 없는 빌드, JDBC shadowJar,
  전체 DB fixture(건너뜀 0)와 생산자별 v1/v2 × JSON/NDJSON 조합 14개가
  통과했다. PostgreSQL 골든은 새 동적 SQL 간선만 추가됐음을 확인해 갱신했다.
  최종 검토에서 index usage를 구조 diff에서 제외하고 package 소속 이동을
  graph id의 제거·추가로 보고하도록 수정했다. 미지 필드 경로의 무표시 절단을
  없애고, 중복 카탈로그 식별자는 종류별 실측 수를 limitation으로 남긴다.
  통합 CI·6개 크레이트 게시·레지스트리 설치 검증·GitHub 릴리스까지 완료했다.
- 2026-09-20: **동적 SQL 리터럴 복구와 오탐 방지 보강**.
  PostgreSQL dollar-quote·Oracle q-quote·T-SQL 괄호/N 리터럴을 복구하고,
  FOR/OPEN/RETURN QUERY EXECUTE와 Oracle OPEN FOR에서도 같은 경로를 쓴다.
  INTO·USING 바인딩의 함수 호출도 간선으로 수집한다. 문자열 뒤에 연결식이나
  원격 실행 꼬리가 있으면 앞부분만 완성된 SQL로 읽지 않고 미추출로 보고한다.
  키워드 탐색·문장 분할의 인용/주석 처리를 통일해 문자열 속 가짜 EXECUTE
  호출·BEGIN/END를 무시하고, 짧은 몸체의 범위 밖 슬라이스 패닉도 고쳤다.
  새 회귀 테스트를 포함한 Rust 96개 테스트와 전체 DB fixture(건너뜀 0)가
  통과했다. PG·Oracle·SQL Server fixture는 새 구문을 실제로 실행하며,
  `Scripts/verify-dynamic-sql.py`가 필요한 간선과 생기면 안 되는 간선을 함께
  확인한다. 당시 미지원이던 문자열 연결식·format·변수 추적은 위 v0.2.0에서 보강했다.
  CI에서 `cargo package`가 `target/debug/schemagraph`를 레지스트리 의존성으로
  링크한 바이너리로 덮어쓰는 문제도 재현했다. 패키징은 별도 target 경로로
  분리하고 작업용 CLI 해시가 변하지 않는지 검사한다. 기존 혼합 산출물 캐시를
  재사용하지 않도록 CI Rust 캐시 이름도 바꿨다.
- 2026-09-20: **GitHub Actions CI 추가** —
  [워크플로](.github/workflows/ci.yml) ·
  [실행 기록](https://github.com/ictechgy/schemagraph/actions/workflows/ci.yml).
  push·PR·수동 실행에서 Rust fmt·빌드·테스트, 스킬·라이선스 사본 일치,
  6개 크레이트 패키징, JDBC shadowJar, Go fmt·vet·빌드, 전체 DB fixture를
  검증한다. 기존 fixture 스크립트의 `주의:` 경고는 CI 실패로 취급해 검사를
  건너뛰고 성공하는 경우를 막는다. 실제 게시 명령이나 게시 토큰은 사용하지
  않는다. Ubuntu 24.04·Rust 1.96.0·JDK 17·Gradle 9.6.1을 사용하고 Go 버전은
  `probe-go/go.mod`에서 읽는다. 외부 액션은 커밋 SHA로, MySQL·Oracle 검증용
  JDBC jar은 버전과 SHA-256으로 고정했다. 워크플로 문법·셸 구문, 건너뜀
  실패 처리, 로컬 전체 패키징과 Go vet·빌드를 확인했다.
  [첫 GitHub 실행](https://github.com/ictechgy/schemagraph/actions/runs/35492301542)도
  Ubuntu 러너에서 전체 단계가 통과했고 DB fixture 건너뜀은 없었다.
- 2026-09-20: **v0.1.0 crates.io 배포 완료** —
  [CLI 패키지](https://crates.io/crates/schemagraph-cli/0.1.0) ·
  [GitHub 릴리스](https://github.com/ictechgy/schemagraph/releases/tag/v0.1.0).
  `schemagraph-core`·`schemagraph-analysis`·`schemagraph-source`·
  `schemagraph-export`·`schemagraph-parser`·`schemagraph-cli` 6개를 게시했다.
  모든 게시 아카이브의 체크섬과 소스 커밋 `7907634`를 확인했고, `v0.1.0`
  태그도 그 커밋을 가리킨다. 로컬 스냅샷이 아닌 레지스트리에서
  `cargo install schemagraph-cli --version 0.1.0 --locked`로 별도 경로에
  설치한 뒤 버전·스킬 문서·SQLite 골든·document 왕복·query·impact를 검증했다.
  첫 인증 실패는 재로그인으로 해결했고, 마지막 CLI의 신규 크레이트 등록
  속도 제한은 서버가 안내한 시각 이후 재시도해 해결했다.
- 2026-09-20: **v0.1.0 패키징 수정과 배포 검증 완료** (`f55b837`).
  CLI의 `include_str!`가 크레이트 밖의 스킬 파일을 읽던 패키징 결함을 수정했다.
  원본 `skills/schemagraph/SKILL.md`와 패키지용 `engine/cli/SKILL.md`는 같은
  내용을 유지해야 한다. 6개 크레이트가 영어 README를 상속하며 각 패키지에
  MIT·Apache 라이선스 사본을 포함한다. 라이선스 원문을 바꾸면 사본도 갱신한다.
  Rust 빌드·테스트·fmt, JDBC shadowJar, 전체 DB fixture(건너뜀 0), 6개
  크레이트의 격리 패키지 빌드와 `cargo publish --workspace --dry-run --locked`가
  통과했다. **Cargo 1.96에서는 workspace dry-run이 임시 레지스트리로 미게시
  형제 의존성을 검증할 수 있다.** 배포 준비는 개발 브랜치에서 진행하고,
  실제 게시와 레지스트리 설치 검증이 끝난 뒤 `main`에 반영했다.
- 2026-09-20: **공개 GitHub 저장소 생성** —
  [ictechgy/schemagraph](https://github.com/ictechgy/schemagraph).
  공개 기본 브랜치는 `main`, 개발 브랜치는 `feature/p0-engine`이다.
  Cargo 워크스페이스와 6개 크레이트에 `repository` URL을 반영했다.
  `README.md`를 영어 정본으로 퇴고하고 `README.ko.md`를 한국어 참고 번역으로
  추가했다. SQL Server 드라이버 번들·NDJSON 메모리 사용 설명도 구현에 맞췄다.
  `.serena/`·중첩 `sanddab/`·Kotlin 캐시는 `.gitignore`로 제외한다.
  문서 링크·셸/TOML 예제·번역 간 명령 일치, 기존 CLI의 SQLite 예제와 골든
  일치, 오프라인 Cargo 메타데이터 로딩을 확인했다. 런타임 코드는 변경하지
  않았으며 전체 DB fixture 검증은 아래 P6 실행 기록을 기준으로 한다.
  이 단계에서는 GitHub 공개까지만 진행했다.
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
- 2026-09-19: **P6 완료 — T-SQL·패키지 멤버·Go MSSQL·진짜 스트리밍·배포 준비**.
  - T-SQL(`4c84052`) — `sqlserver→MsSqlDialect` 매핑. 도매 파싱이 실패하면
    문장 추출기로 폴백해 `IF`/`TRY`/`EXEC`/`WHILE`/커서 안의 SQL을 복구.
    `write_targets`를 재귀로 고쳐 블록 안 DML이 reads로 오분류되던 것도
    같이 고쳤다. 앞 `--` 주석이 첫 조각을 통째로 삼키는 버그도 수정.
  - Oracle 패키지 멤버(`c31be9d`…) — `RoutineDoc.member_of` additive 필드.
    프로브 둘 다 멤버를 `pkg.member#ov` 키로 수확하고 몸체를 슬라이스해
    싣는다. 그래프는 멤버 정점+`contains` 간선, `pkg.member()` 호출은
    멤버로 해석(모호·미발견은 추측하지 않고 limitation). 두 패스 루프로
    정렬 순서 무관하게 패키지가 먼저 생긴다. JVM↔Go 문서·그래프 완전
    패리티. Kotlin merge()가 memberOf·signature를 떨구던 버그도 수정.
  - Go 프로브 MSSQL(`mssql.go`) — go-mssqldb로 sys.* 수확. JVM과 정점·
    간선·limitation 완전 패리티(Azure SQL Edge, encrypt=disable 필요).
  - 진짜 스트리밍(`ef08ae4`) — Extractor가 스키마 단위로 수확·방출한다.
    NDJSON 정본: 헤더 limitations는 비우고 마지막에
    `{"type":"limitations","data":[…]}` 트레일러. 리더는 헤더+트레일러
    합집합·정렬·중복 제거. SQLite·Oracle·MSSQL에서 JSON 경로와 그래프
    동일 확인. 스키마 후보는 getSchemas ∪ 전역 테이블 스캔으로 옛 전역
    수집과 같은 커버리지 보장.
  - 배포 준비(`e51bc95`) — LICENSE-MIT·LICENSE-APACHE, path 의존성
    버전, keywords·categories. `schemagraph-core` dry-run 패키징 통과.
    미게시 형제 의존은 dry-run 해석이 안 되므로 실게시는 의존 순서로.
  - 검증 상태(`5292b12`, `feature/p0-engine`): cargo test 88개,
    verify-fixtures.sh 전 섹션 통과(주의·건너뜀 0 — Oracle 패키지 멤버
    assertion과 JVM↔Go 패리티 포함), fmt·go vet·gradle shadowJar 깨끗.
    작업 트리 clean, 원격 없음 — P6 커밋 13개(4c84052..5292b12).

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
  (verify-fixtures.sh 주석에도 명시). **실행 중 `cargo build`로 바이너리를
  교체해도 안 된다** — 검증이 낡은/새 바이너리를 섞어 써 오탐이 난 적 있다.
- **verify-fixtures의 "주의: … 건너뜀"은 stderr에만 찍히고 종료 코드는
  0이다** — 성공한 섹션은 무출력이라, 끝부분 출력에 주의가 없는지로
  전 섹션 실행을 확인한다. oracle-free 컨테이너는 무거워 다른 Oracle
  인스턴스와 동시 기동하면 기동 실패로 Oracle 섹션이 건너뛴다 —
  돌리기 전 `docker ps`로 남은 sg-* 컨테이너를 정리한다.
- **프로브에 문서 필드를 추가할 때는 세 경로를 같이 갱신한다** — 수확
  조립, merge() 재조립, NDJSON 라인. Kotlin merge()가 `memberOf`·
  `signature`를 떨궈 JVM 출력에서만 null이 된 적 있다(실검증에서 발견,
  단위 검사만으로는 안 잡힘).
- Azure SQL Edge는 자체 서명 인증서라 go-mssqldb가 TLS 검증에 실패한다 —
  연결 문자열에 `encrypt=disable`을 넣는다(테스트용, JVM은 trustServer
  Certificate를 따로 쓴다).
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
- routine 몸체 파싱 게이트는 언어별 세 갈래다 — `sql`/무표기는 방언
  파서로 통째 파싱(T-SQL은 MsSqlDialect, 도매 실패 시 문장 추출 폴백),
  `plpgsql`/`plsql`은 문장 추출기로 보이는 SQL만 회수, 그 외 언어는
  파싱하지 않고 limitation — 몸체 간선이 없다는 사실을 숨기지 않는다.
  `pg_get_functiondef`의 `AS $$…$$`·`AS '…'` 껍질은 `extract_as_body`가
  벗긴다.
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
(cd engine && cargo build --workspace --locked && cargo test --workspace --locked) # 132개 테스트
Scripts/verify-fixtures.sh                    # 네이티브 + JDBC + Go, 전체 DB/전송 조합 검증
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

요청한 성능·메모리 개선, Db2·Informix 특화 지원, 공개 Maven 배포를 완료했다.
추가 요청된 Maven Central 게시도 실제 익명 설치까지 검증했다.
공개 정확도 평가 후속은 위 최신 기록을 참고한다. 서명키와 토큰은 다음 릴리스에서도
기존 설정을 재사용한다. 이번 파서 수정은 공개 0.4.0 이후 변경이며 새 릴리스는 아직 없다.

## 의도적으로 남긴 경계

- 실행 시점 입력·분기 결과에 의존하는 SQL은 추측하지 않고 limitation으로 남긴다.
- JDBC·Go의 NDJSON은 스키마 단위로 방출하지만 Rust 내부 카탈로그·그래프는
  전체 메모리를 사용한다. Go JSON도 전체 문서를 유지한다. 성능과 메모리 한계는
  [PERFORMANCE.md](PERFORMANCE.md)에 측정 근거와 함께 설명한다.
- 모든 멤버 id에 kind를 넣는 변경은 기존 id 호환성 때문에 보류한다.
- Central·GitHub Pages의 기존 버전은 불변이며, 같은 좌표로 다른 JAR·POM을
  게시하지 않는다. 새 배포는 기존 릴리스 태그와 Pages 파일을 기준으로 검증한다.

세부 근거는 DESIGN.md "호환 계약과 확장 경계" 절을 참고한다.
