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
  32개 단위 테스트 통과, `Scripts/verify-fixtures.sh`가 reads·writes·impact·dead까지
  fixture로 양방향 검증한다.

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

## 검증 명령

```bash
cd engine && cargo build && cargo test        # 빌드 + 32개 테스트
Scripts/verify-fixtures.sh                    # fixture 양방향 검증 (골든 diff 포함)
```

## 다음 할 일

1. PG·MySQL 네이티브 reader — docker로 로컬 DB 띄워 fixture 검증.
2. routine(function/procedure) 몸체 파싱 → `calls` 간선 (PG `pg_proc.prosrc`,
   MySQL `information_schema.ROUTINES`). routine은 SQLite에 없어 PG reader와 같이.
3. `rules` — 레이어/순환 규칙 선언 (config 파일이 P2와 같이 온다).
4. catalog document 스키마 고정 → Kotlin/JDBC 프로브(P2).

## 미결

DESIGN.md "미결 사항" 절 참조: document 스키마 세부, routine 파싱 커버리지,
프로브 전송 방식, crates.io/Maven 이름 선점 확인, inferred 휴리스틱.
