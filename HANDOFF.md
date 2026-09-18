# HANDOFF.md

> 세션을 이어받을 때 이 파일부터 읽으세요. 진행 상태와 다음 할 일이 여기 있습니다.

## 지금 상태

- 2026-09-19: 저장소 생성. 설계 정본(DESIGN.md)·에이전트 규칙(AGENTS.md)·
  README 확정. **코드는 아직 없음.**

## 확정된 결정

- 이름: `schemagraph`
- 언어: **Rust 엔진 + Kotlin/JVM 프로브 복합** — 버전 달린 catalog document가 경계
- 지원 범위: JDBC가 닿는 모든 DB. Tier 0 Generic 프로브가 바닥이고,
  네이티브 sqlx로 PG·MySQL·SQLite를 먼저 깊게 판다
- 차별점: FK뿐 아니라 view·routine·trigger 몸체 파싱 간선 + 에이전트용 판정 질의

## 다음 할 일

1. `engine/` cargo workspace 스캐폴드(core·parser·source·analysis·export·cli)
2. core 그래프 모델 — 정점/간선 타입을 DESIGN.md "그래프 모델" 절대로
3. 네이티브 경로(sqlx) SQLite 리더부터 — 파일 DB라 fixture·도그푸딩에 유리
4. 네이티브 경로가 뱉는 document 형태가 곧 프로브 프로토콜의 씨앗 — 버전 필드 필수

## 미결

DESIGN.md "미결 사항" 절 참조: document 스키마 세부, routine 파싱 커버리지,
프로브 전송 방식, crates.io/Maven 이름 선점 확인, inferred 휴리스틱.
