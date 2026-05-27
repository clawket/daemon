# clawketd — AI 컨텍스트

Clawket 의 **상태 계층 데몬**. Axum HTTP + Unix socket 으로 CLI / 웹 대시보드 / 내장 MCP 가 공유하는 SSoT 를 제공한다. SQLite (`rusqlite` + `sqlite-vec`) 가 단일 저장소, 임베딩은 `candle-core` 로 온디바이스에서 생성한다. 이 데몬은 인증·계정·원격 동기화·멀티 테넌트를 다루지 않으며, **로컬 단일 사용자** 전용이다.

## Stack

| Component | Version | Source |
|---|---|---|
| `axum` (HTTP framework) | 0.8 | `Cargo.toml:17` |
| `tokio` (async runtime) | 1.x (macros, rt-multi-thread, signal, net, sync, time) | `Cargo.toml:18` |
| `rusqlite` (SQLite driver) | 0.32 (bundled, load_extension) | `Cargo.toml:29` |
| `sqlite-vec` (vector ext) | 0.1 | `Cargo.toml:32` |
| `r2d2` / `r2d2_sqlite` (pool) | 0.8 / 0.25 | `Cargo.toml:30-31` |
| `candle-core` / `candle-nn` / `candle-transformers` | 0.8 | `Cargo.toml:35-37` |
| `tokenizers` | 0.21 (onig) | `Cargo.toml:38` |
| `tower-http` | 0.6 (trace, cors) | `Cargo.toml:24` |
| `clap` | 4 (derive) | `Cargo.toml:28` |
| Crate / binary name | `clawketd` (version `0.3.3`) | `Cargo.toml:2-3,12-14` |

Embedding model: `paraphrase-multilingual-MiniLM-L12-v2` (384d).

## Layered architecture

```
src/main.rs              # CLI entry (clap) → start / stop / status / restart
  ├── config.rs          # CLI args (port 19400, host 127.0.0.1 default)
  ├── paths.rs           # XDG path resolution + LM-8 invariant
  ├── state.rs           # AppState: DB pool, SSE broadcast, embedder handle
  ├── middleware/        # tcp_auth.rs (X-Clawket-Token / cookie + Origin guard)
  ├── routes/            # axum Router (HTTP surface; see routes/mod.rs)
  ├── repo/              # SQL-facing domain modules (tasks, plans, cycles, units,
  │                      #   artifacts, comments, runs, …)
  ├── db.rs              # rusqlite open + embedded migration runner
  ├── embeddings.rs      # candle-based embedder + sqlite-vec wiring
  ├── envelope/          # task envelope contract (ADR-0001)
  ├── decomposition/     # plan → unit → task decomposition helpers
  ├── jobs/              # background workers (embedding backfill, cascade)
  └── locale.rs / secrets/ / id.rs / git.rs / import_plan.rs / models.rs
migrations/              # 001…025 SQL, embedded in binary
```

Request flow: **`routes/*` (HTTP) → `repo/*` (SQL) → `db.rs` (pool)**. SSE 이벤트는 `repo` 가 변경을 끝낸 직후 `routes` 에서 `app.emit("<entity>:<change>", json)` 호출, `state.rs` 가 정적 mapping 으로 broadcast 한다. 두 리스너 (TCP / Unix socket) 가 동일한 `Router` 를 공유하며 TCP 만 `middleware::tcp_auth_layer` 가 추가로 감싼다.

## Critical invariants

| Invariant | Mechanism | Evidence |
|---|---|---|
| `EVIDENCE_REQUIRED` — `task.status=done` 전환 시 비어있지 않은 `evidence` 필수 | `repo::tasks::update` 가 `bail!("EVIDENCE_REQUIRED: …")`, `routes::error` 가 HTTP 400 + code 로 변환 | `src/repo/tasks.rs:654-658`, `src/routes/error.rs:204-206` |
| `SCHEMA_VERSION_MAX` 는 **하나의 상수**가 단일 진실 | `pub const SCHEMA_VERSION_MAX: i64 = 26;` — migrate 함수가 동일 상수만 참조 | `src/db.rs:16` (헤드 비교 `:286,:442`) |
| Path separation (LM-8): data / cache / config / state / db 가 `~/.claude/plugins/` 하위로 잡히면 데몬 기동 거부 | `Paths::resolve()` 가 다섯 경로 모두에 `ensure_no_plugin_overlap` 실행. `CLAWKET_ALLOW_PLUGIN_OVERLAP=1` 로만 우회 가능 (데이터 손실 인지 의미) | `src/paths.rs:57-62`, 검사 함수 `:367` |
| `/knowledge/*` 라우트가 정본 표면 — SSE 이벤트 `knowledge:{created,updated,deleted}` 발행 | 라우트 파일은 `routes/artifacts.rs` 에 위치 (`/knowledge` mount, `app.emit("knowledge:created/updated/deleted", …)`). 정적 mapping 은 `state.rs` | `src/routes/artifacts.rs:16-23,117,134,189`, `src/state.rs:182-184` |
| TCP 리스너는 **반드시** `X-Clawket-Token` 헤더 또는 `clawket_session` HttpOnly 쿠키 + mutating 요청에 Origin/Referer 가드. Unix socket 은 인증 면제 (local = trusted). `CLAWKET_TCP_AUTH=0` 로만 무력화 가능 | `middleware::tcp_auth::tcp_auth_layer`, 토큰은 `~/.cache/clawket/clawketd.token` 에 기동 시 재생성 | `src/middleware/tcp_auth.rs:25,34-99` |
| 공개 바인드 금지 (`PUBLIC_BIND_NOT_ALLOWED`) — 기본 host `127.0.0.1` | `main.rs` 의 host 가드, `routes/error.rs` 가 코드 매핑 | `src/main.rs:131-138`, `src/routes/error.rs:207-208`, 기본값 `src/config.rs:34-35` |
| 기본 포트 `19400`, 점유 시 +1 (최대 20), `0` = OS 할당 | clap `default_value_t = 19400` | `src/config.rs:28-31` |

## XDG paths (사용자 데이터)

| 영역 | 위치 | 환경 변수 우선순위 |
|---|---|---|
| Data (SQLite `db.sqlite`) | `~/.local/share/clawket/` | `CLAWKET_DB` > `CLAWKET_DATA_DIR` > `XDG_DATA_HOME` |
| Cache (socket, pid, port, token) | `~/.cache/clawket/` (`clawketd.sock`, `clawketd.pid`, `clawketd.port`, `clawketd.token`) | `CLAWKET_CACHE_DIR` > `XDG_CACHE_HOME`; socket: `CLAWKET_SOCKET` |
| Config | `~/.config/clawket/` | `CLAWKET_CONFIG_DIR` > `XDG_CONFIG_HOME` |
| State (logs) | `~/.local/state/clawket/` | `CLAWKET_STATE_DIR` > `XDG_STATE_HOME` |

근거: `src/paths.rs:38-78`. `Paths::ensure_dirs` 가 기동 시 data/cache/config/state 네 디렉터리를 생성한다 (`:80-87`).

## Build / test / run

```bash
cargo build                           # target/debug/clawketd
cargo build --release                 # target/release/clawketd
cargo test                            # 유닛 + smoke (tests/ 하위)
cargo clippy --all-targets            # lint
cargo fmt --check                     # 포맷 검증

cargo run -- start --port 0           # 포어그라운드, OS-할당 포트
cargo run -- status                   # pid/port 파일 + /health 프로브
cargo run -- stop
cargo run -- restart --port 19400
```

릴리스 워크플로 `.github/workflows/release.yml` 이 conventional commit 기준으로 버전 bump → 크로스 컴파일 → GitHub Releases 게시. CI 는 `.github/workflows/ci.yml`.

| Env override | 효과 |
|---|---|
| `CLAWKET_ALLOW_PLUGIN_OVERLAP=1` | LM-8 invariant 무력화 (테스트 전용; 데이터 손실 위험을 인지한 것으로 간주) |
| `CLAWKET_TCP_AUTH=0` | TCP 토큰 인증 비활성화 (테스트 전용) |
| `CLAWKETD_LOG=debug,clawketd=trace` | tracing 레벨 |
| `CLAWKETD_LOG_FORMAT=json` | 1줄 1객체 JSON 로그 |
| `CLAWKET_DEBUG=1` | 기본 레벨 `debug` + 에러 응답에 stack trace 포함 |
| `CLAWKETD_METRICS=1` | `/metrics` Prometheus 엔드포인트 노출 |

## Cross-repo 좌표

릴리스 순서, 컴포넌트 핀 버전 (`components.json`), 플러그인 install gate (`ensureInstalled`), 호환성 매트릭스, 훅 enforcement 설계, i18n / vendor 정책은 모두 wrapper repo (`github.com/clawket/clawket`) 의 정본 문서가 관리한다:

- `clawket/CLAUDE.md` — wrapper 운영 규칙 + 컴포넌트 좌표
- `clawket/docs/RELEASING.md` — 릴리스 순서·체크리스트
- `clawket/docs/COMPATIBILITY.md` — daemon ↔ CLI ↔ web ↔ plugin 버전 범위
- `clawket/docs/HOOK_ENFORCEMENT.md` — MCP 기반 훅 enforcement 설계
- `clawket/components.json` — daemon 의 핀 버전 (v3 baseline)

위 내용은 이 파일에서 중복하지 않는다.

## AI 가드레일 (daemon-local)

- 명시적 지시 없이 커밋/푸시하지 않는다.
- 편집 전 후보 파일을 다시 읽는다. 편집 후 `cargo check` (또는 `cargo build`) 로 컴파일 확인. 보고 전 `cargo test` + `cargo clippy --all-targets`.
- `EVIDENCE_REQUIRED`, LM-8 path invariant, TCP auth, `PUBLIC_BIND_NOT_ALLOWED` 가드는 **사용자가 명시적으로 변경 지시하지 않는 한 우회·약화 금지**. 우회용 env (`CLAWKET_ALLOW_PLUGIN_OVERLAP`, `CLAWKET_TCP_AUTH=0`) 를 새 코드에서 디폴트로 깔지 않는다.
- `/knowledge/*` 라우트를 변경하면 **같은 커밋에서** `src/state.rs` 의 `knowledge:*` SSE 이벤트 mapping (`:182-184`) 을 함께 갱신한다. 이벤트명은 CLI / 웹이 의존하는 wire contract.
- `SCHEMA_VERSION_MAX` 를 올릴 때는 반드시 `migrations/` 에 새 SQL 파일을 추가하고, `db.rs:16` 의 상수와 head 비교 (`:286,:442`) 가 새 번호로 일치하는지 확인.
- 새 라우트는 `routes/mod.rs::router()` 에 merge 하지 않으면 노출되지 않는다.
- Unix socket 핸들러는 인증 면제 경로다 — 그쪽에 추가로 권한 분기를 박지 않는다 (local = trusted 가정 유지).
