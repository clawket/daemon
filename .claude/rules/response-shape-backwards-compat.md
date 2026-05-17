# response-shape-backwards-compat

## Purpose
HTTP 응답 / SSE 페이로드 JSON 의 필드 모양은 외부 클라이언트(CLI, 웹, MCP) 가 동시에 갱신될 수 없다. 데몬은 새 필드를 **추가**할 수는 있지만, 기존 필드를 **제거 / 리네임 / 타입 변경**해서는 안 된다.

## Prevents
- `prev_hash` / `envelope_id` / `envelope_snapshot` 같이 `Option<T>` + `skip_serializing_if = "Option::is_none"` 으로 도입된 필드를 non-optional 로 바꾸거나 다른 이름으로 옮겨, 옛 클라이언트가 deserialize 실패하거나 panic 하는 케이스.
- `serde(rename)` 키(예: `Knowledge.type_` → `"type"`) 가 변경되어 wire 가 깨지는 케이스.
- 필드 타입을 `String` → `i64` 같이 silent 변환해서 클라이언트 파싱이 무력화되는 케이스.

## Evidence
- `src/models.rs:177-178` — `prev_hash: Option<String>` 이 `#[serde(skip_serializing_if = "Option::is_none")]` 로 wire 에 노출된 사례.
- `src/models.rs:276-279` — `envelope_id` / `envelope_snapshot` 동일 패턴.
- `src/models.rs:286-296` — `Knowledge.type_` 이 `#[serde(rename = "type")]` 로 wire key 가 `type` 임. 키 이름이 사람이 쓰는 contract.

## Why not global
글로벌 룰은 wire contract 일반 원칙만 다룬다. daemon 의 `models.rs` 는 `serde(rename)` + `Option<T>` + `skip_serializing_if` 의 결합 패턴으로 비파괴 진화를 표현한다. 이 패턴을 보존하는 책임은 이 repo 안에서만 의미가 있고, 컴파일러는 모양 변경을 silent 하게 통과시킨다.

## Enforcement gap
- 응답 스키마 stability 를 보증하는 snapshot/golden test 가 없다 — 컴파일은 통과해도 wire 가 깨진다.
- `serde` derive 변경은 코드 리뷰 외에 자동 차단 수단이 없다.
- OpenAPI / JSON schema 산출물이 없어 클라이언트 측 contract drift 가 감지되지 않는다.

## Rule body
`models.rs` 의 `Serialize` 구조체와 라우트 응답 JSON 을 변경할 때 다음을 지킨다.

### DO
- 새 필드는 항상 `Option<T>` + `#[serde(skip_serializing_if = "Option::is_none")]` 로 추가한다 — 옛 클라이언트가 unknown 필드를 무시할 수 있을 때.
- 의미가 바뀐 필드는 **새 이름**으로 추가하고 옛 필드를 deprecate 표시한 채 한 릴리스 이상 유지한다.
- `#[serde(rename = "...")]` 로 노출된 wire key 는 변경 시 새 alias 를 함께 노출한다.
- 응답 모양을 바꾸면 같은 커밋에서 `tests/` 하위에 serialized JSON 을 어서트하는 케이스를 추가/갱신한다.

### DON'T
- 기존 필드를 제거하지 않는다(deprecate → 다음 major 까지 보존).
- 필드 이름을 in-place rename 하지 않는다 — wire key 와 Rust 식별자 모두.
- `Option<T>` → `T` 로 좁히지 않는다 — 옛 응답에서 누락이 허용되었던 클라이언트가 깨진다.
- 필드 타입을 silent 변환하지 않는다(`String` ↔ `i64`, `String` ↔ `Vec<String>`).
- `serde(rename)` 의 wire key 를 다른 값으로 덮어쓰지 않는다.
