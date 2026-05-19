# clawketd

> **Structured task contracts for LLM coding agents.**

State-layer daemon for [Clawket](https://github.com/clawket/clawket). `axum` HTTP server backed by `rusqlite` + `sqlite-vec` for local RAG. Embeddings via `candle-core` + `paraphrase-multilingual-MiniLM-L12-v2` (384d, on-device).

## Install

The daemon is distributed as a platform-specific binary on [GitHub Releases](https://github.com/clawket/daemon/releases). In practice, the [`clawket` Claude Code plugin](https://github.com/clawket/clawket) downloads and wires up the daemon for you; the sections below are for running it standalone.

Supported targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

On first run, the daemon:

- writes its port to `$XDG_CACHE_HOME/clawket/clawketd.port`
- binds a Unix socket at `$XDG_CACHE_HOME/clawket/clawketd.sock`
- creates the SQLite DB under `$XDG_DATA_HOME/clawket/db.sqlite`
- applies pending migrations (embedded in the binary)
- enforces the **path-separation invariant**: refuses to start if data/cache/config/state/db paths overlap with `~/.claude/plugins/` (override only via `CLAWKET_ALLOW_PLUGIN_OVERLAP=1`)

## Plugin v3.0 baseline

The "v3.0" below refers to the **plugin contract version** ([`clawket/clawket`](https://github.com/clawket/clawket)) that this daemon must satisfy — not the daemon binary's own version (see `Cargo.toml`).


| Surface | Behavior |
|---|---|
| Schema | `task.evidence` column with `EVIDENCE_REQUIRED` hard enforcement. Current `SCHEMA_VERSION_MAX` is pinned in `daemon/src/db.rs`. |
| `PATCH /tasks/:id` to `done` | Returns **HTTP 400** unless `evidence` is non-empty (file:line or reasoning summary). |
| Knowledge surface | `/knowledge/*` routes emit `knowledge:created \| updated \| deleted` SSE events. |
| Tier routing | Tasks carry `tier ∈ {low, med, high}`; downgrades are advisory (warning only) in v3 and become hard-enforced in v4. |
| Auto-cascade | Task → `done`/`cancelled` cascades terminal state to Unit / Plan / Cycle when all children are terminal. |
| Auto-embedding | Knowledge entries + tasks embedded on create/update; missing embeddings backfilled at daemon startup. |
| TCP auth | All non-local requests require `X-Clawket-Token` header or `clawket_session` HttpOnly cookie (issued only on daemon-served `/`). Unix socket bypasses auth. |

## Consumed by

- `clawket` CLI — discovers the daemon via the port file, communicates over HTTP.
- `clawket mcp` — the embedded MCP stdio server inside the same `clawket` binary; hits the daemon's read-only knowledge endpoints over HTTP.
- Web dashboard (`clawket/web`) — React 19 SPA. The daemon serves the bundled `web/dist/` statically under `/`. Distributed as a GitHub Release tarball, not from npm.

## Development

```sh
cargo run -- --port 0           # auto-assigned port
cargo test                      # unit + smoke tests
cargo build --release           # production binary at target/release/clawketd
```

Cross-compiled release artifacts are produced by `.github/workflows/release.yml` on push to `main` (auto-bumps version from conventional commits, then builds and publishes to GitHub Releases).

## Compatibility

Daemon ↔ CLI ↔ web ↔ plugin compatibility is pinned by the plugin's `components.json`. Breaking schema changes require coordinated release across all three components — see [`clawket/clawket → docs/COMPATIBILITY.md`](https://github.com/clawket/clawket/blob/main/docs/COMPATIBILITY.md).

## Contributing

> *Decompose, contract, execute — the structured agent loop.*

Every contribution to Clawket — including this daemon — moves through three steps in order: **decompose** the work into a task tree, **sign each leaf with a contract** (the 19-field execution envelope), then **execute against the contract**. The `PreToolUse` hook in the plugin shell hard-blocks step 3 if steps 1–2 weren't done.

Full guide: [clawket/clawket → docs/CONTRIBUTING.md](https://github.com/clawket/clawket/blob/main/docs/CONTRIBUTING.md).

## License

MIT
