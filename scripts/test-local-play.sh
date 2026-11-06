# Sourced by scripts/test-local.sh. Android pack purchases verified with Google Play: prepare_local_google writes a
# throwaway service account (a fresh RSA key whose token_uri is the stub in scripts/local-google) into .dev.vars before
# the server starts, and play_with_linking_off checks the route and the cron. Google's APIs themselves cannot be
# stubbed, so the throwaway access token is refused there: the purchase stays credit_pending and the cron's voided
# purchases sweep is postponed; what the run proves is the parts a unit test cannot reach: mako signs a valid RS256
# assertion with SubtleCrypto, answers not_configured for a tenant without a Play package, credits nothing it could
# not verify, and the cron asks Google for voided purchases without taking anything back or moving its window.

GOOGLE_PORT="${GOOGLE_PORT:-8797}"
GOOGLE_DIR="$IDENTITY_DIR/google"
GOOGLE_ASSERTIONS="$GOOGLE_DIR/assertions.log"
GOOGLE_EMAIL="play-verifier@midgar-local.iam.gserviceaccount.com"
PLAY_PATH="/v1/credits/purchase/validate"

prepare_local_google() {
  mkdir -p "$GOOGLE_DIR" || return 1
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$GOOGLE_DIR/key.pem" 2>>"$LOG" || return 1
  openssl pkey -in "$GOOGLE_DIR/key.pem" -pubout -out "$GOOGLE_DIR/public.pem" 2>>"$LOG" || return 1
  : > "$GOOGLE_ASSERTIONS"
  python3 -I "$ROOT/scripts/local-google/token.py" "$GOOGLE_PORT" "$GOOGLE_ASSERTIONS" >>"$LOG" 2>&1 &
  GOOGLE_PID=$!
  local key_file
  key_file="$(python3 -I -c '
import json, sys
print(json.dumps({
    "type": "service_account", "project_id": "midgar-local", "private_key_id": "local-1",
    "private_key": open(sys.argv[1]).read(), "client_email": sys.argv[2], "token_uri": sys.argv[3],
}))
' "$GOOGLE_DIR/key.pem" "$GOOGLE_EMAIL" "http://127.0.0.1:$GOOGLE_PORT/token")" || return 1
  printf "GOOGLE_PLAY_SERVICE_ACCOUNT='%s'\n" "$key_file" >> "$DEV_VARS"
}

play_body() {
  local app_prefix="$1" pack="$2" token="$3"
  printf '{"pack_id":"%s","purchase_token":"%s","product_id":"%s%s","platform":"android","play_order_id":"GPA.0000-local"}' "$pack" "$token" "$app_prefix" "$pack"
}

assertion_verdict() {
  local assertion
  assertion="$(head -n 1 "$GOOGLE_ASSERTIONS")"
  [ -n "$assertion" ] || { echo "no assertion"; return; }
  python3 -I -c '
import base64, json, sys
header, claims, signature = sys.argv[1].split(".")
decode = lambda part: base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))
open(sys.argv[2] + "/input", "w").write(header + "." + claims)
open(sys.argv[2] + "/signature", "wb").write(decode(signature))
json.dump({"header": json.loads(decode(header)), "claims": json.loads(decode(claims))}, open(sys.argv[2] + "/decoded.json", "w"))
' "$assertion" "$GOOGLE_DIR" || { echo "malformed"; return; }
  if openssl dgst -sha256 -verify "$GOOGLE_DIR/public.pem" -signature "$GOOGLE_DIR/signature" "$GOOGLE_DIR/input" >/dev/null 2>&1; then
    echo "verified"
  else
    echo "bad signature"
  fi
}

assertion_claims() {
  python3 -I -c '
import json, sys
d = json.load(open(sys.argv[1] + "/decoded.json"))
h, c = d["header"], d["claims"]
print(h["alg"], h["kid"], c["iss"] == sys.argv[2], c["aud"], c["scope"], c["exp"] - c["iat"])
' "$GOOGLE_DIR" "$GOOGLE_EMAIL"
}

assertion_count() {
  wc -l < "$GOOGLE_ASSERTIONS" | tr -d ' '
}

play_with_linking_off() {
  echo "== Google Play purchases"
  local before pixie_before signed mark
  before="$(balance_of key_free)"
  call POST "$PLAY_PATH" key_free "$(play_body com.dreameater.credits. small gp-local-1)"
  check "a tenant without a Play package answers not_configured" "503 not_configured" "$STATUS $(json_get error/code)"
  check "and credits nothing" "$before" "$(balance_of key_free)"

  pixie_before="$(balance_of key_pixie pixie)"
  signed="$(assertion_count)"
  call POST "$PLAY_PATH" key_pixie "$(play_body com.guitaripod.pixie.credits. starter '../../edits')" pixie
  check "a token that cannot be a Play purchase is unverifiable" "400 unverifiable_purchase" "$STATUS $(json_get error/code)"
  check "and asks Google nothing" "$signed" "$(assertion_count)"

  call POST "$PLAY_PATH" key_pixie "$(play_body com.guitaripod.pixie.credits. starter gp-local-token-1)" pixie
  check "a purchase Google cannot confirm stays pending" "409 credit_pending" "$STATUS $(json_get error/code)"
  check "mako signed in at the service account's token endpoint" yes "$([ -s "$GOOGLE_ASSERTIONS" ] && echo yes || echo no)"
  check "with an RS256 assertion SubtleCrypto signed under the service account's key" verified "$(assertion_verdict)"
  check "for the Play Developer API scope" \
    "RS256 local-1 True http://127.0.0.1:$GOOGLE_PORT/token https://www.googleapis.com/auth/androidpublisher 3600" "$(assertion_claims)"
  check "nothing was credited" "$pixie_before" "$(balance_of key_pixie pixie)"
  check "and no purchase row was written" 0 "$(sql_value "SELECT COUNT(*) FROM credit_purchases WHERE payment_provider = 'play_store'")"

  mark="$(log_mark)"
  curl -s -o /dev/null "$BASE/__scheduled?cron=*/30+*+*+*+*"
  check "the cron asks Google for PixiePocket's voided purchases" yes "$(log_since_has "$mark" "google play voided sweep of com.guitaripod.pixie postponed")"
  check "a sweep Google refused leaves its window where it was" 0 "$(sql_value "SELECT COUNT(*) FROM play_voided_sweeps")"
  check "and takes nothing back" 0 "$(sql_value "SELECT COUNT(*) FROM purchase_refunds WHERE transaction_id LIKE 'gp:%'")"
  check "the balance is untouched" "$pixie_before" "$(balance_of key_pixie pixie)"
}
