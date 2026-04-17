# clawketd-rs (Phase 6 migration)

Pre-release Rust scaffold of the Clawket daemon. The Node daemon (`bin/clawketd.js`) is
authoritative until feature parity lands here.

## Status

| Subsystem | Node | Rust |
|---|---|---|
| HTTP server | yes | stub (`/health`) |
| SQLite (better-sqlite3 / rusqlite) | yes | — |
| sqlite-vec | yes | — |
| Migrations | yes | — |
| Project/Plan/Unit/Task API | yes | — |
| MCP bridge | yes | — |

## Run

```sh
cargo run --manifest-path rust/Cargo.toml
```
