# Sourced by scripts/test-local.sh after the wallet-less suite. It defines the checks of the StoreKit 2 verification
# route (a signed transaction verified through the ledger, here the stub in scripts/local-ledger) and the stable credit
# error codes: verify_with_linking_off runs against the server as it is, verify_with_linking_on is called by
# scripts/test-local-wallet.sh once the server runs with WALLET_LINKING on and the wallet users exist.

VERIFY_MIDGAR="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47"
VERIFY_OTHER_MIDGAR="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d99"
VERIFY_NATIVE_MIDGAR="0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d49"
VERIFY_PATH="/v1/credits/purchase/validate"
VERIFY_OLD_PATH="/v1/credits/purchase/revenuecat/validate"

stub_transaction() {
  python3 -I -c '
import base64, json, sys
fields = {
    "transactionId": sys.argv[1], "originalTransactionId": sys.argv[1], "bundleId": "com.marcusziade.DreamEater",
    "productId": "com.dreameater.credits." + sys.argv[3], "type": "Consumable", "appAccountToken": sys.argv[2],
    "environment": "Production", "purchaseDate": 1760000000000, "registrySlug": "dream-eater",
}
fields.update(json.loads(sys.argv[4] or "{}"))
print("stub." + base64.urlsafe_b64encode(json.dumps(fields).encode()).decode().rstrip("="))
' "$1" "$2" "$3" "${4-}"
}

validate_body() {
  local tx="$1" midgar="$2" pack="${3:-small}" overrides="${4-}"
  printf '{"pack_id":"%s","purchase_token":"%s","product_id":"com.dreameater.credits.%s","platform":"ios","signed_transaction":"%s"}' \
    "$pack" "$tx" "$pack" "$(stub_transaction "$tx" "$midgar" "$pack" "$overrides")"
}

signed_validate() {
  local key="$1" token="$2" body="$3" path="${4:-$VERIFY_PATH}"
  local args=(-s -o "$BODY" -w '%{http_code}' -X POST "$BASE$path" -H "X-App-ID: dreameater" -H "Authorization: Bearer $key" -H "Content-Type: application/json" --data "$body")
  if [ -n "$token" ]; then
    args+=(-H "X-Midgar-Token: $token")
  fi
  STATUS=$(curl "${args[@]}")
}

refused_leaves_balance() {
  local label="$1" expected_status="$2" expected_code="$3" before="$4" key="$5"
  check "$label: status" "$expected_status" "$STATUS"
  check "$label: code" "$expected_code" "$(json_get error/code)"
  check "$label: nothing was credited" "$before" "$(balance_of "$key")"
}

verify_with_linking_off() {
  echo "== signed StoreKit transactions through the ledger, wallets off"
  local token before
  token="$(minted_token valid)"
  before="$(balance_of key_free)"

  signed_validate key_free "$token" "$(validate_body sk-1 "$VERIFY_MIDGAR")"
  check "a verified consumable of this tenant is credited" 200 "$STATUS"
  check "it credits the pack" 5 "$(json_get credits_added)"
  check "it answers the new balance" $((before + 5)) "$(json_get new_balance)"
  check "the local balance holds it" $((before + 5)) "$(balance_of key_free)"
  check "the purchase row is keyed by the store transaction" "completed revenuecat" "$(sql_value "SELECT status || ' ' || payment_provider FROM credit_purchases WHERE payment_id = 'sk-1'")"

  signed_validate key_free "$token" "$(validate_body sk-1 "$VERIFY_MIDGAR")"
  check "a replay is 400" 400 "$STATUS"
  check "a replay has its own code" already_processed "$(json_get error/code)"
  check "a replay keeps the text old clients match" yes "$(json_get error/message | grep -q 'already been processed' && echo yes || echo no)"
  check "a replay credits nothing" $((before + 5)) "$(balance_of key_free)"
  signed_validate key_free "$token" "$(validate_body sk-1 "$VERIFY_MIDGAR")" "$VERIFY_OLD_PATH"
  check "the path builds in the field call answers a replay the same" already_processed "$(json_get error/code)"
  check "one purchase row exists for the transaction" 1 "$(sql_value "SELECT COUNT(*) FROM credit_purchases WHERE payment_id = 'sk-1'")"

  signed_validate key_free "$token" "$(validate_body sk-2 "$VERIFY_MIDGAR")" "$VERIFY_OLD_PATH"
  check "the path builds in the field call credits a signed purchase too" 200 "$STATUS"
  check "with the same answer" 5 "$(json_get credits_added)"
  check "and the same balance" $((before + 10)) "$(balance_of key_free)"
  signed_validate key_free "$token" "$(validate_body sk-2 "$VERIFY_MIDGAR")"
  check "the new path after it is already processed" already_processed "$(json_get error/code)"
  check "and credits nothing more" $((before + 10)) "$(balance_of key_free)"

  local now_balance=$((before + 10))
  signed_validate key_free "" "$(validate_body sk-3 "$VERIFY_MIDGAR")"
  check "a signed transaction without a Midgar token is 401" 401 "$STATUS"
  signed_validate key_free "$(minted_token second)" "$(validate_body sk-3 "$VERIFY_MIDGAR")"
  refused_leaves_balance "a transaction made by another Midgar ID" 403 forbidden "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_OTHER_MIDGAR")"
  refused_leaves_balance "a transaction whose account token is another Midgar ID" 403 forbidden "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"appAccountToken":null}')"
  refused_leaves_balance "a transaction without an account token" 403 forbidden "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"bundleId":"com.guitaripod.payday","registrySlug":"pay-day"}')"
  refused_leaves_balance "a transaction of another app's bundle" 403 forbidden "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"productId":"com.dreameater.credits.giga"}')"
  refused_leaves_balance "a transaction for another product than the request's" 400 bad_request "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"type":"Non-Consumable"}')"
  refused_leaves_balance "a transaction that is no consumable" 400 bad_request "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"revocationDate":1760000100000}')"
  refused_leaves_balance "a revoked transaction" 400 bad_request "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"inAppOwnershipType":"FAMILY_SHARED"}')"
  refused_leaves_balance "a family-shared transaction" 400 bad_request "$now_balance" key_free
  signed_validate key_free "$token" "$(validate_body sk-3 "$VERIFY_MIDGAR" small '{"quantity":2}')"
  refused_leaves_balance "a purchase of two packs" 400 bad_request "$now_balance" key_free
  signed_validate key_free "$token" "{\"pack_id\":\"small\",\"purchase_token\":\"sk-3\",\"product_id\":\"com.dreameater.credits.small\",\"platform\":\"ios\",\"signed_transaction\":\"not-a-signed-transaction\"}"
  check "a payload the ledger does not verify keeps the text clients retry on" yes "$(json_get error/message | grep -q 'could not be verified' && echo yes || echo no)"
  refused_leaves_balance "a payload the ledger does not verify" 400 unverifiable_purchase "$now_balance" key_free
  signed_validate key_free "$token" "{\"pack_id\":\"small\",\"purchase_token\":\"sk-3\",\"product_id\":\"com.dreameater.credits.small\",\"platform\":\"ios\",\"signed_transaction\":\"stub-down\"}"
  check "a pending answer says the credit follows" yes "$(json_get error/message | grep -q 'credited shortly' && echo yes || echo no)"
  refused_leaves_balance "a ledger that is down" 409 credit_pending "$now_balance" key_free
  check "no refused attempt left a purchase row" 2 "$(sql_value "SELECT COUNT(*) FROM credit_purchases WHERE payment_id LIKE 'sk-%'")"

  signed_validate key_free "$token" "$(validate_body sk-4 "$VERIFY_MIDGAR" small '{"environment":"Sandbox"}')"
  check "a sandbox purchase is credited where the worker is not production" 200 "$STATUS"
  check "and credits the pack" $((now_balance + 5)) "$(balance_of key_free)"

  local sandbox_balance=$((now_balance + 5))
  call POST "$VERIFY_PATH" key_free '{"pack_id":"small","purchase_token":"unsigned-1","product_id":"com.dreameater.credits.small","platform":"ios","signed_transaction":""}'
  check "an empty signed_transaction is pending: the ledger's grant credits such a pack" credit_pending "$(json_get error/code)"
  check "and credits nothing here" "$sandbox_balance" "$(balance_of key_free)"

  call POST /v1/run/chat.completion key_broke '{"messages":[{"role":"user","content":"x"}],"response_json":true}'
  check "an empty wallet is 402" 402 "$STATUS"
  check "the 402 keeps its code" insufficient_credits "$(json_get error/code)"
  check "the 402 carries the balance as a number" 0 "$(json_get error/balance)"
  check "the 402 keeps its message" yes "$(json_get error/message | grep -q '^Insufficient credits' && echo yes || echo no)"
}

verify_with_linking_on() {
  echo "== signed StoreKit transactions through the ledger, wallets on"
  local token before
  token="$(minted_token valid)"
  before="$(balance_of key_link)"

  signed_validate key_link "$token" "$(validate_body sk-w1 "$VERIFY_MIDGAR")"
  check "a verified consumable is credited to the wallet" 200 "$STATUS"
  check "it answers the wallet balance" $((before + 5)) "$(json_get new_balance)"
  check "the wallet holds it" $((before + 5)) "$(balance_of key_link)"
  check "the D1 balance did not move" 0 "$(sql_value "SELECT balance FROM user_credits WHERE user_id = 'u-link'")"
  signed_validate key_link "$token" "$(validate_body sk-w1 "$VERIFY_MIDGAR")"
  check "a replay is already processed" already_processed "$(json_get error/code)"
  internal_call /internal/wallet/grant "$GRANT_SECRET" "$(grant_body sk-w1 "$VERIFY_MIDGAR")"
  check "the ledger's grant of the same transaction is a duplicate" duplicate "$(json_get status)"
  check "and credits nothing more" $((before + 5)) "$(balance_of key_link)"

  internal_call /internal/wallet/grant "$GRANT_SECRET" "$(grant_body sk-w2 "$(upper "$VERIFY_MIDGAR")")"
  check "a grant addressed to the Midgar ID credits the wallet" $((before + 10)) "$(balance_of key_link)"
  signed_validate key_link "$token" "$(validate_body sk-w2 "$VERIFY_MIDGAR")"
  check "the signed call after it is already processed" already_processed "$(json_get error/code)"
  check "and credits nothing more" $((before + 10)) "$(balance_of key_link)"

  signed_validate key_link "$token" "$(validate_body sk-w3 "$VERIFY_OTHER_MIDGAR")"
  check "another Midgar ID's transaction is refused" 403 "$STATUS"
  signed_validate key_link "$token" "{\"pack_id\":\"small\",\"purchase_token\":\"sk-w3\",\"product_id\":\"com.dreameater.credits.small\",\"platform\":\"ios\",\"signed_transaction\":\"stub-down\"}"
  check "a ledger that is down leaves the credit pending" credit_pending "$(json_get error/code)"
  check "and the wallet unchanged" $((before + 10)) "$(balance_of key_link)"

  local native
  native="$(minted_token native)"
  call_native POST /v1/run/chat.completion "$native" '{"messages":[{"role":"user","content":"hello"}],"response_json":true}'
  check "an empty native wallet is 402" 402 "$STATUS"
  check "the wallet's 402 carries the balance as a number" 0 "$(json_get error/balance)"
  check "the wallet's 402 keeps its code" insufficient_credits "$(json_get error/code)"
  call_native POST "$VERIFY_PATH" "$native" "$(validate_body sk-w4 "$VERIFY_NATIVE_MIDGAR")"
  check "a native request credits its own Midgar ID's wallet" 200 "$STATUS"
  call_native GET /v1/credits/balance "$native"
  check "the native wallet holds the pack" 5 "$(json_get balance)"
}
