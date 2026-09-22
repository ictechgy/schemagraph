# 경쟁 비교와 검증 중심 보강 계획

확인일: 2026-09-22. 공개 기준 버전: **v0.4.3**.
아래 기능 비교는 공식 문서와 저장소 구현을 대조한 결과다. 경쟁 제품을 같은
입력으로 실행한 정확도·속도 순위가 아니다. 에디션·권한·DB 버전에 따라 기능
범위가 달라진다. 이전 v0.3.0 진단은 [v0.4.3 태그의 조사 문서](https://github.com/ictechgy/schemagraph/blob/v0.4.3/COMPETITIVE-ANALYSIS.md)에
남아 있으며, 당시의 미구현 목록을 현재 상태로 읽으면 안 된다.

## 제품의 초점

여러 생산자가 수집한 카탈로그와 SQL을 하나의 재현 가능한 그래프로 만들고,
변경이 어떤 SQL·객체·애플리케이션 사용처에 영향을 주는지 근거와 한계로
설명하는 개발자·코딩 에이전트용 도구를 지향한다. 의존성·MCP·다이어그램의
존재 자체를 독점적 기능으로 설명하지 않는다.

| 비교 대상 | 공식 자료에서 확인한 범위 | 보강할 사용자 경험 |
| --- | --- | --- |
| SchemaCrawler | view/routine 참조, 상세 카탈로그, lint, snapshot, MCP | 방언·권한별 수집 범위와 분석 근거를 검증 가능한 형태로 제공 |
| tbls | 문서·diff·lint와 여러 설치 경로, CI Action | 한 단계로 검토를 실행하고 결과를 CI에 남기는 소비자 흐름 |
| Atlas / Redgate Flyway | migration 검사, 변경 보고, 정책과 CI 연계 | 변경 사실·영향·차단 정책을 연결한 PR 검토; 일부 기능은 상용 에디션 |
| DataHub / dbt | 모델·쿼리 입력과 컬럼 계보, 외부 데이터 사용처 연결 | compiled SQL과 카탈로그 문맥을 함께 수집해 DB 밖 의존성을 연결 |
| Azimutt / SQL Dependency Tracker | 관심 객체·경로 중심 탐색, 의존성 검토 | 변경에서 근거와 영향 경로로 이동하는 일관된 탐색 |

근거: [SchemaCrawler metadata](https://www.schemacrawler.com/metadata-retrieval.html),
[SchemaCrawler MCP](https://www.schemacrawler.com/mcpserver.html),
[tbls](https://github.com/k1LoW/tbls),
[Atlas lint](https://atlasgo.io/versioned/lint),
[Flyway check](https://documentation.red-gate.com/flyway/reference/commands/check),
[DataHub SQL parsing](https://github.com/datahub-project/datahub/blob/master/docs/lineage/sql_parsing.md),
[dbt column lineage](https://docs.getdbt.com/docs/explore/column-level-lineage),
[Azimutt path](https://azimutt.app/docs/find-path),
[SQL Dependency Tracker](https://www.red-gate.com/products/sql-dependency-tracker/).

## v0.4.3에서 이미 제공하는 것

- 스키마·스코프 기반 view 컬럼 해석과 값 계보: CTE, wildcard, 파생 테이블,
  상관 서브쿼리, set operation, named window를 포함한다.
- 객체별 분석 상태·진단·근거, `explain`·`path`, 보존 루트와 만료 예외.
- catalog 변경과 영향 분석을 결합한 `review`, JSON·Markdown, strict gate,
  수집 범위 비교와 불완전 분석 표시.
- SQL 파일 입력, DB 카탈로그 참조, 명시적 source ID에 따른 다중 문서 merge.
- 읽기 전용 MCP, 오프라인 HTML, 탐색 예산·취소와 SQL 몸체 캐시.
- Linux x86_64·macOS arm64 바이너리, Go·JDBC 프로브, crates.io·Maven 게시.
- 공개 DB 표본 및 실제 DB fixture, 단일 대형 스키마·밀집 그래프 측정.

구현과 완료 근거는 [ANALYSIS.md](ANALYSIS.md), [ACCURACY.md](ACCURACY.md),
[PERFORMANCE.md](PERFORMANCE.md), [HANDOFF.md](HANDOFF.md)를 따른다.
이 기능들을 다시 미구현 과제로 잡지 않는다.

## 승인된 후속 작업과 완료 조건

사용자가 아래 후속 구현을 승인했다. 이 표는 작업 계약이며 완료 주장으로
사용하지 않는다. 진행 상태와 실제 실행 근거는 HANDOFF에 기록한다.

| 단계 | 보강 범위 | 검증할 완료 조건 |
| --- | --- | --- |
| G1 | review 정책·기준선·만료 waiver·SARIF·소비자 Action | 기본 출력/종료 코드 호환, 전체 변경으로 gate 판단, 출력 제한 우회 방지, 정확한 fingerprint, 형식별 동일 결정, 실제 Action 실행 |
| G2 | 독립 DML 코퍼스 | 기대값을 분석 전 고정, 실제 DB SQL/행 결과 확인, 첫 결과 보존, writer별 origin과 상수 목적지의 잘못된 계보 검출 |
| G3 | INSERT SELECT·CTAS·UPDATE FROM·MERGE·순차 임시 결과 | reader가 부여한 실제 컬럼만 연결, 조건 읽기와 값 계보 구분, 불확실한 제어 흐름은 partial, 실제 DB와 양방향 대조 |
| G4 | dbt artifact·관측 query log 입력 | stable query identity, DB/스키마 문맥 확인, 관측 기간·누락 보존, 재수집 중복 방지, 어댑터에서 SQL 의미를 추론하지 않음 |
| G5 | 운영 지원표·권한 검사·배포 도입 | 실제 검증한 DB/버전/역할/객체 범위만 표기, 수집 범위 변화를 DROP으로 확정하지 않음, Docker 및 소비자 실행 검증 |
| G6 | 의존 대상별 캐시 무효화·추가 규모 검증 | 무관한 변경에서 재사용, 참조/모호성/negative lookup 변화는 재분석, cold/warm 전체 결과 일치, JDBC·큰 몸체 계측 경계 공개 |

Windows 등 신규 플랫폼과 신규 웨어하우스의 지원은 실제 빌드·DB 검증을 마친
범위만 선언한다. 이번 작업에서 검사하지 않은 조합은 지원표에 미검증으로
남긴다. 외부 관측·동적 SQL이 없는 것을 사용되지 않음으로 해석하지 않는다.

## DML 계보와 캐시의 의미 경계

쓰기 대상 객체·컬럼과 읽은 컬럼, 값의 원천을 구분한다. 예를 들어 필터에만
쓰인 컬럼은 `reads`이며 출력 값의 `derives-from`으로 넣지 않는다. 원시 객체
읽기에서는 기존처럼 DML의 쓰기 대상 관계를 중복 집계하지 않는다. 컬럼 간선을
객체 수준으로 투영하면 새로운 읽기가 보일 수 있으므로 별도 호환 확장으로
검증한다.

`UPDATE t SET x=x+1`의 이전 값과 새 값은 현재 reader 정점 ID 하나로 구분할 수
없다. 자기 `derives-from`으로 스키마 cycle을 만드는 대신 컬럼 읽기·쓰기를
남기고 temporal self-lineage를 명시적 partial로 다룬다. 임시 테이블은 파서의
문장별 symbol로 다루고 원천 컬럼으로 접으며 가상의 graph 정점을 만들지 않는다.

캐시는 현재 객체의 몸체 효과만 보관한다. 물리 대상 컬럼의 계보를 전역 graph에서
다시 긁으면 다른 writer의 효과가 섞일 수 있으므로 cold/warm 적용 경계를 같게
검증한다. 이름 해석 문맥을 안전하게 추적하지 못한 경로는 전체 구조 fingerprint를
사용해 재분석한다. 이는 SQL 분석 재사용이며, 전체 카탈로그와 graph의 상주 메모리
제약이 사라졌다는 뜻이 아니다.

## 비교 수치의 사용

현재 SQLGlot 비교는 API adapter와 명시한 계보 정책의 비교다. 일부 입력 타입을
UNKNOWN으로 넘기며, 수정에 사용한 회귀 코퍼스의 점수를 제품 전체 정확도로
확장하지 않는다. 새 코퍼스는 실제 SQL 유효성·독립 기대값을 먼저 고정하고,
누락·오탐·거짓 complete·미지원 범위를 분리한다. 값 계보와 필터/정렬 읽기 등
정의가 다른 항목은 정의를 맞춘 뒤 비교하거나 별도 지표로 표시한다.
