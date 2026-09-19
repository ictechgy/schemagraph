# HANDOFF.md

> 세션을 이어받을 때 이 파일부터 읽으세요. 진행 상태와 다음 할 일이 여기 있습니다.

## 지금 상태

- 2026-09-19: **P0 완료.** `engine/` Rust 워크스페이스에 core·source·analysis·
  export·cli 크레이트. SQLite 네이티브 reader가 카탈로그를 읽어 그래프를 만들고,
  `scan`/`graph`/`query`/`cycles`가 동작한다. 18개 단위 테스트 통과,
  `Scripts/verify-fixtures.sh`가 fixture+골든으로 양방향 검증한다.

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

## 검증 명령

```bash
cd engine && cargo build && cargo test        # 빌드 + 18개 테스트
Scripts/verify-fixtures.sh                    # fixture 양방향 검증 (골든 diff 포함)
```

## 다음 할 일 (P1)

1. `parser` 크레이트 — sqlparser-rs로 view 본문을 파싱해 `reads` 간선 생성.
   limitations의 "view 본문 미파싱" 보고가 실제로 줄어드는지 fixture로 확인.
2. `impact` 명령 — query의 역방향 전이 클로저 + 간선 종류별 분해.
3. PG·MySQL 네이티브 reader — docker로 로컬 DB 띄워 fixture 검증.

## 미결

DESIGN.md "미결 사항" 절 참조: document 스키마 세부, routine 파싱 커버리지,
프로브 전송 방식, crates.io/Maven 이름 선점 확인, inferred 휴리스틱.
