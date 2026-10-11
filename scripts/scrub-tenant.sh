#!/usr/bin/env bash
set -euo pipefail

DB_NAME="${MAKO_D1_NAME:-openai-image-proxy}"
BUCKET="${MAKO_R2_BUCKET:-openai-image-proxy-images}"

usage() {
  echo "usage: $0 <app_id> --yes [--local]" >&2
  echo "  Deletes the tenant's R2 objects and stored_images rows and blanks its stored prompts and provider errors." >&2
  echo "  Targets the remote D1 and R2 unless --local is given. Safe to run again." >&2
  exit 2
}

APP_ID=""
CONFIRMED=0
TARGET="--remote"
for arg in "$@"; do
  case "$arg" in
    --yes) CONFIRMED=1 ;;
    --local) TARGET="--local" ;;
    -*) usage ;;
    *) [ -z "$APP_ID" ] && APP_ID="$arg" || usage ;;
  esac
done

[ -n "$APP_ID" ] || usage
[[ "$APP_ID" =~ ^[a-z0-9_-]+$ ]] || { echo "app_id must match [a-z0-9_-]+" >&2; exit 2; }
[ "$CONFIRMED" -eq 1 ] || { echo "refusing to run without --yes" >&2; usage; }

WRANGLER=(npx wrangler)

d1_json() {
  "${WRANGLER[@]}" d1 execute "$DB_NAME" "$TARGET" --json --command "$1"
}

d1_run() {
  "${WRANGLER[@]}" d1 execute "$DB_NAME" "$TARGET" --command "$1" >/dev/null
}

first_value() {
  python3 -I -c '
import json, sys
data = json.load(sys.stdin)
rows = data[0]["results"] if data else []
print(list(rows[0].values())[0] if rows else 0)
'
}

print_counts() {
  local label="$1"
  local images prompts errors
  images=$(d1_json "SELECT COUNT(*) AS n FROM stored_images WHERE app_id = '$APP_ID'" | first_value)
  prompts=$(d1_json "SELECT COUNT(*) AS n FROM usage_records WHERE app_id = '$APP_ID' AND prompt != ''" | first_value)
  errors=$(d1_json "SELECT COUNT(*) AS n FROM usage_records WHERE app_id = '$APP_ID' AND error IS NOT NULL" | first_value)
  echo "$label: stored_images=$images usage_records_with_prompt=$prompts usage_records_with_error=$errors"
}

list_r2_keys() {
  d1_json "SELECT r2_key FROM stored_images WHERE app_id = '$APP_ID'" | python3 -I -c '
import json, sys
data = json.load(sys.stdin)
for row in (data[0]["results"] if data else []):
    print(row["r2_key"])
'
}

echo "scrubbing tenant '$APP_ID' ($TARGET)"
print_counts "before"

KEYS_FILE=$(mktemp)
trap 'rm -f "$KEYS_FILE"' EXIT
list_r2_keys > "$KEYS_FILE"

FAILED=0
DELETED=0
while IFS= read -r key; do
  [ -n "$key" ] || continue
  if "${WRANGLER[@]}" r2 object delete "$BUCKET/$key" "$TARGET" >/dev/null 2>&1; then
    DELETED=$((DELETED + 1))
  else
    FAILED=$((FAILED + 1))
    echo "could not delete R2 object $key" >&2
  fi
done < "$KEYS_FILE"
echo "r2 objects deleted: $DELETED, failed: $FAILED"

if [ "$FAILED" -gt 0 ]; then
  echo "leaving D1 rows in place so a re-run retries the failed objects" >&2
  exit 1
fi

d1_run "UPDATE usage_records SET prompt = '', r2_keys = '[]', error = NULL WHERE app_id = '$APP_ID' AND (prompt != '' OR r2_keys != '[]' OR error IS NOT NULL)"
d1_run "UPDATE stored_images SET prompt = '' WHERE app_id = '$APP_ID' AND prompt != ''"
d1_run "DELETE FROM image_reports WHERE app_id = '$APP_ID' AND image_id IN (SELECT id FROM stored_images WHERE app_id = '$APP_ID')"
d1_run "DELETE FROM stored_images WHERE app_id = '$APP_ID'"

print_counts "after"
