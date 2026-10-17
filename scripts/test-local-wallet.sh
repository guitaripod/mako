# Sourced by scripts/test-local.sh after the whole existing suite has run with WALLET_LINKING off. It restarts the
# server with WALLET_LINKING on against the same local database and Durable Object storage and runs the wallet flow
# of P3.07 steps 5 and 6: linking, spends, purchases, residue, native requests and deletion.

M1="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47"
M_SECOND="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d48"
M_NATIVE="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d49"
M_DEVELOPMENT="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d4a"
M_UNKNOWN="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d4b"
M_MOVING="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d4c"

upper() {
  printf %s "$1" | tr a-f A-F
}

call_token() {
  local method="$1" path="$2" key="$3" token="$4" body="${5:-}" app="${6:-dreameater}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path" -H "X-App-ID: $app" -H "Authorization: Bearer $key" -H "X-Midgar-Token: $token")
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  STATUS=$(curl "${args[@]}")
}

call_native() {
  local method="$1" path="$2" token="$3" body="${4:-}" app="${5:-dreameater}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path" -H "X-App-ID: $app" -H "Authorization: Bearer $token")
  if [ -n "$body" ]; then
    args+=(-H "Content-Type: application/json" --data "$body")
  fi
  STATUS=$(curl "${args[@]}")
}

webhook() {
  STATUS=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "$BASE/v1/revenuecat/webhook" -H "Authorization: Bearer $WEBHOOK_SECRET" --data "$1")
}

purchase_event() {
  local user="$1" tx="$2" kind="${3:-NON_RENEWING_PURCHASE}" product="${4:-small}"
  printf '{"api_version":"1.0","event":{"app_user_id":"%s","environment":"PRODUCTION","event_timestamp_ms":1,"id":"ev-%s","is_family_share":false,"product_id":"com.dreameater.credits.%s","purchased_at_ms":1,"store":"APP_STORE","transaction_id":"%s","type":"%s"}}' "$user" "$tx" "$product" "$tx" "$kind"
}

summary_of() {
  sql_value "SELECT $1 FROM wallet_summary WHERE app_id = 'dreameater' AND midgar_id = '$2'"
}

trigger_cron() {
  curl -s -o /dev/null "$BASE/__scheduled?cron=*/30+*+*+*+*"
  sleep 1
}

stop_server() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
    SERVER_PID=""
  fi
}

start_server() {
  npx wrangler dev --local --test-scheduled --config "$CONFIG" --config "$IDENTITY_CONFIG" --persist-to "$STATE" --port "$PORT" >>"$LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 240); do
    if curl -s -o /dev/null "$BASE/docs"; then
      return 0
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      echo "FAIL  wrangler dev exited early"
      tail -30 "$LOG"
      return 1
    fi
    sleep 1
  done
  echo "FAIL  server did not become ready"
  return 1
}

transaction_types() {
  python3 -I -c '
import json, sys
body = json.load(open(sys.argv[1]))
print(" ".join(t["type"] for t in body["transactions"]))
' "$BODY"
}

echo "== wallet linking off: a token changes nothing"
WNOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
OLD="2025-01-01T00:00:00+00:00"
WSEED="$(mktemp -t mako-local-wallet-seed.XXXXXX)"
cat > "$WSEED" <<EOF
INSERT INTO users (id, provider, provider_id, name, api_key_hash, created_at, updated_at, app_id, is_admin) VALUES
 ('u-link',   'anonymous', 'link',   'Link',   '$(key_hash key_link)',   '$WNOW', '$WNOW', 'dreameater', 0),
 ('u-link2',  'anonymous', 'link2',  'Link2',  '$(key_hash key_link2)',  '$WNOW', '$WNOW', 'dreameater', 0),
 ('u-move',   'anonymous', 'move',   'Move',   '$(key_hash key_move)',   '$WNOW', '$WNOW', 'dreameater', 0),
 ('u-admin',  'anonymous', 'admin',  'Admin',  '$(key_hash key_admin)',  '$WNOW', '$WNOW', 'dreameater', 1);
INSERT INTO user_credits (app_id, user_id, balance, created_at, updated_at) VALUES
 ('dreameater', 'u-link', 40, '$WNOW', '$WNOW'),
 ('dreameater', 'u-link2', 1, '$WNOW', '$WNOW'),
 ('dreameater', 'u-move', 9, '$WNOW', '$WNOW'),
 ('dreameater', 'u-admin', 0, '$WNOW', '$WNOW');
INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, created_at) VALUES
 ('t-link-welcome', 'dreameater', 'u-link', 'bonus', 1, 1, 'Welcome bonus', '$WNOW'),
 ('t-link2-welcome', 'dreameater', 'u-link2', 'bonus', 1, 1, 'Welcome bonus', '$WNOW');
EOF
if ! sql_file "$WSEED"; then
  echo "FAIL  the wallet seed did not apply (see $LOG)"
  tail -20 "$LOG"
  exit 1
fi
rm -f "$WSEED"
check "the flag ships off" off "$(sed -n 's/^WALLET_LINKING = "\(.*\)"$/\1/p' wrangler.toml)"
call_token GET /v1/credits/balance key_link "$(minted_token valid)"
check "a valid token with the flag off leaves the user on its D1 balance" 40 "$(json_get balance)"
check "the flag off creates no link" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_links")"
check "the flag off writes no mirror" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_summary")"
call_native GET /v1/credits/balance "$(minted_token native)"
check "the flag off refuses a token in the bearer slot" 401 "$STATUS"
check "the flag off creates no Midgar user" 0 "$(sql_value "SELECT COUNT(*) FROM users WHERE provider = 'midgar'")"

echo "== restarting with WALLET_LINKING on"
stop_server
printf 'WALLET_LINKING=on\nIDENTITY_LOOKUP_SECRET=%s\n' "$LOOKUP_SECRET" >> "$DEV_VARS"
if ! start_server; then
  exit 1
fi
echo "PASS  server is up with wallets on"

echo "== a legacy user links and its balance moves exactly once"
call GET /v1/credits/balance key_link
check "without a token the user is still on its D1 balance" 40 "$(json_get balance)"
check "a request without a token links nobody" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_links")"
call_token GET /v1/credits/balance key_link "$(minted_token valid)"
check "the first request with a token answers the migrated balance" 40 "$(json_get balance)"
check "the link is linked" linked "$(sql_value "SELECT state FROM wallet_links WHERE user_id = 'u-link'")"
check "the link records the moved balance" 40 "$(sql_value "SELECT moved_credits FROM wallet_links WHERE user_id = 'u-link'")"
check "the link records the welcome bonus inside it" 1 "$(sql_value "SELECT welcome_part FROM wallet_links WHERE user_id = 'u-link'")"
check "the link names the lowercased Midgar ID" "$M1" "$(sql_value "SELECT midgar_id FROM wallet_links WHERE user_id = 'u-link'")"
check "the D1 balance is zero" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
check "the mirror shows the wallet balance" 40 "$(summary_of balance "$M1")"
call_token GET /v1/credits/balance key_link "$(minted_token valid)"
check "a second request with the token changes nothing" 40 "$(json_get balance)"
call GET /v1/credits/balance key_link
check "the API key alone now reads the wallet" 40 "$(json_get balance)"
check "the D1 balance is still zero" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
call GET /v1/credits/transactions key_link
check "the transactions list names the migration once, then the older D1 rows" "migrate bonus" "$(transaction_types)"

echo "== spends hit the wallet"
call POST /v1/run/chat.completion key_link '{"messages":[{"role":"user","content":"I dreamed of a pier"}],"response_json":true}'
check "a chat against the wallet answers 200" 200 "$STATUS"
check "it charges the flat credit" 1 "$(json_get credits_charged)"
check "the wallet paid for it" 39 "$(balance_of key_link)"
check "the D1 balance did not move" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
check "no D1 ledger row was written for the spend" 0 "$(sql_value "SELECT COUNT(*) FROM credit_transactions WHERE user_id = 'u-link' AND type = 'spend'")"
check "the mirror counted the spend" 1 "$(summary_of spent "$M1")"
call GET /v1/credits/transactions key_link
check "the spend heads the list" "spend migrate bonus" "$(transaction_types)"

echo "== purchases: a replayed webhook grants once"
webhook "$(purchase_event u-link tx-w1)"
check "a purchase event for a linked user is accepted" 200 "$STATUS"
check "the pack went to the wallet" 44 "$(balance_of key_link)"
webhook "$(purchase_event u-link tx-w1)"
check "the same event again" 200 "$STATUS"
check "grants nothing more" 44 "$(balance_of key_link)"
check "one purchase row, completed" "1 completed" "$(sql_value "SELECT COUNT(*) || ' ' || MAX(status) FROM credit_purchases WHERE payment_id = 'tx-w1'")"
check "the D1 balance still did not move" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
check "the mirror counted the purchase" 5 "$(summary_of purchased "$M1")"
webhook "$(purchase_event "$(upper "$M1")" tx-w2)"
check "a purchase addressed to the Midgar ID in capitals is accepted" 200 "$STATUS"
check "it reached the wallet of the user linked to that Midgar ID" 49 "$(balance_of key_link)"
call POST /v1/credits/purchase/revenuecat/validate key_link '{"pack_id":"large","purchase_token":"tx-w3","product_id":"com.dreameater.credits.large","platform":"ios"}'
check "the client fast-track is accepted" 200 "$STATUS"
check "and credits the wallet" 64 "$(balance_of key_link)"
webhook "$(purchase_event u-link tx-w3 NON_RENEWING_PURCHASE large)"
check "the webhook for the same transaction credits nothing more" 64 "$(balance_of key_link)"
call POST /v1/credits/purchase/revenuecat/validate key_link '{"pack_id":"large","purchase_token":"tx-w3","product_id":"com.dreameater.credits.large","platform":"ios"}'
check "the fast-track for a completed transaction is refused" 400 "$STATUS"
check "that left the balance alone" 64 "$(balance_of key_link)"

echo "== a reinstall counts the welcome bonus once"
call_token GET /v1/credits/balance key_link2 "$(minted_token valid)"
check "the second user of the same Midgar ID reads the same wallet" 64 "$(json_get balance)"
check "it moved its welcome bonus' worth of nothing" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link2'")"
check "its link recorded one credit moved, all of it the welcome bonus" "1 1 linked" "$(sql_value "SELECT moved_credits || ' ' || welcome_part || ' ' || state FROM wallet_links WHERE user_id = 'u-link2'")"
call GET /v1/credits/transactions key_link2
check "the wallet holds one migration per user" 2 "$(python3 -I -c 'import json,sys;print(sum(1 for t in json.load(open(sys.argv[1]))["transactions"] if t["type"]=="migrate"))' "$BODY")"

echo "== a user keeps its link when another Midgar ID shows up"
call_token GET /v1/credits/balance key_link2 "$(minted_token second)"
check "the request still succeeds on the existing link" 200 "$STATUS"
check "and reads the first Midgar ID's wallet" 64 "$(json_get balance)"
check "the link still names the first Midgar ID" "$M1" "$(sql_value "SELECT midgar_id FROM wallet_links WHERE user_id = 'u-link2'")"
check "no wallet was opened for the other Midgar ID" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_summary WHERE midgar_id = '$M_SECOND'")"

echo "== credits that landed in the frozen D1 row move on the next cron"
sql_json "UPDATE user_credits SET balance = 7 WHERE user_id = 'u-link'" >/dev/null
trigger_cron
check "the sweep moved the residue into the wallet" 71 "$(balance_of key_link)"
check "the D1 row is empty again" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
check "the sweep counted the move and cleared the pending amount" "1 null" "$(sql_value "SELECT residue_moves || ' ' || COALESCE(residue_pending, 'null') FROM wallet_links WHERE user_id = 'u-link'")"
trigger_cron
check "a second cron moves nothing more" 71 "$(balance_of key_link)"

echo "== refunds and admin adjustments"
webhook "$(purchase_event u-link tx-w1 CANCELLATION)"
check "a refund event is accepted" 200 "$STATUS"
check "the refund took the pack back out of the wallet" 66 "$(balance_of key_link)"
webhook "$(purchase_event u-link tx-w1 CANCELLATION)"
check "a refund is reversed only once" 66 "$(balance_of key_link)"
call POST /v1/admin/credits/adjust key_admin '{"user_id":"u-link","amount":3,"reason":"local test"}'
check "an admin credit is accepted" 200 "$STATUS"
check "it reports the wallet balance" 69 "$(json_get new_balance)"
call POST /v1/admin/credits/adjust key_admin '{"user_id":"u-link","amount":-4,"reason":"local test"}'
check "an admin debit is accepted" 200 "$STATUS"
check "it takes from the wallet" 65 "$(balance_of key_link)"
call POST /v1/admin/credits/adjust key_admin '{"user_id":"u-link","amount":-1000,"reason":"local test"}'
check "an admin debit larger than the balance clamps at zero" 0 "$(json_get new_balance)"
call POST /v1/admin/credits/adjust key_admin '{"user_id":"u-link","amount":65,"reason":"put it back"}'
check "the balance is back for the next checks" 65 "$(balance_of key_link)"
call GET /v1/admin/credits/stats key_admin
check "the admin statistics include the wallet balances" yes "$(python3 -I -c 'import json,sys;print("yes" if json.load(open(sys.argv[1]))["credits"]["total_balance"] >= 65 else "no")' "$BODY")"

echo "== a purchase left pending and a link left moving are finished by the cron"
sql_json "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at) VALUES ('p-stale', 'dreameater', 'u-link2', 'small', 5, 299, 'revenuecat', 'tx-stale', 'pending', '$OLD')" >/dev/null
sql_json "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at) VALUES ('p-stripe', 'dreameater', 'u-link2', 'small', 5, 299, 'stripe', 'cs_waiting', 'pending', '$OLD')" >/dev/null
sql_json "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES ('dreameater', 'u-move', '$M_MOVING', 'moving', '$OLD')" >/dev/null
trigger_cron
check "the stale RevenueCat purchase was completed" completed "$(sql_value "SELECT status FROM credit_purchases WHERE id = 'p-stale'")"
check "and credited once" 70 "$(balance_of key_link2)"
check "a Stripe purchase waiting for its payment was left alone" pending "$(sql_value "SELECT status FROM credit_purchases WHERE id = 'p-stripe'")"
check "the stale moving link was finished" linked "$(sql_value "SELECT state FROM wallet_links WHERE user_id = 'u-move'")"
check "its balance moved into the wallet" 9 "$(summary_of balance "$M_MOVING")"
check "and left D1" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-move'")"
trigger_cron
check "a second cron credits nothing twice" 70 "$(balance_of key_link2)"

echo "== grants addressed to a Midgar ID with no user yet"
webhook "$(purchase_event "$M_UNKNOWN" tx-unknown)"
check "a Midgar ID identity does not hold is refused so the sender retries" 500 "$STATUS"
check "and creates no user" 0 "$(sql_value "SELECT COUNT(*) FROM users WHERE provider_id = '$M_UNKNOWN'")"
webhook "$(purchase_event "not-a-user-or-a-uuid" tx-garbage)"
check "an id that is neither a user nor a Midgar ID is refused" 500 "$STATUS"
webhook "$(purchase_event "$(upper "$M_SECOND")" tx-second)"
check "a Midgar ID identity holds is accepted" 200 "$STATUS"
check "it got a Midgar user" 1 "$(sql_value "SELECT COUNT(*) FROM users WHERE provider = 'midgar' AND provider_id = '$M_SECOND'")"
check "and a linked wallet with nothing to move" "linked 0" "$(sql_value "SELECT state || ' ' || moved_credits FROM wallet_links WHERE midgar_id = '$M_SECOND'")"
check "holding the purchase" 5 "$(summary_of balance "$M_SECOND")"
call_native GET /v1/credits/balance "$(minted_token second)"
check "the buyer's first request with only its token finds the wallet" 5 "$(json_get balance)"

echo "== Midgar-ID-native requests"
call_native GET /v1/credits/balance "$(minted_token native)"
check "a token alone in the bearer slot is a credential with wallets on" 200 "$STATUS"
check "an attested device gets the welcome bonus" 1 "$(json_get balance)"
check "the user is a midgar user without an API key" 1 "$(sql_value "SELECT COUNT(*) FROM users WHERE provider = 'midgar' AND provider_id = '$M_NATIVE' AND api_key_hash IS NULL")"
call_native GET /v1/credits/balance "$(minted_token native)"
check "a second native request grants nothing more" 1 "$(json_get balance)"
check "and creates no second user" 1 "$(sql_value "SELECT COUNT(*) FROM users WHERE provider = 'midgar' AND provider_id = '$M_NATIVE'")"
call_native POST /v1/run/chat.completion "$(minted_token native)" '{"messages":[{"role":"user","content":"hello"}],"response_json":true}'
check "a native chat spends the welcome credit" 200 "$STATUS"
call_native GET /v1/credits/balance "$(minted_token native)"
check "leaving nothing" 0 "$(json_get balance)"
call_native POST /v1/run/chat.completion "$(minted_token native)" '{"messages":[{"role":"user","content":"hello"}],"response_json":true}'
check "the next chat is refused for lack of credits" 402 "$STATUS"
call_native GET /v1/credits/balance "$(minted_token native_development)"
check "a development tier device is accepted" 200 "$STATUS"
check "but gets no welcome bonus" 0 "$(json_get balance)"
for bad in native_wrong_app native_expired tampered other_key; do
  call_native GET /v1/credits/balance "$(minted_token $bad)"
  check "a $bad token alone is refused" 401 "$STATUS"
done
call_native GET /v1/credits/balance "eyJ.garbage.here"
check "garbage shaped like a token is refused" 401 "$STATUS"
call_native GET /v1/credits/balance "$(minted_token native)" "" pixie
check "a token issued for another app is refused" 401 "$STATUS"
call GET /v1/credits/balance key_unknown
check "an unknown API key is still refused" 401 "$STATUS"

echo "== deleting an account erases its wallet partition"
call_native DELETE /v1/identity "$(minted_token native)"
check "a native account deletes itself" 200 "$STATUS"
check "its wallet mirror is gone" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_summary WHERE midgar_id = '$M_NATIVE'")"
check "its link and user are gone" 0 "$(sql_value "SELECT (SELECT COUNT(*) FROM wallet_links WHERE midgar_id = '$M_NATIVE') + (SELECT COUNT(*) FROM users WHERE provider_id = '$M_NATIVE')")"
call DELETE /v1/identity key_link
check "a linked legacy account deletes itself" 200 "$STATUS"
check "its wallet mirror is gone" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_summary WHERE midgar_id = '$M1'")"
check "its link row is gone" 0 "$(sql_value "SELECT COUNT(*) FROM wallet_links WHERE user_id = 'u-link'")"
check "its user row is gone" 0 "$(sql_value "SELECT COUNT(*) FROM users WHERE id = 'u-link'")"
check "the tenant partition of the Midgar ID was erased for the other user too" 0 "$(balance_of key_link2)"
call GET /v1/credits/balance key_link
check "the deleted key no longer authenticates" 401 "$STATUS"

stop_server
