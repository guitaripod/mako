#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

PORT="${PORT:-8798}"
BASE="http://127.0.0.1:$PORT"
STATE="$(mktemp -d "${TMPDIR:-/tmp}/wallet-smoke-state.XXXXXX")"
CONFIG="$ROOT/wrangler.wallet-smoke.toml"
LOG="$(mktemp "${TMPDIR:-/tmp}/wallet-smoke-log.XXXXXX")"
BODY="$(mktemp "${TMPDIR:-/tmp}/wallet-smoke-body.XXXXXX")"
SERVER_PID=""
FAILURES=0
STEPS=0

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
  fi
  rm -rf "$CONFIG" "$BODY" "$STATE"
}
trap cleanup EXIT

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

json_get() {
  python3 -I -c '
import json, sys
value = json.load(open(sys.argv[2]))
try:
    for part in sys.argv[1].split("/"):
        value = value[int(part)] if isinstance(value, list) else value[part]
    print("null" if value is None else json.dumps(value) if isinstance(value, (dict, list, bool)) else value)
except Exception:
    print("MISSING")
' "$1" "$BODY"
}

call() {
  local method="$1" id="$2" path="$3" body="${4:-}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X "$method" "$BASE/$id$path")
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  STATUS=$(curl "${args[@]}")
}

check() {
  local label="$1" expected="$2" actual="$3"
  STEPS=$((STEPS + 1))
  if [ "$expected" = "$actual" ]; then
    echo "PASS  $label"
  else
    echo "FAIL  $label (expected '$expected', got '$actual')"
    echo "      body: $(head -c 300 "$BODY")"
    FAILURES=$((FAILURES + 1))
  fi
}

entry() {
  printf '{"key":"%s","tenant":"%s","kind":"%s","amount":%s,"source":"smoke","reference":"ref:%s","description":"smoke"}' "$1" "$2" "$3" "$4" "$1"
}

echo "== building the worker and starting wrangler dev --local"
sed -e 's|^command = .*|command = "worker-build --dev"|' -e 's|^main = .*|main = "scripts/wallet-smoke/worker.mjs"|' wrangler.toml > "$CONFIG"
npx wrangler dev --local --config "$CONFIG" --persist-to "$STATE" --port "$PORT" >"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 300); do
  if curl -s -o /dev/null "$BASE/ready/balance?tenant=t"; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL  wrangler dev exited early"
    tail -30 "$LOG"
    exit 1
  fi
  sleep 1
done
call GET ready "/balance?tenant=t"
check "the object answers" 200 "$STATUS"

ID="midgar-id-a"
echo "== grant and replay"
call POST $ID /grant "$(entry tx:1 dreameater purchase 10)"
check "grant is applied" 200 "$STATUS"
check "grant outcome" applied "$(json_get outcome)"
check "grant balance" 10 "$(json_get balance)"
SEQ="$(json_get seq)"
call POST $ID /grant "$(entry tx:1 dreameater purchase 10)"
check "the replay is 200" 200 "$STATUS"
check "the replay outcome" replayed "$(json_get outcome)"
check "the replay keeps the seq" "$SEQ" "$(json_get seq)"
call GET $ID "/balance?tenant=dreameater"
check "the replay changed nothing" '{"balance":10,"purchased":10,"spent":0}' "$(python3 -I -c 'import json;print(json.dumps(json.load(open("'"$BODY"'")),separators=(",",":")))')"
call POST $ID /grant "$(entry tx:1 dreameater purchase 11)"
check "a reused key with another amount is 409" 409 "$STATUS"
check "the conflict code" idempotency_conflict "$(json_get error)"

echo "== spend to insufficiency"
for n in 1 2 3 4 5; do
  call POST $ID /spend "$(entry spend:$n dreameater spend -2)"
  check "spend $n" 200 "$STATUS"
done
check "balance after five spends" 0 "$(json_get balance)"
call POST $ID /spend "$(entry spend:6 dreameater spend -2)"
check "the next spend is 402" 402 "$STATUS"
check "the 402 reports the balance" 0 "$(json_get balance)"
call POST $ID /spend "$(entry spend:big dreameater spend -50)"
check "an overdraw is 402" 402 "$STATUS"
call POST $ID /spend "$(entry spend:1 dreameater spend -2)"
check "a spend replays" replayed "$(json_get outcome)"
call GET $ID "/entries?tenant=dreameater"
check "no entry was appended by the failures" 6 "$(python3 -I -c 'import json;print(len(json.load(open("'"$BODY"'"))["entries"]))')"

echo "== tenants, migrate, reverse, pool"
call POST $ID /grant "$(entry tx:2 pixie purchase 4)"
call POST $ID /spend "$(entry spend:p pixie spend -5)"
check "another tenant cannot spend the first tenant's credits" 402 "$STATUS"
check "that tenant's own balance" 4 "$(json_get balance)"
call POST $ID /grant "$(entry tx:3 '*' purchase 4)"
check "tenant * is rejected" 400 "$STATUS"
call POST $ID /migrate '{"tenant":"payday","key":"migrate:payday:u1","moved":10,"welcome_part":6}'
check "first migrate moves everything" 10 "$(json_get amount)"
call POST $ID /migrate '{"tenant":"payday","key":"migrate:payday:u2","moved":8,"welcome_part":6}'
check "second migrate drops the welcome" 2 "$(json_get amount)"
call POST $ID /migrate '{"tenant":"payday","key":"migrate:payday:u2","moved":99,"welcome_part":0}'
check "a migrate replays" true "$(json_get replayed)"
check "a migrate replays its stored amount" 2 "$(json_get amount)"
call POST $ID /reverse "$(entry reverse:tx:3 payday reversal -100)"
check "a reversal is clamped" 12 "$(json_get taken)"
check "the clamped balance" 0 "$(json_get balance)"
call POST other-midgar-id /spend "$(entry spend:1 dreameater spend -1)"
check "another Midgar ID has its own empty wallet" 402 "$STATUS"

echo "== verify, erase"
call GET $ID /verify
check "verify is ok" true "$(json_get ok)"
check "verify saw three tenants" 3 "$(json_get tenants)"
call POST $ID /erase '{"tenant":"pixie"}'
check "erase removes one tenant's entries" 1 "$(json_get erased)"
call GET $ID "/balance?tenant=pixie"
check "the erased tenant is empty" 0 "$(json_get balance)"
call GET $ID "/balance?tenant=dreameater"
check "other tenants are untouched" 10 "$(json_get spent)"
call GET $ID /verify
check "verify is ok after the erase" true "$(json_get ok)"

echo "== $STEPS checks, $FAILURES failed"
[ "$FAILURES" -eq 0 ]
