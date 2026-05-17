#!/usr/bin/env bash
# Migration 024 prep: export archive scope rows to wiki paths, then delete from DB.
# Decision: LM-10807 (option C). Run BEFORE migration 024 lands in db.rs MIGRATIONS.
#
# Usage:
#   bash daemon/scripts/migration-024-archive-export.sh [--dry-run]
#
# Output:
#   - lattice-mono/docs/qa-archive/<id>.md       per archive row (1:1)
#   - lattice-mono/docs/qa-archive/MANIFEST.tsv  rowid + id + type + title + parent_id
#
# Pre-condition: clawketd running.
# Post-condition: SELECT COUNT(*) FROM artifacts WHERE scope='archive' == 0.

set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
fi

DB="${CLAWKET_DB:-$HOME/.local/share/clawket/db.sqlite}"
OUT_DIR="${CLAWKET_ARCHIVE_OUT:-$(pwd)/lattice-mono/docs/qa-archive}"

if [[ ! -f "$DB" ]]; then
  echo "ERROR: clawket DB not found at $DB" >&2
  exit 1
fi

# Stop daemon to avoid concurrent writes during the destructive phase.
# Dry-run skips this — read-only against a live DB is safe.
if [[ "$DRY_RUN" == "0" ]]; then
  if pgrep -x clawketd >/dev/null 2>&1; then
    echo "ERROR: clawketd is running; stop it first (clawket daemon stop) before destructive export" >&2
    exit 2
  fi
fi

mkdir -p "$OUT_DIR"

ARCHIVE_COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM artifacts WHERE scope='archive';")
echo "Archive rows to export: $ARCHIVE_COUNT"

if [[ "$ARCHIVE_COUNT" == "0" ]]; then
  echo "Nothing to export. Exiting."
  exit 0
fi

MANIFEST="$OUT_DIR/MANIFEST.tsv"
: > "$MANIFEST"
echo -e "id\ttype\ttitle\tparent_id\tcreated_at" >> "$MANIFEST"

# Export each archive row to a markdown file. Bash `read -r` with IFS=$'\t'
# collapses adjacent tabs (tab is whitespace IFS), so use a non-whitespace
# sentinel `|` between fields. Sanitize titles to drop any literal pipe.
sqlite3 -separator '|' "$DB" \
  "SELECT id, type, COALESCE(REPLACE(title, '|', '/'), ''), COALESCE(parent_id, ''), COALESCE(created_at, 0) FROM artifacts WHERE scope='archive' ORDER BY created_at;" \
  > "$OUT_DIR/.export-list.txt"

EXPORTED=0
while IFS='|' read -r id type title parent_id created_at; do
  [[ -z "$id" ]] && continue
  safe_id=$(printf '%s' "$id" | tr -c '[:alnum:]_-' '_')
  out_file="$OUT_DIR/${safe_id}.md"
  {
    echo "---"
    echo "id: $id"
    echo "type: $type"
    echo "title: $title"
    echo "parent_id: $parent_id"
    echo "created_at: $created_at"
    echo "scope_at_export: archive"
    echo "exported_by: migration-024-archive-export.sh"
    echo "---"
    echo ""
    sqlite3 "$DB" "SELECT COALESCE(content,'') FROM artifacts WHERE id=?;" -- "$id" 2>/dev/null || true
  } > "$out_file"
  printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$type" "$title" "$parent_id" "$created_at" >> "$MANIFEST"
  EXPORTED=$((EXPORTED + 1))
done < "$OUT_DIR/.export-list.txt"

rm -f "$OUT_DIR/.export-list.txt"

echo "Exported $EXPORTED file(s) to $OUT_DIR"
echo "MANIFEST: $MANIFEST"

if [[ "$EXPORTED" != "$ARCHIVE_COUNT" ]]; then
  echo "ERROR: export count mismatch ($EXPORTED != $ARCHIVE_COUNT)" >&2
  exit 3
fi

if [[ "$DRY_RUN" == "1" ]]; then
  echo "[dry-run] would now: DELETE FROM artifacts WHERE scope='archive';"
  exit 0
fi

# Destructive phase: delete archived rows from DB so migration 024 guard passes.
echo "Deleting $ARCHIVE_COUNT archive row(s) from DB..."
sqlite3 "$DB" <<EOF
BEGIN;
-- Defensive: only delete if export count matches.
DELETE FROM artifact_versions WHERE artifact_id IN (SELECT id FROM artifacts WHERE scope='archive');
DELETE FROM artifacts WHERE scope='archive';
COMMIT;
EOF

REMAINING=$(sqlite3 "$DB" "SELECT COUNT(*) FROM artifacts WHERE scope='archive';")
if [[ "$REMAINING" != "0" ]]; then
  echo "ERROR: post-delete archive count is $REMAINING, expected 0" >&2
  exit 4
fi

echo "OK. archive scope row count is now 0. Migration 024 guard will pass."
