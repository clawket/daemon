# clawketd

State-layer daemon for [Clawket](https://github.com/clawket/clawket). Axum HTTP server backed by `rusqlite` + `sqlite-vec` for local RAG. Embeddings via `candle-core` + all-MiniLM-L6-v2.

## Install

The daemon is distributed as a platform-specific binary on [GitHub Releases](https://github.com/clawket/daemon/releases). In practice, the [`clawket` Claude Code plugin](https://github.com/clawket/clawket) downloads and wires up the daemon for you; the sections below are for running it standalone.

Supported targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

On first run, the daemon:

- writes its port to `$XDG_CACHE_HOME/clawket/clawketd.port`
- creates the SQLite DB under `$XDG_DATA_HOME/clawket/db.sqlite`
- applies pending migrations (embedded in the binary)

## Consumed by

- `clawket` CLI — discovers the daemon via the port file, communicates over HTTP.
- `clawket mcp` — the embedded MCP stdio server inside the same `clawket` binary; hits the daemon's read-only search endpoints over HTTP.
- `@clawket/web` — React dashboard, served statically under `/` (built artifact bundled into `web/dist/`).

> The legacy `@clawket/mcp` Node stdio server is no longer in the chain — replaced by the embedded `clawket mcp` in plugin v2.3.2 and scheduled for archive in plugin v11 U4.

## Development

```sh
cargo run -- --port 0
```

Cross-compiled release artifacts are produced by `.github/workflows/release.yml` on push to `main` (auto-bumps version from conventional commits, then builds and publishes to GitHub Releases).

## License

MIT
