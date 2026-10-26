> The full guide as it stood on 2026-10-23, kept for its history and detail. The working guide is ../CLAUDE.md.

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

**mako** is Midgar's shared, multi-tenant **AI-credits backend**, a Rust Cloudflare Worker fronting LLM/image providers (OpenAI, Gemini, Anthropic Claude) behind one credits ledger, per-app isolation (`X-App-ID`), and RevenueCat entitlement handling. It serves every Midgar app from one deployment.

> Deployed as the Cloudflare Worker `openai-image-proxy` (D1 db `openai-image-proxy`, R2 `openai-image-proxy-images`) at `mako.midgarcorp.cc`. The worker/D1/R2 keep the `openai-image-proxy` names (id-bound, invisible to clients); **mako** is the service identity.
>
> The client apps that consume this backend (Pixie iOS/Android/CLI) live in a **separate repo: `guitaripod/pixie`**. Do not add app/UI code here.

## Common Commands

```bash
cargo check --target wasm32-unknown-unknown --all-targets   # type-check as CI does
cargo test --lib                              # run the pure-logic unit tests (host target)
npx wrangler dev                              # run locally with hot reload
npx wrangler deploy                           # deploy to production (openai-image-proxy)
npx wrangler tail                             # watch logs

# Database (D1 name is openai-image-proxy)
npx wrangler d1 execute openai-image-proxy --remote --file migrations/0XX_*.sql

# Secrets (MUST use wrangler secret, never config files)
npx wrangler secret put OPENAI_API_KEY        # also GEMINI_API_KEY, ANTHROPIC_API_KEY,
                                              # REVENUECAT_IOS_API_KEY_<APPID>, ...
```

## Providers (chat routing by model-id prefix: `src/handlers/chat.rs`)

- `claude*` / `anthropic*` → Anthropic Messages API (`ANTHROPIC_API_KEY`)
- `gpt*` / `o1` / `o3` / `o4` / `chatgpt*` → OpenAI (`OPENAI_API_KEY`)
- everything else → Gemini (`GEMINI_API_KEY`)

Add a provider by extending `enum Provider`, `for_model()`, `secret_name()`, a `<provider>_generate()` fn, and `model_prices()`.

## Onboarding a tenant (data-only, no deploy)

Add `migrations/0XX_seed_<app>.sql` (model on `migrations/014_seed_livingdex.sql` or `012_seed_helia.sql`): `apps` row + optional `capability_costs` + `credit_packs`, then `wrangler d1 execute openai-image-proxy --remote --file …`. Set `REVENUECAT_IOS_API_KEY_<APPID>` if the app has a premium tier.

## Apple Ads attribution (`src/attribution.rs`, `src/handlers/attribution.rs`)

Traces an App Store ad install to the keyword that produced it, then to whether that user paid, the input to per-keyword pause rules and per-campaign kill rules.

- `POST /v1/attribution`: the app posts the `AAAttribution.attributionToken()` blob on first launch; the worker exchanges it with `https://api-adservices.apple.com/api/v1/` and stores the campaign/ad group/keyword/country against the caller's identity. Always answers 200 with a status (`attributed`, `organic`, `duplicate`, `pending`, `invalid`, `unavailable`) so measurement can never block launch. Apple's 404 = not propagated yet (retry); 400 = terminal.
- `GET /v1/attribution/keywords?since=&until=`: admin-only per-keyword installs / payers / revenue.
- Tables (migration 015): `install_attributions` (one row per identity, PK `(app_id,user_id)`, unique `(app_id,token_hash)`) and `purchase_attributions` (one row per purchase, PK `purchase_id`). Both idempotent by construction; purchases are tagged from `credits::complete_purchase` (packs) and the RevenueCat webhook (subscriptions, keyed `rc:<transaction_id>`).
- `apps.store_net_share` (0.85 Small Business Program / 0.70 otherwise) nets out purchases RevenueCat reports no `takehome_percentage` for. Unset ⇒ the report returns `net_*: null` rather than 0.
- **Privacy invariants**: never store or log the raw token (only its SHA-256 fingerprint); no IDFA, no `ASIdentifierManager`, no ATT prompt. Apps running campaigns disclose this in `src/privacy.rs` (`ad_measurement_section`).

## Tenant switches and v2 request shapes (migrations 020-024)

Every one of these is data on the `apps` row or a new request shape, so a tenant that has not opted in behaves exactly as before.

- **Privacy flags** (`apps.store_prompts`, `apps.store_images`, default 1; `dreameater` 0): 0 keeps prompt text out of `usage_records`/`stored_images` and returns images as base64 without R2. `scripts/scrub-tenant.sh <app_id> --yes [--local]` removes what a tenant already stored. No provider error body, prompt or request body is ever logged (`src/redact.rs`).
- **Chat aliases** (`chat_aliases`, `weekly_usage`, `chat_requests`): a `chat.completion` body with `alias` is resolved server-side (system prompt, JSON schema, max tokens, temperature, flat price, weekly free bucket, premium bypass). Pure logic in `src/chat_alias.rs`, handler in `src/handlers/chat_alias.rs`. Rows are inserted by the operator, never by a migration. A cut-off reply is 422 `truncated` and uncharged.
- **Art** (`image_styles`, `apps.art_credits`): a `/v1/images/generations` body with `style` for a tenant that has styles is composed on the server, charged `art_credits` per image after success, returned as base64 and never stored (`src/art.rs`, `src/handlers/art.rs`).
- **Strict purchases** (`apps.strict_purchases`, default 0): the RevenueCat webhook credits `NON_RENEWING_PURCHASE` (deduplicated with the client validate call on the transaction id), reverses `CANCELLATION` refunds of credit packs once down to a zero balance, and `is_premium_user` requires the tenant's own entitlement and falls back to a 24 hour cached answer on a RevenueCat error. Pure logic in `src/purchases.rs`.
- **Account deletion** removes every table in `USER_DATA_TABLES` (`src/handlers/identity.rs`); a test derives the tables with a `user_id` column from the migrations and fails when one is missing, so add new per-user tables to that list.
- **Hashed per-device API keys** (migrations 025 and 026, `src/api_keys.rs`, `src/auth.rs`): only SHA-256 hashes are stored. A login for an existing user inserts a new row in `api_keys` (`auth::issue_api_key`) and returns that key, so signing in on another device never invalidates the first. Auth tries `api_keys` (refreshing `last_used_at` at most hourly), then the legacy single key `users.api_key_hash`, then the legacy plaintext `users.api_key` with upgrade on use; `lookup_steps` holds that order. `users.api_key` (NOT NULL UNIQUE) holds only the placeholder `hashed:<user id>`; the `*/30` cron backfills 500 legacy rows per run inside Cloudflare. New users go through `NewCredentials::generate`. There is no logout or revoke route yet; one would delete only the presented key's `api_keys` row. User-supplied provider keys are gone: `users.openai_api_key` and `users.gemini_api_key` are unused columns and request bodies no longer accept `openai_api_key`.
- **Tenant spellings and the Midgar token** (P3.07 step 4, `docs/p3.07-wallet-design.md`): `X-App-ID` accepts the mako id (`dreameater`) and the registry slug (`dream-eater`, `apps.registry_slug` from migration 030); `tenant::canonical_app_id` resolves either to the stored `apps.app_id` from an alias table cached 5 minutes per isolate, and everything stored keeps the mako id. `X-Midgar-Token` (`src/midgar_token.rs`) is verified after the API key authenticates: ES256 through SubtleCrypto, `iss`, `aud`, `exp`, `app` equal to the slug of the canonical tenant, keys from identity's JWKS over the `IDENTITY` service binding (cache 5 minutes, one forced refresh a minute on an unknown `kid`, last keys reused while identity is down). A bad token is logged (`midgar_token ignored ... reason=`) and ignored; a valid one is logged (`midgar_token recognised`, id prefix only) and kept on `AuthedUser.midgar`. Every account is still `Account::Local`; `src/ledgers.rs` (`Ledgers`) wraps the balance, check, deduct, add and paywall functions by account for the metered handlers, and `authenticate_with_token` is what those handlers call. The claim-check vectors in `test-vectors/midgar-token.json` come from identity's own signer: regenerate with `node scripts/token-vectors/generate.mjs` after `npm ci` in `services/identity` (a throwaway key, never a real one). `scripts/test-local.sh` runs a stub `midgar-identity` worker (`scripts/local-identity/`) next to mako through a second `--config`, serving a JWKS for a throwaway key it generates per run, and signs its own tokens with `scripts/local-identity/mint.mjs`.
- **Wallet linking** (P3.07 steps 5 and 6, `docs/p3.07-wallet-design.md`; `WALLET_LINKING`, `off` in `wrangler.toml`): off, every account is `Account::Local` and none of this runs, not even a read of `wallet_links`; `on`, `auth::authenticate_with_token` and `auth::authenticate_account` (the credit-aware authentication every handler uses) resolve the account. The first request with an API key and a valid `X-Midgar-Token` runs the saga in `src/wallet/link.rs` (`Linker`, state in `wallet_links`, SQL in `src/wallet/links.rs`): insert `moving`, freeze the D1 balance into the link row, `POST /migrate` to the Midgar ID's `Wallet` object, mark `linked`. The freeze is compare-and-swap on `user_credits.updated_at`, so a concurrent run or a late credit makes it a no-op. A bearer that looks like a Midgar token (`eyJ`, two dots) is a Midgar-ID-native request: the user is the oldest one linked to that ID in the tenant, or a new `provider = 'midgar'` user with a `linked` row and, for tier `app_attest` only, the `welcome:<tenant>` bonus. Every credit change goes through `Ledgers` (`src/ledgers.rs`): `Account::Wallet` runs `src/wallet/ops.rs` with an idempotency key per change (`spend:<reference>`, `tx:<store transaction>`, `stripe:<purchase id>`, `crypto:<purchase id>`, `refund:<reference>`, `settle:<session>`, `sweep:<session>`, `reverse:<store transaction>`, `admin:<uuid>`, `merge:<anonymous user>`), mirrors the totals to `wallet_summary` (best effort) and fails closed with 503 `wallet_unavailable` and `Retry-After`; a local spend or check that answers 402 re-resolves the link once and retries through the wallet. `wallet::target::credit_target` routes RevenueCat grants (a linked user, a local user, a Midgar ID some user is linked to, or a Midgar ID identity confirms over `GET /internal/exists/<id>` with `IDENTITY_LOOKUP_SECRET`). A wallet purchase is a `pending` row, then the idempotent grant, then `completed`. The cron (`Ledgers::run_wallet_cron`) finishes `moving` links older than 5 minutes (20), sweeps credits that landed in a linked user's frozen D1 row under `migrate:<tenant>:<user>:<n>` (20) and completes `pending` RevenueCat purchases older than 5 minutes (50). `DELETE /v1/identity` erases the tenant partition first and fails if the wallet cannot answer. The wallet code is written against `D1Exec` and `WalletTransport`, so `cargo test --lib` runs the saga, with a crash injected after every write, on SQLite with all migrations and in-memory wallet objects (`src/wallet/testing.rs`); `scripts/test-local-wallet.sh` (sourced by `scripts/test-local.sh`) runs the flow against `wrangler dev` with the flag on.
- **StoreKit 2 verification and credit error codes** (P3.07 follow-up, `src/storekit.rs`, `src/handlers/storekit.rs`, `docs/p3.07-wallet-design.md` section 11): a validate body with `signed_transaction` is verified by the ledger's `POST /internal/verify-transaction` over the `LEDGER` binding (secret `LEDGER_VERIFY_SECRET` in `X-Midgar-Internal`, the same value on `midgar-ledger`), then credited once under `tx:<transactionId>` like the RevenueCat webhook. Credit errors carry stable codes next to their unchanged messages: `credit_pending` (409, `Retry-After: 5`, was a masked 500), `already_processed` and `unverifiable_purchase` (400), `insufficient_credits` (402, with a numeric `error.balance`). `AppError::parts()` is the testable form of `to_response()`. `scripts/test-local-verify.sh` runs the checks against a stub ledger (`scripts/local-ledger/`).
- **Image URLs and retention** (migration 033, `src/storage.rs`, `src/retention.rs`): stored images are returned as `{PUBLIC_BASE_URL}/r2/{user}/{image}.png`, `PUBLIC_BASE_URL` a `wrangler.toml` var defaulting to `https://api.midgarcorp.cc/v1/intelligence`; the worker's own host keeps serving `/r2/` for URLs handed out earlier, and the gateway lets `GET /v1/intelligence/r2/<user>/<image>.png` through without `X-App-ID` (image views cannot send headers). The purge is opt-in per tenant: the `*/30` cron deletes up to 200 images per run whose `stored_images.expires_at` (seven days after creation, never extended) has passed, only for tenants with `apps.purge_expired_images = 1` (default 0; migration 033 opts nobody in, so by default nothing is deleted and old URLs stay valid). It deletes the R2 object first, then the image's `image_reports` and the row; a failed object delete is logged and retried next run. Opt a tenant in with `UPDATE apps SET purge_expired_images = 1 WHERE app_id = ...` once its app does not need old URLs. Keep `pixie` (PixiePocket) at 0: its gallery lists a user's own images from `stored_images` and its policy says they stay until deleted. Apply the migration before deploying: the purge query reads `apps.purge_expired_images`, and until the column exists it fails and deletes nothing.
- **Local end-to-end run**: `scripts/test-local.sh` builds, applies every migration to a fresh local D1, seeds users and aliases, and exercises the flows against `wrangler dev --local` with mock providers (`MOCK_PROVIDERS=1` and a non-production `ENVIRONMENT`, both set in a temporary `.dev.vars`, never in `wrangler.toml`). `scripts/smoke-dreameater.sh` is the same check against a real deployment.

## Important Notes

- **Tests**: `cargo test --lib` runs the pure-logic unit tests on the host target (D1/fetch paths are validated with `npx wrangler dev` + `wrangler d1 execute --local`, and against the CLI in `guitaripod/pixie`). CI type-checks tests for wasm but does not run them.
- **Rate limiting**: one concurrent request per user via `user_locks`; locks can get stuck.
- **API compatibility**: `/v1/images/generations` must match OpenAI's format exactly.
- **Build failures**: `cargo install worker-build` first.
- **Migrations**: tables have foreign keys, so order matters.
- **No compiler warnings.**

# No Code comments
- DO NOT ADD CODE COMMENTS. THEY ARE BLOAT!!
