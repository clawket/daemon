# Contributing to `clawket/daemon`

The Clawket state daemon — axum HTTP over a Unix socket, rusqlite + sqlite-vec
for the work-management store, no remote services. The daemon is the single
source of truth for projects, plans, units, cycles, tasks, runs, comments,
artifacts, and envelope contracts (ADR-0001).

## Cross-repo workflow

The cross-repo contribution model (decompose → contract → execute, active-task
gate, PR / commit conventions, Conventional Commits bump policy, Code of Conduct)
is canonical in the meta repo:

- [`clawket/clawket` › `docs/CONTRIBUTING.md`](https://github.com/clawket/clawket/blob/main/docs/CONTRIBUTING.md) — workflow + repo layout + submission rules
- [`clawket/clawket` › `docs/RELEASING.md`](https://github.com/clawket/clawket/blob/main/docs/RELEASING.md) — release order across the seven repos
- [`clawket/clawket` › `CODE_OF_CONDUCT.md`](https://github.com/clawket/clawket/blob/main/CODE_OF_CONDUCT.md) — Contributor Covenant v2.1; reports go to **conduct@clawket.dev**

Do not duplicate those rules here — they live in one place to avoid drift.

## Local setup

```bash
git clone https://github.com/clawket/daemon
cd daemon
rustup toolchain install stable
cargo build                    # debug binary at target/debug/clawketd
./target/debug/clawketd start  # foreground; default socket at $XDG_CACHE_HOME/clawket/clawketd.sock
```

The daemon honors XDG paths — `XDG_DATA_HOME`, `XDG_CACHE_HOME`,
`XDG_CONFIG_HOME`, `XDG_STATE_HOME`. Override with `CLAWKET_DATA_DIR`,
`CLAWKET_CACHE_DIR`, `CLAWKET_CONFIG_DIR`, `CLAWKET_STATE_DIR` for tests.
SQLite migrations under `migrations/` apply on startup.

## Run tests

```bash
cargo test                     # unit + integration (hermetic — uses temp dirs)
cargo clippy --all-targets -- -D warnings   # CI gate
cargo fmt --all -- --check     # CI gate
```

Integration tests under `tests/http_integration.rs` and `tests/smoke_baseline.rs`
spin a daemon on a temp socket and exercise the full HTTP surface. They are
hermetic — no host state is touched.

## Repo-specific PR rules

- Branch off `main`. The release workflow auto-bumps the crate version via
  `cargo set-version --bump` based on Conventional Commit subjects since the
  last tag — **do not edit `Cargo.toml#version` by hand**.
- **Schema migrations are forward-only** and numbered. Add a new file under
  `migrations/<NN>_<name>.sql`, append to the `MIGRATIONS` array in `src/db.rs`,
  and bump `SCHEMA_VERSION_MAX` in the **same commit**. Past migration bodies
  must not be edited (see `.claude/rules/schema-migration-discipline.md`).
- The path-separation invariant (LM-8) is enforced at runtime in
  `src/paths.rs::ensure_no_plugin_overlap`. Do not relax it without an ADR.
- HTTP response shapes (`src/models.rs` `Serialize` structs) are an external
  wire contract — fields can be added (`Option<T>` + `skip_serializing_if`),
  never removed or renamed. See `.claude/rules/response-shape-backwards-compat.md`.
- SSE event names (`app.emit("entity:change", …)`) and the `state.rs::parse_event_name`
  mapping must change together. See `.claude/rules/sse-event-wire-contract.md`.
- Error code prefixes (`bail!("CODE: …")` → HTTP status + `code` JSON field)
  are an external contract. Adding a code requires both `repo` side `bail!` and
  `routes/error.rs` mapping in the same commit. See
  `.claude/rules/error-code-stability.md`.
