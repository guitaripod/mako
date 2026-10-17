#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PORT="${PORT:-8799}"
BASE="http://127.0.0.1:$PORT"
DB_NAME="openai-image-proxy"
BUCKET="openai-image-proxy-images"
STATE="$ROOT/.wrangler/test-local-state"
CONFIG="$ROOT/wrangler.local.toml"
DEV_VARS="$ROOT/.dev.vars"
DEV_VARS_BACKUP="$ROOT/.dev.vars.test-local-backup"
LOG="$(mktemp "${TMPDIR:-/tmp}/mako-local-log.XXXXXX")"
BODY="$(mktemp -t mako-local-body.XXXXXX)"
WEBHOOK_SECRET="local-webhook-secret"
SERVER_PID=""
FAILURES=0
STEPS=0

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
  fi
  rm -f "$CONFIG" "$BODY"
  if [ -f "$DEV_VARS_BACKUP" ]; then
    mv "$DEV_VARS_BACKUP" "$DEV_VARS"
  else
    rm -f "$DEV_VARS"
  fi
}
trap cleanup EXIT

export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/private/tmp/mako-target}"

wrangler_local() {
  npx wrangler "$@" --config "$CONFIG" --persist-to "$STATE"
}

sql_file() {
  wrangler_local d1 execute "$DB_NAME" --local --file "$1" >/dev/null 2>>"$LOG"
}

sql_json() {
  wrangler_local d1 execute "$DB_NAME" --local --json --command "$1" 2>>"$LOG"
}

sql_value() {
  sql_json "$1" | python3 -I -c '
import json, sys
data = json.load(sys.stdin)
rows = data[0]["results"] if data else []
print(list(rows[0].values())[0] if rows else "")
'
}

json_get() {
  python3 -I -c '
import json, sys
path = sys.argv[1].split("/")
try:
    value = json.load(open(sys.argv[2]))
    for part in path:
        value = value[part] if not isinstance(value, list) else value[int(part)]
    print("null" if value is None else (json.dumps(value) if isinstance(value, (dict, list, bool)) else value))
except Exception:
    print("MISSING")
' "$1" "$BODY"
}

has_key() {
  python3 -I -c '
import json, sys
value = json.load(open(sys.argv[2]))
print("yes" if sys.argv[1] in value else "no")
' "$1" "$BODY" 2>/dev/null || echo "no"
}

call() {
  local method="$1" path="$2" key="$3" body="${4:-}" app="${5:-dreameater}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path" -H "X-App-ID: $app" -H "Authorization: Bearer $key")
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

balance_of() {
  call GET /v1/credits/balance "$1" "" "${2:-dreameater}"
  json_get balance
}

echo "== building and preparing a fresh local database"
sed -e 's|^command = .*|command = "worker-build --dev"|' wrangler.toml > "$CONFIG"

if [ -f "$DEV_VARS" ]; then
  cp "$DEV_VARS" "$DEV_VARS_BACKUP"
fi
cat > "$DEV_VARS" <<EOF
ENVIRONMENT=development
MOCK_PROVIDERS=1
REVENUECAT_WEBHOOK_AUTH=$WEBHOOK_SECRET
EOF

rm -rf "$STATE"
mkdir -p "$STATE"

for migration in $(ls migrations/*.sql | sort); do
  if ! sql_file "$migration"; then
    echo "FAIL  migration $migration did not apply (see $LOG)"
    tail -20 "$LOG"
    exit 1
  fi
done
echo "PASS  all migrations applied in order to a fresh local D1"

SEED="$(mktemp -t mako-local-seed.XXXXXX)"
NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
key_hash() {
  printf %s "$1" | shasum -a 256 | cut -d' ' -f1
}
cat > "$SEED" <<EOF
INSERT INTO users (id, provider, provider_id, name, api_key_hash, created_at, updated_at, app_id) VALUES
 ('u-free',     'anonymous', 'free',     'Free',     '$(key_hash key_free)',     '$NOW', '$NOW', 'dreameater'),
 ('u-premium',  'anonymous', 'premium',  'Premium',  '$(key_hash key_premium)',  '$NOW', '$NOW', 'dreameater'),
 ('u-broke',    'anonymous', 'broke',    'Broke',    '$(key_hash key_broke)',    '$NOW', '$NOW', 'dreameater'),
 ('u-throw',    'anonymous', 'throw',    'Throw',    '$(key_hash key_throw)',    '$NOW', '$NOW', 'dreameater'),
 ('u-pixie',    'anonymous', 'pixie',    'Pixie',    '$(key_hash key_pixie)',    '$NOW', '$NOW', 'pixie'),
 ('u-scrub',    'anonymous', 'scrub',    'Scrub',    '$(key_hash key_scrub)',    '$NOW', '$NOW', 'scrubme');
UPDATE users SET premium_cached = 1, premium_checked_at = '$NOW' WHERE id = 'u-premium';
INSERT INTO api_keys (key_hash, user_id, app_id, created_at, last_used_at) VALUES
 ('prune-never-used-old', 'u-pixie', 'pixie', '2025-01-01T00:00:00+00:00', NULL),
 ('prune-last-used-old', 'u-pixie', 'pixie', '2025-01-01T00:00:00+00:00', '2025-06-01T00:00:00+00:00'),
 ('prune-created-old-used-now', 'u-pixie', 'pixie', '2025-01-01T00:00:00+00:00', '$NOW'),
 ('prune-created-now', 'u-pixie', 'pixie', '$NOW', NULL);
INSERT INTO user_credits (app_id, user_id, balance, created_at, updated_at) VALUES
 ('dreameater', 'u-free', 5, '$NOW', '$NOW'),
 ('dreameater', 'u-premium', 0, '$NOW', '$NOW'),
 ('dreameater', 'u-broke', 0, '$NOW', '$NOW'),
 ('dreameater', 'u-throw', 3, '$NOW', '$NOW'),
 ('pixie', 'u-pixie', 100, '$NOW', '$NOW'),
 ('scrubme', 'u-scrub', 0, '$NOW', '$NOW');
INSERT INTO chat_aliases (app_id, alias, model, system_prompt, response_schema, max_tokens, temperature, flat_credits, weekly_free, premium_bypass, version) VALUES
 ('dreameater', 'test.reading', 'gemini-3-flash-preview', 'Write in {language}.', '{"type":"object","properties":{"essence":{"type":"string"}},"required":["essence"]}', 900, 0.4, 1, 2, 1, 7),
 ('dreameater', 'test.followup', 'claude-haiku-4-5', 'Answer briefly in {language}.', NULL, NULL, NULL, 1, 0, 0, 3);
INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality, image_count, total_tokens, input_tokens, output_tokens, text_tokens, image_tokens, r2_keys, response_time_ms, created_at) VALUES
 ('ur-throw', 'dreameater', 'u-throw', 'generation', 'gemini', 'm', 'old dream text', '1024x1024', 'low', 1, 0, 0, 0, 0, 0, '[]', 1, '$NOW');
INSERT INTO paywall_events (id, app_id, user_id, capability, balance, created_at) VALUES ('pw-throw', 'dreameater', 'u-throw', 'chat.completion', 0, '$NOW');
INSERT INTO weekly_usage (app_id, user_id, alias, week_start, used) VALUES ('dreameater', 'u-throw', 'test.reading', '2026-01-05', 1);
INSERT INTO chat_requests (app_id, user_id, reference, alias, kind, created_at) VALUES ('dreameater', 'u-throw', 'chat:old', 'test.reading', 'weekly', '$NOW');
INSERT INTO stored_images (id, app_id, user_id, r2_key, prompt, model, size, created_at, expires_at) VALUES
 ('img-scrub', 'scrubme', 'u-scrub', 'u-scrub/img-scrub.png', 'a scrub dream', 'm', '1024x1024', '$NOW', '$NOW');
INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality, image_count, total_tokens, input_tokens, output_tokens, text_tokens, image_tokens, r2_keys, response_time_ms, error, created_at) VALUES
 ('ur-scrub', 'scrubme', 'u-scrub', 'generation', 'gemini', 'm', 'a scrub dream', '1024x1024', 'low', 1, 0, 0, 0, 0, 0, '["u-scrub/img-scrub.png"]', 1, 'Gemini API error: echoed a scrub dream', '$NOW');
EOF
if ! sql_file "$SEED"; then
  echo "FAIL  seeding did not apply (see $LOG)"
  tail -20 "$LOG"
  exit 1
fi
rm -f "$SEED"
echo "PASS  seeded tenants, users, the test aliases and rows to delete"

echo "== starting wrangler dev --local with mock providers"
npx wrangler dev --local --test-scheduled --config "$CONFIG" --persist-to "$STATE" --port "$PORT" >"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 240); do
  if curl -s -o /dev/null "$BASE/docs"; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL  wrangler dev exited early"
    tail -30 "$LOG"
    exit 1
  fi
  sleep 1
done
if ! curl -s -o /dev/null "$BASE/docs"; then
  echo "FAIL  server did not become ready"
  tail -30 "$LOG"
  exit 1
fi
echo "PASS  server is up"

echo "== api key hashing"
call GET /v1/credits/balance key_free
check "a key stored as a legacy hash authenticates" 200 "$STATUS"
call GET /v1/credits/balance key_free
check "the same key authenticates again" 200 "$STATUS"
call GET /v1/credits/balance "$(key_hash key_free)"
check "the stored hash is never a credential" 401 "$STATUS"
call GET /v1/credits/balance hashed:u-free
check "a placeholder left over from before migration 028 is never a credential" 401 "$STATUS"
call GET /v1/credits/balance key_unknown
check "an unknown key is rejected" 401 "$STATUS"
curl -s -o /dev/null "$BASE/__scheduled?cron=*/30+*+*+*+*"
check "the scheduled prune deleted the key never used and created long ago" 0 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash = 'prune-never-used-old'")"
check "the scheduled prune deleted the key last used long ago" 0 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash = 'prune-last-used-old'")"
check "the scheduled prune kept the old key used recently" 1 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash = 'prune-created-old-used-now'")"
check "the scheduled prune kept the key created recently" 1 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash = 'prune-created-now'")"
check "the scheduled prune left the user and the balance alone" "u-pixie 100" "$(sql_value "SELECT u.id || ' ' || c.balance FROM users u JOIN user_credits c ON c.user_id = u.id WHERE u.id = 'u-pixie'")"
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/identity/anonymous" -H "X-App-ID: dreameater" -H "Content-Type: application/json" --data '{"unverified":true,"device_id":"local-device-1"}')
check "anonymous registration succeeds" 200 "$STATUS"
NEW_KEY="$(json_get api_key)"
NEW_ID="$(json_get user_id)"
check "the users table has no plaintext key column" 0 "$(sql_value "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name = 'api_key'")"
check "a new user stores the key hash" "$(printf %s "$NEW_KEY" | shasum -a 256 | cut -d' ' -f1)" "$(sql_value "SELECT api_key_hash FROM users WHERE id = '$NEW_ID'")"
call GET /v1/credits/balance "$NEW_KEY"
check "a new user's key authenticates" 200 "$STATUS"
register_local_device() {
  curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/identity/anonymous" -H "X-App-ID: dreameater" -H "Content-Type: application/json" --data '{"unverified":true,"device_id":"local-device-1"}'
}
STATUS=$(register_local_device)
SECOND_KEY="$(json_get api_key)"
check "a second login for the same account returns a key" 200 "$STATUS"
STATUS=$(register_local_device)
THIRD_KEY="$(json_get api_key)"
for key in "$NEW_KEY" "$SECOND_KEY" "$THIRD_KEY"; do
  call GET /v1/credits/balance "$key"
  check "every login's key authenticates" 200 "$STATUS"
done
check "the three logins hold three different keys" 3 "$(printf '%s\n' "$NEW_KEY" "$SECOND_KEY" "$THIRD_KEY" | sort -u | wc -l | tr -d ' ')"
check "extra logins add key rows and never replace the account's first key" 2 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE user_id = '$NEW_ID'")"
check "the key table stores the hash of the second key" 1 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash = '$(printf %s "$SECOND_KEY" | shasum -a 256 | cut -d' ' -f1)'")"
check "no plaintext key is stored in the key table" 0 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE key_hash IN ('$SECOND_KEY', '$THIRD_KEY')")"
check "using a key stamps last_used_at" 2 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE user_id = '$NEW_ID' AND last_used_at IS NOT NULL")"
call DELETE /v1/identity "$SECOND_KEY"
check "deleting the account with one key succeeds" 200 "$STATUS"
check "account deletion removed every key row" 0 "$(sql_value "SELECT COUNT(*) FROM api_keys WHERE user_id = '$NEW_ID'")"
check "account deletion removed the user row" 0 "$(sql_value "SELECT COUNT(*) FROM users WHERE id = '$NEW_ID'")"
for key in "$NEW_KEY" "$SECOND_KEY" "$THIRD_KEY"; do
  call GET /v1/credits/balance "$key"
  check "no key of the deleted account authenticates" 401 "$STATUS"
done

echo "== balance"
call GET /v1/credits/balance key_free
check "balance 200" 200 "$STATUS"
check "balance shows 5 credits" 5 "$(json_get balance)"
check "weekly bucket for the test alias: limit" 2 "$(json_get weekly/test.reading/limit)"
WEEKLY_OUT="$(mktemp -t mako-local-weekly.XXXXXX)"
python3 -I - "$BODY" <<'PY' > "$WEEKLY_OUT"
import json, sys
body = json.load(open(sys.argv[1]))
bucket = body.get("weekly", {}).get("test.reading", {})
print(bucket.get("limit"), bucket.get("remaining"), bucket.get("resets_at", "")[-10:])
PY
check "weekly bucket is full and resets at midnight UTC" "2 2 T00:00:00Z" "$(cat "$WEEKLY_OUT")"
call GET /v1/credits/balance key_pixie "" pixie
check "other tenants have no weekly field" "no" "$(has_key weekly)"
check "other tenants' balance body is unchanged" '{"balance":100,"currency":"credits"}' "$(tr -d ' \n' < "$BODY")"

echo "== legacy chat (no alias)"
call POST /v1/run/chat.completion key_free '{"messages":[{"role":"user","content":"I dreamed of a pier"}],"response_json":true}'
check "legacy chat 200" 200 "$STATUS"
check "legacy chat charges a flat 1 credit" 1 "$(json_get credits_charged)"
check "legacy chat has no alias fields" "no" "$(has_key alias)"
check "legacy balance dropped by 1" 4 "$(balance_of key_free)"

echo "== alias chat"
READING='{"alias":"test.reading","input":"{\"dream\":\"a pier\"}","idempotency_key":"k1","language":"Finnish"}'
call POST /v1/run/chat.completion key_free "$READING"
check "alias call 200" 200 "$STATUS"
check "weekly bucket makes it free" 0 "$(json_get credits_charged)"
check "free_remaining counts down" 1 "$(json_get free_remaining)"
check "alias echoed" test.reading "$(json_get alias)"
check "version echoed" 7 "$(json_get version)"
check "content satisfies the schema" '{"essence":"mock"}' "$(json_get content)"
check "alias call left the balance alone" 4 "$(balance_of key_free)"

call POST /v1/run/chat.completion key_free "$READING"
check "same idempotency key answers again" 200 "$STATUS"
check "a repeated key is not counted against the bucket" 1 "$(json_get free_remaining)"
check "a repeated key is not charged" 0 "$(json_get credits_charged)"

call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"{}","idempotency_key":"k2"}'
check "second reading uses the last free one" 0 "$(json_get free_remaining)"
call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"{}","idempotency_key":"k3"}'
check "third reading is charged the flat price" 1 "$(json_get credits_charged)"
check "bucket stays empty" 0 "$(json_get free_remaining)"
check "balance paid for the third reading" 3 "$(balance_of key_free)"
call GET /v1/credits/balance key_free
check "balance endpoint shows the empty bucket" 0 "$(json_get weekly/test.reading/remaining)"

call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"MOCK_TRUNCATE","idempotency_key":"k4"}'
check "a truncated reply is 422" 422 "$STATUS"
check "truncation has its own error code" truncated "$(json_get error/code)"
check "a truncated reply is not charged" 3 "$(balance_of key_free)"

call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"{}","idempotency_key":"k5","retry_of":"k3"}'
check "a retry of a charged request is free" 0 "$(json_get credits_charged)"
check "the retry did not touch the balance" 3 "$(balance_of key_free)"
call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"{}","idempotency_key":"k6","retry_of":"k3"}'
check "a request is retried for free only once" 1 "$(json_get credits_charged)"

call POST /v1/run/chat.completion key_free '{"alias":"missing.alias","input":"{}","idempotency_key":"k7"}'
check "an unknown alias is 400" 400 "$STATUS"
call POST /v1/run/chat.completion key_free '{"alias":"test.reading","input":"{}"}'
check "an alias call without an idempotency key is 400" 400 "$STATUS"
call POST /v1/run/chat.completion key_broke '{"alias":"test.followup","input":"hi","idempotency_key":"b1"}'
check "a broke user is refused an alias call before the provider" 402 "$STATUS"

call POST /v1/run/chat.completion key_free '{"alias":"test.followup","input":"and then?","messages":[{"role":"user","content":"q"},{"role":"assistant","content":"a"}],"idempotency_key":"f1"}'
check "follow-up with prior turns" 200 "$STATUS"
check "follow-up without a bucket costs the flat price" 1 "$(json_get credits_charged)"
check "no bucket means null free_remaining" null "$(json_get free_remaining)"

call POST /v1/run/chat.completion key_premium "$READING"
check "premium user is not charged" 0 "$(json_get credits_charged)"
check "premium does not use the weekly bucket" 2 "$(json_get free_remaining)"

echo "== art"
sql_json "UPDATE user_credits SET balance = 5 WHERE user_id = 'u-free'" >/dev/null
call POST /v1/images/generations key_free '{"prompt":"a quiet pier at night","n":1}'
check "legacy image 200" 200 "$STATUS"
check "legacy image is returned as base64" yes "$(python3 -I -c 'import json;d=json.load(open("'"$BODY"'"));print("yes" if d["data"][0].get("b64_json") and not d["data"][0].get("url") else "no")')"
check "legacy art stays free" 5 "$(balance_of key_free)"
check "the tenant stored no image rows" 0 "$(sql_value "SELECT COUNT(*) FROM stored_images WHERE app_id = 'dreameater'")"
check "the tenant stored no prompt text" 0 "$(sql_value "SELECT COUNT(*) FROM usage_records WHERE app_id = 'dreameater' AND prompt != '' AND id != 'ur-throw'")"

call POST /v1/images/generations key_broke '{"style":"ink","scene":"A pier into a dark lake","idempotency_key":"a0"}'
check "a zero-balance user is refused art" 402 "$STATUS"
call POST /v1/images/generations key_free '{"style":"ink","scene":"A pier into a dark lake","mood":"calm","idempotency_key":"a1"}'
check "styled art 200" 200 "$STATUS"
check "styled art charges art_credits" 1 "$(json_get credits_charged)"
check "styled art returns base64" yes "$(python3 -I -c 'import json;d=json.load(open("'"$BODY"'"));print("yes" if d["data"][0].get("b64_json") else "no")')"
check "balance dropped by the art price" 4 "$(balance_of key_free)"
call POST /v1/images/generations key_free '{"style":"ink","scene":"A pier into a dark lake","mood":"calm","idempotency_key":"a1"}'
check "an idempotent art repeat is free" 0 "$(json_get credits_charged)"
check "an art repeat leaves the balance alone" 4 "$(balance_of key_free)"
call POST /v1/images/generations key_free '{"style":"nope","scene":"x","idempotency_key":"a2"}'
check "unknown style is 400" 400 "$STATUS"
call POST /v1/images/generations key_free '{"style":"ink","scene":"","idempotency_key":"a3"}'
check "empty scene is 400" 400 "$STATUS"
call POST /v1/images/generations key_free "{\"style\":\"ink\",\"scene\":\"$(python3 -I -c 'print("x"*401)')\",\"idempotency_key\":\"a4\"}"
check "a 401 character scene is 400" 400 "$STATUS"
call POST /v1/images/generations key_free '{"style":"ink","scene":"x","n":3,"idempotency_key":"a5"}'
check "n above 2 is 400" 400 "$STATUS"
call POST /v1/images/generations key_free '{"style":"ink","scene":"MOCK_NO_IMAGE","idempotency_key":"a6"}'
check "a provider that makes no image answers 422" 422 "$STATUS"
check "a failed art request is not charged" 4 "$(balance_of key_free)"
call POST /v1/images/generations key_premium '{"style":"gilded","scene":"A key in a door","n":2,"idempotency_key":"p1"}'
check "premium art 200 with two images" 2 "$(python3 -I -c 'import json;print(len(json.load(open("'"$BODY"'"))["data"]))')"
check "premium art is free" 0 "$(json_get credits_charged)"
check "styled art stored no prompt" 0 "$(sql_value "SELECT COUNT(*) FROM usage_records WHERE app_id = 'dreameater' AND request_type = 'art' AND prompt != ''")"
check "art wrote usage rows" 1 "$(sql_value "SELECT CASE WHEN COUNT(*) >= 2 THEN 1 ELSE 0 END FROM usage_records WHERE app_id = 'dreameater' AND request_type = 'art'")"

call POST /v1/images/generations key_pixie '{"prompt":"a cat on a roof","n":1}' pixie
check "another tenant's image request 200" 200 "$STATUS"
check "another tenant still gets a stored image url" yes "$(python3 -I -c 'import json;d=json.load(open("'"$BODY"'"));print("yes" if d["data"][0].get("url") else "no")')"
check "another tenant's image row is stored with its prompt" 1 "$(sql_value "SELECT COUNT(*) FROM stored_images WHERE app_id = 'pixie' AND prompt = 'a cat on a roof'")"
call POST /v1/images/generations key_pixie '{"prompt":"a cat","style":"ink","n":1}' pixie
check "a stray style field from a tenant without styles is ignored" 200 "$STATUS"

echo "== purchases"
EVENT='{"api_version":"1.0","event":{"app_user_id":"u-free","environment":"PRODUCTION","event_timestamp_ms":1,"id":"ev1","is_family_share":false,"product_id":"com.dreameater.credits.small","purchased_at_ms":1,"store":"APP_STORE","transaction_id":"tx-1","type":"NON_RENEWING_PURCHASE"}}'
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer wrong" --data "$EVENT")
check "a wrong webhook secret is 401" 401 "$STATUS"
BEFORE=$(balance_of key_free)
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$EVENT")
check "a consumable purchase event is accepted" 200 "$STATUS"
check "the webhook credited the pack" $((BEFORE + 5)) "$(balance_of key_free)"
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$EVENT")
check "the same event again credits nothing" $((BEFORE + 5)) "$(balance_of key_free)"
call POST /v1/credits/purchase/revenuecat/validate key_free '{"pack_id":"small","purchase_token":"tx-1","product_id":"com.dreameater.credits.small","platform":"ios"}'
check "the client validate call for the same transaction does not credit again" $((BEFORE + 5)) "$(balance_of key_free)"
call POST /v1/credits/purchase/revenuecat/validate key_free '{"pack_id":"large","purchase_token":"tx-2","product_id":"com.dreameater.credits.large","platform":"ios"}'
check "validate first credits the large pack" $((BEFORE + 20)) "$(balance_of key_free)"
LARGE_EVENT=$(echo "$EVENT" | sed -e 's/tx-1/tx-2/' -e 's/credits.small/credits.large/')
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$LARGE_EVENT")
check "the webhook after validate credits nothing more" $((BEFORE + 20)) "$(balance_of key_free)"

REFUND=$(echo "$LARGE_EVENT" | sed -e 's/NON_RENEWING_PURCHASE/CANCELLATION/')
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$REFUND")
check "a refund reverses the pack" $((BEFORE + 5)) "$(balance_of key_free)"
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$REFUND")
check "a refund is reversed only once" $((BEFORE + 5)) "$(balance_of key_free)"
check "the reversal is in the ledger under its refund reference" 1 "$(sql_value "SELECT COUNT(*) FROM credit_transactions WHERE reference_id = 'refund:tx-2'")"
for n in $(seq 1 15); do
  call POST /v1/run/chat.completion key_free "{\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]}" >/dev/null
done
SMALL_REFUND=$(echo "$EVENT" | sed -e 's/NON_RENEWING_PURCHASE/CANCELLATION/')
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$SMALL_REFUND")
check "a refund never takes the balance below zero" 0 "$(balance_of key_free)"

PIXIE_EVENT=$(echo "$EVENT" | sed -e 's/u-free/u-pixie/' -e 's/com.dreameater.credits.small/com.guitaripod.pixie.credits.starter/' -e 's/tx-1/tx-pixie/')
PIXIE_BEFORE=$(balance_of key_pixie pixie)
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$PIXIE_EVENT")
check "another tenant's consumable event still only tags revenue" "$PIXIE_BEFORE" "$(balance_of key_pixie pixie)"

echo "== privacy page"
STATUS=$(curl -s -o "$BODY" -w '%{http_code}' "$BASE/privacy/dreameater")
check "dreameater privacy page moves to midgarcorp.cc (P3.08)" 301 "$STATUS"
LOCATION=$(curl -s -o /dev/null -w '%{redirect_url}' "$BASE/privacy/dreameater")
check "the redirect names the registry page" "https://midgarcorp.cc/legal/dream-eater/privacy/" "$LOCATION"

echo "== account deletion"
call DELETE /v1/identity key_throw
check "DELETE /v1/identity succeeds with usage history and a foreign key" 200 "$STATUS"
for table in usage_records paywall_events weekly_usage chat_requests user_credits; do
  check "no $table rows left for the deleted user" 0 "$(sql_value "SELECT COUNT(*) FROM $table WHERE user_id = 'u-throw'")"
done
check "the user row is gone" 0 "$(sql_value "SELECT COUNT(*) FROM users WHERE id = 'u-throw'")"
call GET /v1/credits/balance key_throw
check "the deleted key no longer authenticates" 401 "$STATUS"

echo "== scrub script"
printf 'png' > "$STATE/scrub.png"
wrangler_local r2 object put "$BUCKET/u-scrub/img-scrub.png" --file "$STATE/scrub.png" --local >/dev/null 2>>"$LOG"
export MAKO_WRANGLER_ARGS="--config $CONFIG --persist-to $STATE"
if bash scripts/scrub-tenant.sh scrubme --local >/dev/null 2>&1; then
  check "scrub refuses to run without --yes" refused ran
else
  check "scrub refuses to run without --yes" refused refused
fi
SCRUB_OUT=$(bash scripts/scrub-tenant.sh scrubme --yes --local 2>>"$LOG")
echo "$SCRUB_OUT" | sed 's/^/      /'
check "scrub removed the stored image rows" 0 "$(sql_value "SELECT COUNT(*) FROM stored_images WHERE app_id = 'scrubme'")"
check "scrub blanked the usage prompt and error" 0 "$(sql_value "SELECT COUNT(*) FROM usage_records WHERE app_id = 'scrubme' AND (prompt != '' OR error IS NOT NULL)")"
SECOND=$(bash scripts/scrub-tenant.sh scrubme --yes --local 2>>"$LOG")
check "scrub is safe to run again" 1 "$(echo "$SECOND" | grep -c 'after: stored_images=0 usage_records_with_prompt=0')"

echo "== smoke script against the local server"
sql_json "UPDATE user_credits SET balance = 10 WHERE user_id = 'u-free'" >/dev/null
SMOKE_OUT=$(MAKO_URL="$BASE" API_KEY=key_free APP_ID=dreameater ALIAS=test.reading bash scripts/smoke-dreameater.sh 2>&1)
SMOKE_STATUS=$?
echo "$SMOKE_OUT" | sed 's/^/      /'
STEPS=$((STEPS + 1))
if [ "$SMOKE_STATUS" -eq 0 ]; then
  echo "PASS  scripts/smoke-dreameater.sh passes against the mock-provider server"
else
  echo "FAIL  scripts/smoke-dreameater.sh"
  FAILURES=$((FAILURES + 1))
fi

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "ALL $STEPS CHECKS PASSED"
else
  echo "$FAILURES OF $STEPS CHECKS FAILED (server log: $LOG)"
  exit 1
fi
