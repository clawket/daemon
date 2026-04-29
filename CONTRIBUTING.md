# Contributing to `clawket/daemon`

The Clawket state daemon — axum HTTP over a Unix socket, rusqlite + sqlite-vec
for the work-management store, no remote services. The daemon is the single
source of truth for projects, plans, units, cycles, tasks, runs, comments,
artifacts, and envelope contracts (ADR-0001).

## Local setup

```bash
git clone https://github.com/clawket/daemon
cd daemon
rustup toolchain install stable
cargo build                    # debug binary at target/debug/clawketd
./target/debug/clawketd        # foreground, default socket at $XDG_CACHE_HOME/clawket/clawketd.sock
```

The daemon honors XDG paths — `XDG_DATA_HOME`, `XDG_CACHE_HOME`,
`XDG_CONFIG_HOME`, `XDG_STATE_HOME`. Override with `CLAWKET_DATA_DIR` etc.
for tests. SQLite migrations under `migrations/` apply on startup.

## Run tests

```bash
cargo test                     # unit + integration (uses temp dirs)
cargo clippy -- -D warnings    # CI gate
cargo fmt --check              # CI gate
```

Integration tests under `tests/http_integration.rs` and `tests/smoke_baseline.rs`
spin a daemon on a temp socket and exercise the full HTTP surface. They are
hermetic — no host state is touched.

## Pull requests

- Branch off `main`. The release workflow auto-bumps SemVer from Conventional
  Commit prefixes (`feat:` / `fix:` / `perf:` only); do not bump
  `Cargo.toml` by hand.
- Schema changes get a forward-only numbered migration in `migrations/` —
  rollbacks are not supported. The daemon runs every pending migration on
  startup, so the file ordering matters.
- Path-separation invariant (LM-8) is enforced at runtime in
  `src/paths.rs::ensure_no_plugin_overlap`. Do not relax it without an ADR.

## Commit convention

Conventional Commits. Release-worthy: `feat:` (minor), `fix:` / `perf:`
(patch), `feat!:` or `BREAKING CHANGE:` body (major). All other prefixes
ship via the next release-worthy commit.

## Roadmap

See [`ROADMAP.md`](./ROADMAP.md) for the multi-repo milestones. Daemon-
specific schemas + ADRs live under `docs/` and `schemas/`.
