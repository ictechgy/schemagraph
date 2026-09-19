# AGENTS.md

이 저장소에서 작업하는 코딩 에이전트를 위한 안내입니다.

> This file is written in Korean because it is the maintainer's working language.
> For the project overview in English, see [README.md](README.md).

**설계의 정본은 [DESIGN.md](DESIGN.md)입니다.** 새 기능을 넣을 때
"이것도 그래프 질의로 표현되는가"를 먼저 물어보세요.

**지금 어디까지 왔고 다음이 무엇인지는 [HANDOFF.md](HANDOFF.md)에 있습니다.**
세션을 이어받을 때 먼저 읽으세요.

자매 프로젝트가 바탕화면에 있습니다: [cartograph](../cartograph)(Swift) ·
[kartograph](../kartograph)(Kotlin/Android) · [dartograph](../dartograph)(Dart/Flutter) ·
[isthmus](../isthmus)(언어 경계 조인). 이 저장소의 프로브↔엔진 document 계약은
isthmus의 GRAPH-EXCHANGE와 같은 격입니다. 출력 계약 관례(`query` JSON의
`state`/`edges`/`limitations` 철학)는 계열과 같게 유지합니다.

---

## 이 프로젝트가 하는 일

DB 카탈로그와 SQL 몸체를 읽어 의존성 그래프를 만들고, 그 위에서
깨짐(impact)·순환(cycles)·미사용(dead)·규칙(rules)을 질의합니다.

핵심 설계는 한 문장입니다. **그래프가 산출물이고, 나머지는 전부 그 위의 질의입니다.**

구조는 둘로 갈립니다. **엔진(Rust)**은 DB를 직접 만지지 않고 catalog document만
소비하는 순수 함수입니다. **프로브(JVM/Kotlin, P2~)**는 JDBC로 카탈로그와 몸체
원문을 긁어 document를 뱉는 멍청한 추출기입니다. 판정·파싱·그래프 의미론은
항상 엔진에만 있습니다.

## 절대 하지 말 것

- **`core`에 외부 의존성을 추가하지 마세요.** 그래프 도메인이 순수해야 분석 계층
  전체를 DB 없이 파일 fixture로 테스트할 수 있습니다.
- **sqlx와 프로세스 spawn을 `source` 밖에서 쓰지 마세요.** 마찬가지로
  sqlparser-rs는 `parser` 안에서만, JDBC는 `probe` 안에서만 씁니다.
- **프로브를 똑똑하게 만들지 마세요.** 파싱·판정·간선 생성이 JVM으로 새 나가면
  그래프 의미론의 권위가 둘이 되어 같은 입력이 어디서 스캔됐냐에 따라
  달라집니다. 프로브는 원문을 옮기기만 합니다.
- **JSON 출력은 결정적이어야 합니다.** 정렬된 키, 정렬된 배열. 같은 입력이 매번
  다른 파일이 되면 리포트 diff와 캐시가 모두 무의미해집니다.
- **삭제 판정을 내지 마세요.** `dead`는 "보존 루트에서 도달할 수 없다"는 그래프
  사실을 evidence와 함께 보고할 뿐, "지워도 된다"고 말하지 않습니다. DB에는
  `main()`이 없어서, 참조 없는 테이블이 가장 핫한 테이블일 수 있습니다.
  확신이 없으면 살리는 쪽을 고르고 이유를 남기세요.
- **사용 통계를 단독 증거로 쓰지 마세요.** 통계는 리셋 시점 이후만 유효합니다.
  `since`를 함께 실어 소비자가 스스로 판단하게 합니다.
- **`limitations`를 장식으로 쓰지 마세요.** 파싱 실패 routine 수, 통계 부재,
  드라이버가 숨긴 카탈로그 — 그 DB에서 **실제로 세어서** 만듭니다. 매번 붙는
  상투적 경보는 읽히지 않습니다.
- **커버리지 숫자를 올리려고 아무것도 검증하지 않는 테스트를 쓰지 마세요.**
  각 단계의 완료 조건은 실제 DB fixture로의 양방향 검증입니다.

## 에이전트가 소비하는 출력

`query`·JSON 리포트는 사람이 아니라 코딩 에이전트가 읽는다고 전제합니다.
DESIGN.md "에이전트 출력 계약" 절이 계약입니다 — `truncated`/`depth`/`level`
표기, 이웃 간선 전부 나열, 없는 선택 필드는 키 생략, 억제 표시는 실제 보고된
정점에만.

## 커밋

Conventional Commits, 본문은 한국어. 스코프는 모듈 이름을 씁니다
(`core`, `parser`, `source`, `analysis`, `export`, `cli`, `probe`, `config`).

```
feat(analysis): impact 역방향 전이 클로저 구현
fix(source): MySQL FK 메타데이터가 스키마 한정 없이 읽히던 문제 수정
```

커밋은 작고 한 가지 목적만 담습니다. 본문에는 *왜*를 쓰세요.
`main`에 직접 커밋하지 말고 `feature/…`, `fix/…`, `refactor/…` 브랜치에서
작업하세요.

## 코드 스타일

- 주석은 한국어, 식별자는 영어. 사용자에게 보이는 출력 문자열은 영어(오픈소스 대상).
- 모든 public 타입·함수에 문서 주석. *무엇을*이 아니라 *왜*를 적으세요.
- 함수는 하나의 역할만. 본문 10줄을 넘기면 분리를 검토하세요.
- 빈 `catch`/`Err` 무시 금지. 오류 메시지에는 원인과 해결 방향을 함께 담습니다.
  프로덕션 경로의 `unwrap`은 왜 실패 불가인지 주석이 없으면 안 됩니다.
