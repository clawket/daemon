#!/usr/bin/env bash
# Release smoke test: boot the daemon binary against a throwaway DB,
# hit /health and a handful of parity endpoints, then shut down.
#
# Targets: intended to run in CI across {macos-x64, macos-arm64, linux-x64, linux-arm64}.
# Locally this verifies the host binary in target/release/clawketd.
#
# Exit 0 on pass, nonzero on any failure.

set -euo pipefail

BIN="${CLAWKETD_BIN:-target/release/clawketd}"
if [[ ! -x "$BIN" ]]; then
  echo "[smoke] binary not found or not executable: $BIN" >&2
  echo "[smoke] hint: cargo build --release" >&2
  exit 1
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"; [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null || true' EXIT

export CLAWKET_DATA_DIR="$TMPDIR/data"
export CLAWKET_CACHE_DIR="$TMPDIR/cache"
export CLAWKET_CONFIG_DIR="$TMPDIR/config"
export CLAWKET_STATE_DIR="$TMPDIR/state"
export CLAWKETD_LOG=warn

mkdir -p "$CLAWKET_DATA_DIR" "$CLAWKET_CACHE_DIR" "$CLAWKET_CONFIG_DIR" "$CLAWKET_STATE_DIR"

"$BIN" --port 0 --db "$TMPDIR/data/test.sqlite" >"$TMPDIR/daemon.log" 2>&1 &
DAEMON_PID=$!

# Wait for port file
for _ in $(seq 1 50); do
  if [[ -s "$CLAWKET_CACHE_DIR/clawketd.port" ]]; then
    break
  fi
  sleep 0.1
done

PORT="$(cat "$CLAWKET_CACHE_DIR/clawketd.port" | tr -d '\n')"
if [[ -z "$PORT" ]]; then
  echo "[smoke] daemon failed to write port file" >&2
  echo "--- daemon.log ---" >&2
  cat "$TMPDIR/daemon.log" >&2
  exit 1
fi

BASE="http://127.0.0.1:$PORT"
echo "[smoke] daemon bound to $BASE (pid $DAEMON_PID)"

fail() { echo "[smoke] FAIL: $1" >&2; exit 1; }

pass() { echo "[smoke] ok: $1"; }

# /health
resp="$(curl -fsS "$BASE/health")" || fail "GET /health"
echo "$resp" | grep -q '"status":"ok"' || fail "/health did not return ok"
echo "$resp" | grep -q '"engine":"rust"' || fail "/health engine mismatch"
pass "/health"

# /agents
curl -fsS "$BASE/agents" >/dev/null || fail "GET /agents"
pass "/agents"

# /handoff (no project)
resp="$(curl -fsS "$BASE/handoff")" || fail "GET /handoff"
echo "$resp" | grep -q 'No project found' || fail "/handoff empty case"
pass "/handoff"

# /dashboard (no project)
resp="$(curl -fsS "$BASE/dashboard")" || fail "GET /dashboard"
echo "$resp" | grep -q '"project":null' || fail "/dashboard empty case"
pass "/dashboard"

# / (static SPA fallback — 503 when no web/dist)
http_code="$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")"
if [[ "$http_code" != "200" && "$http_code" != "503" ]]; then
  fail "GET / returned unexpected $http_code"
fi
pass "/ (status $http_code)"

# /projects create+list round-trip
curl -fsS -X POST "$BASE/projects" -H "Content-Type: application/json" \
  -d '{"name":"smoke-proj","key":"SMK"}' >/dev/null || fail "POST /projects"
count="$(curl -fsS "$BASE/projects" | grep -o '"id"' | wc -l | tr -d ' ')"
[[ "$count" -ge 1 ]] || fail "GET /projects count"
pass "/projects round-trip"

echo "[smoke] all checks passed"
