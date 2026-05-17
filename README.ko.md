<!-- 번역 상태: 정본은 README.md (영문). 영문이 갱신되면 docs/i18n-policy.md 의 14d/21d drift 윈도우 안에 본 파일을 동기화한다. -->

[English](README.md)

# clawketd

> **LLM 코딩 에이전트를 위한 구조화된 태스크 계약.**

[Clawket](https://github.com/clawket/clawket) 의 상태 계층 데몬. `rusqlite` + `sqlite-vec` 기반 로컬 RAG 를 가진 `axum` HTTP 서버. 임베딩은 `candle-core` + `paraphrase-multilingual-MiniLM-L12-v2` (384d, 온디바이스).

## 설치

데몬은 [GitHub Releases](https://github.com/clawket/daemon/releases) 에 플랫폼별 바이너리로 배포된다. 실제로는 [`clawket` Claude Code 플러그인](https://github.com/clawket/clawket) 이 데몬을 다운로드하고 자동으로 연결해 주므로, 아래 섹션은 standalone 실행이 필요할 때 참고한다.

지원 타겟:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

첫 실행 시 데몬은 다음을 수행한다:

- `$XDG_CACHE_HOME/clawket/clawketd.port` 에 포트 기록
- `$XDG_CACHE_HOME/clawket/clawketd.sock` Unix socket bind
- `$XDG_DATA_HOME/clawket/db.sqlite` SQLite DB 생성
- (바이너리에 임베드된) pending 마이그레이션 적용
- **path-separation invariant** 강제: data / cache / config / state / db 경로가 `~/.claude/plugins/` 와 겹치면 기동 거부 (`CLAWKET_ALLOW_PLUGIN_OVERLAP=1` 로만 우회)

## v3.0 baseline

| 표면 | 동작 |
|---|---|
| 스키마 | `task.evidence` 컬럼 + `EVIDENCE_REQUIRED` 하드 enforcement. 현재 `SCHEMA_VERSION_MAX` 는 `daemon/src/db.rs` 에 핀됨. |
| `PATCH /tasks/:id` → `done` | `evidence` 가 비어있으면 **HTTP 400** 반환 (file:line 또는 추론 요약). |
| Knowledge 표면 | `/knowledge/*` 라우트가 `knowledge:created \| updated \| deleted` SSE 이벤트를 발행. |
| Tier 라우팅 | 태스크는 `tier ∈ {low, med, high}` 를 가지며, v3 에서 다운그레이드는 advisory (경고만), v4 에서 hard-enforce. |
| Auto-cascade | Task → `done`/`cancelled` 시 Unit / Plan / Cycle 의 모든 자식이 terminal 이면 terminal 로 cascade. |
| Auto-embedding | Knowledge + task 가 create/update 시 임베드; 데몬 startup 에서 누락분 백필. |
| TCP auth | 모든 비-로컬 요청에 `X-Clawket-Token` 헤더 또는 `clawket_session` HttpOnly 쿠키 (데몬이 `/` 를 서빙할 때만 발급) 필요. Unix socket 은 auth 우회. |

## 사용 주체

- `clawket` CLI — 포트 파일로 데몬을 디스커버하고 HTTP 로 통신.
- `clawket mcp` — 같은 `clawket` 바이너리 안의 내장 MCP stdio 서버; 데몬의 read-only knowledge 엔드포인트를 HTTP 로 호출.
- 웹 대시보드 (`clawket/web`) — React 19 SPA. 데몬이 번들된 `web/dist/` 를 `/` 아래 정적 서빙. GitHub Release tarball 로 배포되며 npm 패키지가 아니다.

## 개발

```sh
cargo run -- --port 0           # 포트 자동 할당
cargo test                      # unit + smoke 테스트
cargo build --release           # target/release/clawketd 산출
```

크로스 컴파일 릴리즈 아티팩트는 `.github/workflows/release.yml` 이 `main` push 시 생성한다 (Conventional Commit 으로부터 버전 자동 bump → 빌드 → GitHub Releases 게시).

## 호환성

데몬 ↔ CLI ↔ web ↔ 플러그인 호환성은 플러그인의 `components.json` 으로 핀된다. breaking 스키마 변경은 세 컴포넌트의 coordinated 릴리즈가 필요하다 — [`clawket/clawket → docs/COMPATIBILITY.md`](https://github.com/clawket/clawket/blob/main/docs/COMPATIBILITY.md) 참조.

## 기여

> *분해, 계약, 실행 — 구조화된 에이전트 루프.*

Clawket 에 기여하는 모든 작업 (이 데몬 포함) 은 세 단계를 순서대로 거친다: **분해** (작업을 태스크 트리로 쪼갬), **각 leaf 에 계약 서명** (19 필드 실행 envelope), **계약 대비 실행**. 플러그인 shell 의 `PreToolUse` 훅이 1–2 단계를 거치지 않은 3 단계를 하드 블록한다.

전체 가이드: [clawket/clawket → docs/CONTRIBUTING.md](https://github.com/clawket/clawket/blob/main/docs/CONTRIBUTING.md).

## 라이선스

MIT
