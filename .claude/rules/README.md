# daemon `.claude/rules/`

`clawketd` repo 특화 AI 가드레일. 글로벌 룰(`~/.claude/rules/*.md`) 이 잡지 못하는, 이 repo 의 코드 패턴에 묶인 invariant 만 담는다.

| 룰 | 한 줄 요약 |
|---|---|
| `sse-event-wire-contract.md` | `app.emit(...)` emit 사이트와 `state.rs::parse_event_name` 정적 매핑은 같은 커밋에서 동기. fallback unknown 으로 떨어지는 silent drift 방지. |
| `response-shape-backwards-compat.md` | `models.rs` 의 `Serialize` 구조체와 라우트 응답 JSON 의 필드는 추가만 허용. 기존 필드 제거 / 리네임 / `Option<T>` 좁힘 / `serde(rename)` 변경 금지. |
| `schema-migration-discipline.md` | `SCHEMA_VERSION_MAX`, `MIGRATIONS` 배열, `migrations/` 디렉터리 SQL 의 셋 동기. 과거 마이그레이션 본문 사후 편집 / 순서 변경 / 단독 bump 금지. |
| `error-code-stability.md` | `repo` 의 `bail!("CODE: ...")` prefix 와 `routes/error.rs` 의 매핑은 외부 wire contract. prefix / HTTP status / `code` 문자열 단독 변경 금지. |
| `release-cascade-to-plugin-manifest.md` | `main` push 는 release.yml 을 trigger 해 `clawket/clawket` 에 components.json bump PR 을 자동 생성. 사용자 명시 지시 없이 push 금지. |

## 적용 우선순위
1. 이 디렉터리의 룰
2. 상위 `daemon/CLAUDE.md` 의 `## AI 가드레일 (daemon-local)` 섹션
3. 글로벌 룰(`~/.claude/rules/product-quality-first.md`, `mechanical-overrides.md`, `clawket-context-management.md`)
4. wrapper `clawket/CLAUDE.md`

룰끼리 충돌하면 더 좁은 스코프(이 디렉터리) 가 우선이다.

## 글로벌 승격 트리거
동일 패턴이 ≥ 3 sub-repo 에서 반복되면 글로벌 룰 후보로 분리한다. SSE wire-contract / response-shape backwards-compat 은 daemon + web 양쪽에 존재 — cross-repo 글로벌 룰로 추출 가능성 있음(`.local/rules-inventory-phase1.md` 참조).
