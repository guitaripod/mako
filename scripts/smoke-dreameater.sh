#!/usr/bin/env bash
set -uo pipefail

: "${MAKO_URL:?set MAKO_URL, for example https://mako.midgarcorp.cc}"
: "${API_KEY:?set API_KEY to the mako api key of a Dream Eater user}"
APP_ID="${APP_ID:-dreameater}"
ALIAS="${ALIAS:-reading.v2}"
DEFAULT_INPUT='{"dream":{"text":"I stood at the end of a long pier at night."}}'
ALIAS_INPUT="${ALIAS_INPUT:-$DEFAULT_INPUT}"
STYLE="${STYLE:-ink}"
ART_PRICE="${ART_PRICE:-1}"
PREMIUM="${PREMIUM:-0}"
THROWAWAY_API_KEY="${THROWAWAY_API_KEY:-}"

BODY="$(mktemp -t mako-smoke.XXXXXX)"
trap 'rm -f "$BODY"' EXIT
RUN="smoke-$(date +%s)-$$"
FAILURES=0
STATUS=""

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; echo "      status $STATUS body: $(head -c 300 "$BODY")"; FAILURES=$((FAILURES + 1)); }

call() {
  local method="$1" path="$2" key="$3" body="${4:-}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X "$method" "$MAKO_URL$path" -H "X-App-ID: $APP_ID" -H "Authorization: Bearer $key")
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  STATUS=$(curl "${args[@]}")
}

field() {
  python3 -I -c '
import json, sys
try:
    value = json.load(open(sys.argv[2]))
    for part in sys.argv[1].split("/"):
        value = value[part]
    print("null" if value is None else value)
except Exception:
    print("MISSING")
' "$1" "$BODY"
}

balance() {
  call GET /v1/credits/balance "$API_KEY"
  field balance
}

step() {
  local label="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then pass "$label"; else fail "$label (expected '$expected', got '$actual')"; fi
}

echo "smoke test against $MAKO_URL as app $APP_ID (alias $ALIAS)"

call GET /v1/credits/balance "$API_KEY"
START=$(field balance)
if [ "$STATUS" = 200 ] && [ "$START" != "MISSING" ]; then
  pass "balance reads $START credits"
else
  fail "balance"
fi
WEEKLY=$(field "weekly/$ALIAS/limit")
if [ "$WEEKLY" = "MISSING" ]; then
  pass "balance has no weekly bucket for $ALIAS (legacy shape)"
else
  REMAINING=$(field "weekly/$ALIAS/remaining")
  RESETS=$(field "weekly/$ALIAS/resets_at")
  step "balance weekly bucket for $ALIAS has limit $WEEKLY, remaining $REMAINING, resets $RESETS" "yes" \
    "$([ "$REMAINING" != MISSING ] && [ "$RESETS" != MISSING ] && echo yes || echo no)"
fi

BEFORE=$(balance)
call POST /v1/run/chat.completion "$API_KEY" '{"messages":[{"role":"user","content":"Reply with the single word ok."}]}'
CHARGED=$(field credits_charged)
EXPECTED_LEGACY=1
[ "$PREMIUM" = 1 ] && EXPECTED_LEGACY=0
step "legacy chat call answers 200" 200 "$STATUS"
step "legacy chat call charges the flat price" "$EXPECTED_LEGACY" "$CHARGED"
step "legacy chat call has no alias fields" MISSING "$(field alias)"
AFTER=$(balance)
step "legacy chat call moved the balance by the flat price" $((BEFORE - EXPECTED_LEGACY)) "$AFTER"

ALIAS_BODY=$(python3 -I -c '
import json, sys
print(json.dumps({"alias": sys.argv[1], "input": sys.argv[2], "idempotency_key": sys.argv[3]}))
' "$ALIAS" "$ALIAS_INPUT" "$RUN-alias")
BEFORE=$(balance)
call POST /v1/run/chat.completion "$API_KEY" "$ALIAS_BODY"
step "alias call answers 200" 200 "$STATUS"
step "alias call echoes the alias" "$ALIAS" "$(field alias)"
ALIAS_CHARGED=$(field credits_charged)
if [ "$STATUS" = 200 ]; then
  step "alias call returns usable content" yes "$(python3 -I -c '
import json
d = json.load(open("'"$BODY"'"))
print("yes" if isinstance(d.get("content"), str) and d["content"] else "no")')"
  echo "      credits_charged=$ALIAS_CHARGED free_remaining=$(field free_remaining) version=$(field version)"
fi

call POST /v1/run/chat.completion "$API_KEY" "$ALIAS_BODY"
step "idempotent repeat of the alias call answers 200" 200 "$STATUS"
step "idempotent repeat is not charged again" 0 "$(field credits_charged)"
step "idempotent repeat did not move the balance" $((BEFORE - ALIAS_CHARGED)) "$(balance)"

TRUNC_BODY=$(python3 -I -c '
import json, sys
print(json.dumps({"alias": sys.argv[1], "input": sys.argv[2], "idempotency_key": sys.argv[3]}))
' "$ALIAS" "$ALIAS_INPUT" "$RUN-alias-2")
call POST /v1/run/chat.completion "$API_KEY" "$TRUNC_BODY"
if [ "$STATUS" = 200 ]; then
  pass "second alias call finished without truncation"
elif [ "$(field error/code)" = "truncated" ]; then
  fail "second alias call was truncated, raise max_tokens on the alias row"
elif [ "$STATUS" = 402 ]; then
  pass "second alias call refused for balance (402), nothing charged"
else
  fail "second alias call"
fi

BEFORE=$(balance)
call POST /v1/images/generations "$API_KEY" '{"prompt":"a quiet pier at night, ink wash","n":1}'
step "legacy art call (no style) answers 200" 200 "$STATUS"
step "legacy art call stays free" "$BEFORE" "$(balance)"

BEFORE=$(balance)
ART_BODY=$(python3 -I -c '
import json, sys
print(json.dumps({"style": sys.argv[1], "scene": "A long pier reaches into dark, still water under a pale moon.", "mood": "calm", "idempotency_key": sys.argv[2]}))
' "$STYLE" "$RUN-art")
call POST /v1/images/generations "$API_KEY" "$ART_BODY"
EXPECTED_ART=$ART_PRICE
[ "$PREMIUM" = 1 ] && EXPECTED_ART=0
step "styled art call answers 200" 200 "$STATUS"
step "styled art call charges the art price" "$EXPECTED_ART" "$(field credits_charged)"
step "styled art call returns a base64 image" yes "$(python3 -I -c '
import json
d = json.load(open("'"$BODY"'"))
print("yes" if d.get("data") and d["data"][0].get("b64_json") else "no")' 2>/dev/null || echo no)"
step "styled art call moved the balance by the art price" $((BEFORE - EXPECTED_ART)) "$(balance)"
call POST /v1/images/generations "$API_KEY" "$ART_BODY"
step "idempotent art repeat answers 200" 200 "$STATUS"
step "idempotent art repeat is not charged again" 0 "$(field credits_charged)"

if [ -z "$THROWAWAY_API_KEY" ]; then
  REGISTER=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$MAKO_URL/v1/identity/anonymous" -H "X-App-ID: $APP_ID" -H "Content-Type: application/json" \
    --data "{\"unverified\":true,\"device_id\":\"$RUN\",\"client_device_id\":\"$RUN\"}")
  if [ "$REGISTER" = 200 ]; then
    THROWAWAY_API_KEY=$(field api_key)
  fi
fi
if [ -n "$THROWAWAY_API_KEY" ] && [ "$THROWAWAY_API_KEY" != MISSING ]; then
  call GET /v1/credits/balance "$THROWAWAY_API_KEY"
  step "throwaway user authenticates before deletion" 200 "$STATUS"
  call DELETE /v1/identity "$THROWAWAY_API_KEY"
  step "DELETE /v1/identity on the throwaway user answers 200" 200 "$STATUS"
  call GET /v1/credits/balance "$THROWAWAY_API_KEY"
  step "the deleted key no longer authenticates" 401 "$STATUS"
else
  echo "SKIP  DELETE /v1/identity: this server needs a DeviceCheck token to register, set THROWAWAY_API_KEY to a disposable user's key"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "SMOKE PASSED"
else
  echo "SMOKE FAILED: $FAILURES step(s)"
  exit 1
fi
