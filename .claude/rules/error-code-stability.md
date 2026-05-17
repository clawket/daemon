# error-code-stability

## Purpose
`routes::error::From<anyhow::Error>` 는 `repo` 가 던진 `bail!("<CODE>: ...")` 문자열의 prefix 를 매칭해서 HTTP status + 구조화된 `code` 필드로 변환한다. prefix 문자열과 HTTP status 는 외부 wire contract (CLI hook 강제 / 웹 토스트 / MCP 응답) 의 일부이며, 매칭 누락은 HTTP 500 fallthrough 를 의미한다.

## Prevents
- `repo` 측에서 `bail!("EVIDENCE_REQUIRED: ...")` 를 `bail!("evidence required: ...")` 같이 리네임해 prefix 매칭이 무력화되고 HTTP 500 으로 빠지는 케이스.
- `error.rs` 의 분기를 정리하다가 `EVIDENCE_REQUIRED` / `PUBLIC_BIND_NOT_ALLOWED` 같은 user-data integrity 코드를 제거해 hook gate 가 약해지는 케이스.
- prefix 는 유지한 채 HTTP status 만 400 → 422 같이 silent 변경해 CLI 측 retry / 분기 로직이 깨지는 케이스.
- 새 도메인 에러를 추가하면서 매핑을 잊어 generic heuristic(`msg.contains("Invalid")`) 으로 떨어지는 케이스.

## Evidence
- `src/routes/error.rs:169-280` — `impl From<anyhow::Error> for ApiError` 본문. `msg.starts_with("<CODE>:")` prefix 매칭 사다리(177-252) 와 generic fallback(264-279).
- `src/routes/error.rs:204-206` — `EVIDENCE_REQUIRED` 는 `bad_request_coded` (HTTP 400 + `code: "EVIDENCE_REQUIRED"`).
- `src/routes/error.rs:207-208` — `PUBLIC_BIND_NOT_ALLOWED` 동일.
- `daemon/CLAUDE.md:109` — "`EVIDENCE_REQUIRED`, LM-8 path invariant, TCP auth, `PUBLIC_BIND_NOT_ALLOWED` 가드는 사용자가 명시적으로 변경 지시하지 않는 한 우회·약화 금지" 라는 invariant 본문.

## Why not global
글로벌 룰은 wire contract 안정성을 다루지만, daemon 은 "anyhow 메시지 prefix 매칭 → 구조화된 code" 라는 특이 패턴을 쓴다. `repo` 모듈에서 `bail!` 문자열이 wire contract 가 된다는 사실은 이 repo 의 코드 컨벤션이고, 다른 sub-repo 에서는 의미가 없다.

## Enforcement gap
- `repo` 측에 새 에러 prefix 가 추가되었을 때 `error.rs` 매핑이 누락된 것을 컴파일 타임에 잡는 매크로/enum 이 없다.
- prefix 와 HTTP status 의 짝을 검증하는 통합 테스트가 부분적이다 — generic heuristic 으로 떨어진 케이스는 silent 하게 400 OK 가 된다.
- CLI / web 이 의존하는 `code` 문자열 집합이 코드 상에 enum 으로 박혀 있지 않아 (`code: Some("EVIDENCE_REQUIRED".to_string())`) 오타가 detect 되지 않는다.

## Rule body
도메인 에러 추가 / 변경 시 다음을 지킨다.

### DO
- 새 도메인 에러는 `repo` 측에서 `bail!("UPPER_SNAKE_CODE: human readable")` 포맷으로 던지고, 같은 커밋에서 `routes/error.rs` 의 prefix 매칭 사다리에 `ApiError::bad_request_coded("UPPER_SNAKE_CODE", msg)` 등 명시적 분기를 추가한다.
- HTTP status 매핑(`bad_request_coded` 400 / `not_found_coded` 404 / `conflict_coded` 409)은 의미에 맞게 한 번 결정하고 이후 변경하지 않는다.
- 신규 코드는 `tests/` 하위에 prefix → HTTP status → `code` 필드 짝을 어서트하는 케이스를 추가한다.
- prefix 문자열은 `UPPER_SNAKE` 컨벤션을 유지한다.

### DON'T
- 기존 prefix(`EVIDENCE_REQUIRED:`, `PUBLIC_BIND_NOT_ALLOWED:`, `TASK_NOT_FOUND:` 등)를 rename / 삭제하지 않는다 — CLI / web / MCP 가 직접 매칭한다.
- HTTP status 매핑을 in-place 변경하지 않는다 (400 → 422 등).
- 새 에러를 generic heuristic(`msg.contains("Invalid")`) 에 의존해 처리하지 않는다 — 명시 분기를 추가한다.
- `daemon/CLAUDE.md` 가 명시한 invariant 가드 코드(EVIDENCE_REQUIRED, PUBLIC_BIND_NOT_ALLOWED 등) 의 status 를 사용자 지시 없이 약화하지 않는다.
