# @clawket/daemon

State layer daemon for [Clawket](https://github.com/clawket/clawket). Hono HTTP server backed by `better-sqlite3` + `sqlite-vec` for local RAG.

## Install

```sh
pnpm install -g @clawket/daemon
clawketd
```

On first run, the daemon:
- writes its port to `$XDG_CACHE_HOME/clawket/clawketd.port`
- creates the SQLite DB under `$XDG_DATA_HOME/clawket/clawket.db`
- applies pending migrations

## Consumed by

- `clawket` CLI — discovers the daemon via the port file, communicates over HTTP.
- `@clawket/web` — React dashboard, served statically under `/` (built artifact bundled into `web/`).
- `@clawket/mcp` — read-only MCP tools that hit the daemon's search endpoints.

## Development

```sh
pnpm install
pnpm start            # runs bin/clawketd.js
```

## Rust migration

A Rust rewrite is in progress in this repo and will eventually replace the Node.js implementation. Releases will continue to ship from this repository; tags bind to whichever implementation is current at release time.

## License

MIT
