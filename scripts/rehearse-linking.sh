#!/usr/bin/env bash
set -uo pipefail

# Rehearses the wallet linking saga (docs/p3.07-wallet-design.md, sections 4 and 7) on a local copy of the production
# D1, before WALLET_LINKING is turned on. It exports production D1 to ~/.config/midgar/backups/, loads the export into a
# throwaway local D1, optionally applies pending migrations while checksumming the balances, links every user to a
# synthetic Midgar ID by running the saga's own code through the scheduled handler of `wrangler dev --local`, and then
# checks the pass criteria: the credits that moved equal the D1 balances that were zeroed, the wallets hold exactly that
# sum less the welcome bonuses the reinstall rule drops, the ledgers of the users did not change, and a second run of
# the cron moves nothing. Nothing is sent to production except the read-only export.
#
#   scripts/rehearse-linking.sh                         export production (needs wrangler logged in; run it on arch)
#   scripts/rehearse-linking.sh --from-sql FILE         use an export you already have, no network
#   scripts/rehearse-linking.sh --make-fixture OUT.sql  write a synthetic export to OUT.sql and stop
#   --migration FILE   apply FILE (a migrations/*.sql not yet in production) before linking; repeatable
#   --group-size N     users per synthetic Midgar ID, across apps in sorted order (default 3)
#   --keep             keep the local state and print where it is

usage() {
  sed -n '/^#   scripts/,/^#   --keep/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PORT="${PORT:-8797}"
BASE="http://127.0.0.1:$PORT"
DB_NAME="openai-image-proxy"
GROUP_SIZE=3
FROM_SQL=""
MAKE_FIXTURE=""
KEEP=0
MIGRATIONS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --from-sql) FROM_SQL="${2:-}"; shift 2 || usage ;;
    --make-fixture) MAKE_FIXTURE="${2:-}"; shift 2 || usage ;;
    --migration) MIGRATIONS+=("${2:-}"); shift 2 || usage ;;
    --group-size) GROUP_SIZE="${2:-}"; shift 2 || usage ;;
    --keep) KEEP=1; shift ;;
    *) usage ;;
  esac
done
[[ "$GROUP_SIZE" =~ ^[1-9][0-9]*$ ]] || usage
[ -z "$FROM_SQL" ] || [ -f "$FROM_SQL" ] || { echo "no such file: $FROM_SQL" >&2; exit 2; }

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/mako-target}"

STATE="$(mktemp -d "${TMPDIR:-/tmp}/mako-rehearsal.XXXXXX")"
CONFIG="$ROOT/wrangler.rehearsal.toml"
DEV_VARS="$ROOT/.dev.vars"
DEV_VARS_BACKUP="$ROOT/.dev.vars.rehearsal-backup"
LOG="$STATE/server.log"
BODY="$STATE/body.json"
SERVER_PID=""
FAILURES=0
STEPS=0

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
  fi
  rm -f "$CONFIG"
  if [ -f "$DEV_VARS_BACKUP" ]; then
    mv "$DEV_VARS_BACKUP" "$DEV_VARS"
  elif [ "${DEV_VARS_WRITTEN:-0}" = 1 ]; then
    rm -f "$DEV_VARS"
  fi
  if [ "$KEEP" -eq 1 ]; then
    echo "kept: $STATE"
  else
    rm -rf "$STATE"
  fi
}
trap cleanup EXIT

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 | cut -d' ' -f1; }
fi

sed -e 's|^command = .*|command = "worker-build --dev"|' wrangler.toml > "$CONFIG"

wr() {
  npx wrangler "$@" --config "$CONFIG" --persist-to "$STATE/d1"
}

sql_file() {
  wr d1 execute "$DB_NAME" --local --file "$1" >/dev/null 2>>"$STATE/wrangler.log"
}

# Production D1 exports a table where its last rebuild left it, so a table can come after rows of other tables whose
# foreign keys name it, which a local D1 refuses ("no such table"). The copy loads with every CREATE TABLE first and
# the rest in the export's own order.
tables_first() {
  python3 -I - "$1" "$2" <<'PY'
import sys
lines = open(sys.argv[1], encoding="utf-8").read().split("\n")
creates, rest, current = [], [], None
for line in lines:
    if current is not None:
        current.append(line)
        if line.rstrip().endswith(");"):
            creates.append("\n".join(current))
            current = None
    elif line.startswith("CREATE TABLE"):
        if line.rstrip().endswith(");"):
            creates.append(line)
        else:
            current = [line]
    else:
        rest.append(line)
pragmas = [l for l in rest if l.startswith("PRAGMA")]
body = [l for l in rest if not l.startswith("PRAGMA")]
open(sys.argv[2], "w", encoding="utf-8").write("\n".join(pragmas + creates + body))
PY
  chmod 600 "$2"
}

# A production export holds the final schema and the rows. Replaying the migrations (ALTER, DROP, RENAME of rebuilt
# tables) is not that shape and `tables_first` would break it, so the synthetic fixture is migrated in SQLite and
# written out as a dump with the deferred foreign keys pragma a D1 export starts with, and without the transaction
# statements D1 refuses.
dump_as_export() {
  python3 -I - "$1" "$2" <<'PY'
import sqlite3, sys
db = sqlite3.connect(":memory:")
db.executescript(open(sys.argv[1], encoding="utf-8").read())
refused = {"BEGIN TRANSACTION;", "COMMIT;"}
dump = ["PRAGMA defer_foreign_keys=TRUE;"] + [line for line in db.iterdump() if line not in refused]
open(sys.argv[2], "w", encoding="utf-8").write("\n".join(dump) + "\n")
PY
  local status=$?
  chmod 600 "$2"
  return $status
}

report_link_failures() {
  grep -aE "wallet (stub|fetch|call|answered)|link resume failed|residue sweep failed" "$LOG" \
    | sed -E 's/\x1b\[[0-9;]*m//g; s/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/<id>/g; s/^[^A-Za-z]*(ERROR[^A-Za-z]*)?//' \
    | sort | uniq -c | sort -rn | head -6 | sed 's/^/        /'
}

rows() {
  wr d1 execute "$DB_NAME" --local --json --command "$1" 2>>"$STATE/wrangler.log" | python3 -I -c '
import json, sys
data = json.load(sys.stdin)
print(json.dumps(data[0]["results"] if data else [], separators=(",", ":"), sort_keys=True))
'
}

value() {
  rows "$1" | python3 -I -c '
import json, sys
rows = json.load(sys.stdin)
print(list(rows[0].values())[0] if rows else "")
'
}

check() {
  local label="$1" expected="$2" actual="$3"
  STEPS=$((STEPS + 1))
  if [ "$expected" = "$actual" ]; then
    echo "PASS  $label"
  else
    echo "FAIL  $label (expected '$expected', got '$actual')"
    FAILURES=$((FAILURES + 1))
  fi
}

ledger_checksum() {
  rows "SELECT app_id,user_id,balance,lifetime_purchased,lifetime_spent FROM user_credits ORDER BY app_id,user_id" | sha256
}

history_checksum() {
  rows "SELECT app_id,user_id,lifetime_purchased,lifetime_spent FROM user_credits ORDER BY app_id,user_id" | sha256
}

users_checksum() {
  rows "SELECT app_id,id FROM users ORDER BY app_id,id" | sha256
}

apply_migrations() {
  local file
  for file in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    local before after
    before="$(ledger_checksum)"
    if ! sql_file "$file"; then
      echo "FAIL  migration $file did not apply"
      tail -5 "$STATE/wrangler.log"
      exit 1
    fi
    after="$(ledger_checksum)"
    check "migration $(basename "$file") leaves every balance as it was" "$before" "$after"
    check "migration $(basename "$file") leaves no foreign key broken" "[]" "$(rows "PRAGMA foreign_key_check")"
  done
}

make_fixture() {
  local out="$1" now sql
  now="2026-10-01T00:00:00+00:00"
  sql="$STATE/fixture-seed.sql"
  python3 -I - "$now" > "$sql" <<'PY'
import sys
now = sys.argv[1]
apps = ["dreameater", "livingdex", "payday", "pixie"]
lines = []
user = 0
for app in apps:
    for n in range(1, 8):
        user += 1
        uid = f"{app}-user-{n:02d}"
        balance = [0, 6, 6, 11, 40, 250, 1][n - 1]
        welcome = 6 if n in (2, 3, 4, 5) else 0
        purchased = max(0, balance - welcome)
        spent = (user * 7) % 23
        lines.append(
            f"INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES ('{uid}', 'anonymous', '{uid}', 'F{user}', '{now}', '{now}', '{app}');"
        )
        lines.append(
            f"INSERT INTO user_credits (app_id, user_id, balance, lifetime_purchased, lifetime_spent, created_at, updated_at) VALUES ('{app}', '{uid}', {balance}, {purchased}, {spent}, '{now}', '{now}');"
        )
        if welcome:
            lines.append(
                f"INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, created_at) VALUES ('t-{uid}', '{app}', '{uid}', 'bonus', {welcome}, {welcome}, 'Welcome bonus', '{now}');"
            )
lines.append(
    f"INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at) VALUES ('p-fixture-pending', 'dreameater', 'dreameater-user-05', 'small', 5, 299, 'revenuecat', 'tx-fixture-pending', 'pending', '{now}');"
)
print("\n".join(lines))
PY
  umask 077
  { cat $(ls migrations/*.sql | sort); cat "$sql"; } > "$STATE/fixture-all.sql"
  if ! dump_as_export "$STATE/fixture-all.sql" "$out"; then
    echo "FAIL  the fixture did not load into SQLite"
    exit 1
  fi
  echo "fixture: $out (the schema after every migration, dumped the way D1 exports it, and $(grep -c '^INSERT INTO users ' "$sql") users)"
}

if [ -n "$MAKE_FIXTURE" ]; then
  make_fixture "$MAKE_FIXTURE"
  exit 0
fi

echo "== loading a copy of the mako database"
if [ -n "$FROM_SQL" ]; then
  DUMP="$FROM_SQL"
else
  umask 077
  BACKUPS="$HOME/.config/midgar/backups"
  mkdir -p "$BACKUPS"
  DUMP="$BACKUPS/mako-d1-$(date +%F)-p307-linking.sql"
  if ! npx wrangler d1 export "$DB_NAME" --remote --output "$DUMP" >/dev/null 2>"$STATE/export.log"; then
    echo "FAIL  the production export failed:"
    sed -E 's#https?://[^ ]+#<url>#g' "$STATE/export.log" | tail -5
    exit 1
  fi
  chmod 600 "$DUMP"
fi
mkdir -p "$STATE/d1"
ORDERED="$STATE/export-tables-first.sql"
tables_first "$DUMP" "$ORDERED"
if ! sql_file "$ORDERED"; then
  echo "FAIL  the export did not load into a local D1"
  sed -E 's#https?://[^ ]+#<url>#g' "$STATE/wrangler.log" | tail -5
  exit 1
fi
check "the copy has wallet_links and wallet_summary, so migration 030 is in" 2 "$(value "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('wallet_links', 'wallet_summary')")"
check "the copy has no wallet state yet, so nothing is linked twice" 0 "$(value "SELECT COUNT(*) FROM wallet_links")"
USERS="$(value "SELECT COUNT(*) FROM users")"
echo "      $USERS users, $(value "SELECT COUNT(*) FROM user_credits") balance rows, $(value "SELECT COALESCE(SUM(balance), 0) FROM user_credits") credits in D1"

apply_migrations

echo "== before linking"
LEDGER_BEFORE="$(ledger_checksum)"
HISTORY_BEFORE="$(history_checksum)"
USERS_BEFORE="$(users_checksum)"
BALANCE_BEFORE="$(value "SELECT COALESCE(SUM(balance), 0) FROM user_credits")"
check "foreign_key_check is clean" "[]" "$(rows "PRAGMA foreign_key_check")"

echo "== queuing a moving link for every user"
SEED="$STATE/links.sql"
cat > "$STATE/links.py" <<'PY'
import json, sys, uuid

group = int(sys.argv[1])
users = json.load(sys.stdin)
for rank, user in enumerate(users):
    midgar = uuid.uuid5(uuid.NAMESPACE_URL, "mako-rehearsal:%d" % (rank // group))
    linked_at = "2025-01-01T00:00:00.%06d+00:00" % rank
    print(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES ('%s', '%s', '%s', 'moving', '%s');"
        % (user["app_id"], user["id"], midgar, linked_at)
    )
PY
rows "SELECT app_id, id FROM users ORDER BY app_id, id" | python3 -I "$STATE/links.py" "$GROUP_SIZE" > "$SEED"
sql_file "$SEED" || { echo "FAIL  the links did not queue"; tail -5 "$STATE/wrangler.log"; exit 1; }
check "every user has a moving link" "$USERS" "$(value "SELECT COUNT(*) FROM wallet_links WHERE state = 'moving'")"
echo "UPDATE credit_purchases SET created_at = strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now') WHERE status = 'pending' AND payment_provider = 'revenuecat';" > "$STATE/young.sql"
sql_file "$STATE/young.sql" || { echo "FAIL  the pending purchases could not be made young"; exit 1; }
echo "      pending RevenueCat purchases in the copy were made young, so the cron leaves them out of the sums"

echo "== starting wrangler dev --local with WALLET_LINKING on"
if [ -f "$DEV_VARS" ]; then
  cp "$DEV_VARS" "$DEV_VARS_BACKUP"
fi
DEV_VARS_WRITTEN=1
printf 'ENVIRONMENT=development\nWALLET_LINKING=on\n' > "$DEV_VARS"
npx wrangler dev --local --test-scheduled --config "$CONFIG" --persist-to "$STATE/d1" --port "$PORT" >"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 300); do
  curl -s -o /dev/null "$BASE/docs" && break
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL  wrangler dev exited early"
    sed -E 's#https?://[^ ]+#<url>#g' "$LOG" | tail -20
    exit 1
  fi
  sleep 1
done
curl -s -o /dev/null "$BASE/docs" || { echo "FAIL  the server did not become ready"; tail -20 "$LOG"; exit 1; }
echo "PASS  server is up"

echo "== running the cron until every link is linked"
RUNS=$(( USERS / 20 + 6 ))
for _ in $(seq 1 "$RUNS"); do
  curl -s -o /dev/null "$BASE/__scheduled?cron=*/30+*+*+*+*"
  [ "$(value "SELECT COUNT(*) FROM wallet_links WHERE state = 'moving'")" = "0" ] && break
done
STILL_MOVING="$(value "SELECT COUNT(*) FROM wallet_links WHERE state = 'moving'")"
if [ "$STILL_MOVING" != "0" ]; then
  echo "      $STILL_MOVING links are still moving; why, from the server log (ids removed):"
  report_link_failures
fi
check "every link finished" "$USERS" "$(value "SELECT COUNT(*) FROM wallet_links WHERE state = 'linked'")"

echo "== pass criteria"
check "SUM(moved_credits) equals the D1 balances before linking" "$BALANCE_BEFORE" "$(value "SELECT COALESCE(SUM(moved_credits), 0) FROM wallet_links")"
check "the D1 balances of the linked users are zeroed" 0 "$(value "SELECT COALESCE(SUM(c.balance), 0) FROM user_credits c JOIN wallet_links l ON l.app_id = c.app_id AND l.user_id = c.user_id")"
EXPECTED_SQL="WITH ranked AS (
  SELECT app_id, midgar_id, moved_credits AS m, welcome_part AS w,
         ROW_NUMBER() OVER (PARTITION BY app_id, midgar_id ORDER BY linked_at, user_id) AS rn
  FROM wallet_links),
expected AS (
  SELECT app_id, midgar_id, SUM(CASE WHEN rn = 1 THEN m ELSE m - MIN(m, COALESCE(w, 0)) END) AS e FROM ranked GROUP BY app_id, midgar_id)"
EXPECTED_TOTAL="$(value "$EXPECTED_SQL SELECT COALESCE(SUM(e), 0) FROM expected")"
echo "      expected in the wallets: $EXPECTED_TOTAL (moved $BALANCE_BEFORE, less the welcome bonuses the reinstall rule drops)"
check "the wallets hold exactly the moved credits less the welcome dedupe" "$EXPECTED_TOTAL" "$(value "SELECT COALESCE(SUM(balance), 0) FROM wallet_summary")"
check "every wallet partition matches its expected balance" 0 "$(value "$EXPECTED_SQL SELECT COUNT(*) FROM expected x LEFT JOIN wallet_summary s ON s.app_id = x.app_id AND s.midgar_id = x.midgar_id WHERE COALESCE(s.balance, -1) != x.e")"
check "no D1 balance was left behind to sweep" 0 "$(value "SELECT COUNT(*) FROM wallet_links WHERE residue_pending IS NOT NULL")"
check "the lifetime ledgers of the users did not change" "$HISTORY_BEFORE" "$(history_checksum)"
check "the set of users did not change" "$USERS_BEFORE" "$(users_checksum)"
check "foreign_key_check is still clean" "[]" "$(rows "PRAGMA foreign_key_check")"

echo "== the cron again moves nothing"
WALLET_AFTER="$(value "SELECT COALESCE(SUM(balance), 0) FROM wallet_summary")"
LINKS_AFTER="$(rows "SELECT app_id, user_id, midgar_id, state, moved_credits, welcome_part FROM wallet_links ORDER BY app_id, user_id" | sha256)"
for _ in 1 2 3; do
  curl -s -o /dev/null "$BASE/__scheduled?cron=*/30+*+*+*+*"
done
check "the wallets hold the same" "$WALLET_AFTER" "$(value "SELECT COALESCE(SUM(balance), 0) FROM wallet_summary")"
check "the links are the same" "$LINKS_AFTER" "$(rows "SELECT app_id, user_id, midgar_id, state, moved_credits, welcome_part FROM wallet_links ORDER BY app_id, user_id" | sha256)"
check "the D1 balances are still zero" 0 "$(value "SELECT COALESCE(SUM(c.balance), 0) FROM user_credits c JOIN wallet_links l ON l.app_id = c.app_id AND l.user_id = c.user_id")"

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "REHEARSAL PASSED: $STEPS CHECKS"
else
  echo "REHEARSAL FAILED: $FAILURES OF $STEPS CHECKS (server log: $LOG, run with --keep to look at it)"
  exit 1
fi
