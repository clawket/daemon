# sse-event-wire-contract

## Purpose
`app.emit(<event_name>, …)` 로 발행되는 SSE 이벤트 문자열은 CLI / 웹 / MCP 구독자가 직접 의존하는 wire contract 다. 라우트 측 emit 과 `state.rs` 의 정적 매핑이 동기되지 않으면 다운스트림이 silent 하게 fallthrough 한다.

## Prevents
- 라우트에 새 이벤트(`knowledge:archived` 등)를 추가했는데 `state.rs::parse_event_name` 업데이트를 잊어 `("unknown", "unknown")` 으로 broadcast 되는 케이스.
- 기존 이벤트 이름을 리네임(`knowledge:created` → `artifact:created`) 하면서 CLI/웹 구독자가 다음 릴리스 사이에 끊기는 케이스.
- emit 위치는 그대로 두고 매핑 테이블만 지우는 역방향 실수.

## Evidence
- `src/state.rs` `parse_event_name` — 정적 `match`. fallback 은 `("unknown", "unknown")` 로 떨어지며 entity / change 라벨이 사라진다. (줄번호는 적지 않는다 — 이 표의 참조가 실제로 낡아 세 라운드 연속 지적됐다.)
- `src/routes/knowledge.rs:117,134,189` — `app.emit("knowledge:created" | "knowledge:deleted" | "knowledge:updated", …)` 의 세 emit 사이트.
- `daemon/CLAUDE.md:110` — "`/knowledge/*` 라우트를 변경하면 **같은 커밋에서** `state.rs` 의 mapping 을 함께 갱신한다" 라는 invariant 본문.

## Why not global
글로벌 룰은 wire contract 일반 원칙만 다룬다. daemon 특유의 "emit 사이트 + 정적 lookup" 이중 정의 패턴, 그리고 fallback 이 조용히 unknown 으로 떨어지는 행동은 이 repo 의 구현 디테일이다. 다른 sub-repo (cli/web) 는 emit 하지 않고 구독만 하므로 동일한 이중 책임이 없다.

## Enforcement gap
- 라우트가 `app.emit(...)` 를 추가했을 때 `state.rs` 매핑 누락을 컴파일 타임에 잡는 매크로/타입이 없다. 이벤트명이 `&'static str` 로 흘러가서 unknown fallback 까지 통과한다.
- 테스트 타임에는 잡는다: `state::tests::every_emitted_event_name_is_mapped` 가 소스에서 이벤트명 리터럴을 스캔해 매핑 누락을 실패로 만든다(`emit("…"` 직접 호출과 `cascade_complete` 가 반환하는 `.push(("…"` 두 형태). 런타임에 조립되는 이름(`format!`)은 여전히 못 본다 — 그런 이름을 도입하면 그 스캔을 함께 확장해야 한다.

## Rule body
새 SSE 이벤트를 추가/변경/삭제할 때는 다음을 **같은 커밋에서** 함께 처리한다.

### DO
- emit 사이트(`app.emit("<entity>:<change>", json!({...}))`)와 `state.rs::parse_event_name` 의 `match` 분기를 한 diff 안에서 같이 갱신한다.
- 이벤트명은 `<entity>:<change>` 형식으로 유지한다(`entity` 는 snake, `change` 는 과거형 동사).
- 이벤트 페이로드 필드는 추가만 허용. 기존 필드를 제거/리네임하지 않는다(필요 시 새 이벤트를 발행).
- 새 이벤트는 `tests/` 하위에 broadcast → `parse_event_name` 결과를 어서트하는 케이스를 추가한다.

### DON'T
- emit 만 추가하고 `parse_event_name` 매핑을 빠뜨려 fallback `("unknown", "unknown")` 로 흘리지 않는다.
- 매핑 테이블에서만 항목을 지우면서 emit 은 남겨두지 않는다(반대 방향도 금지).
- 이벤트 이름을 silent rename 하지 않는다 — 옛 이름 emit 을 한 릴리스 사이클 유지 후 제거하는 단계적 deprecation 으로만 변경.
- payload JSON 의 키 이름을 임의로 바꾸지 않는다 — `response-shape-backwards-compat.md` 와 함께 적용.
