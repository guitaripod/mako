# mako

Midgar's shared multi-tenant AI-credits backend: a Rust Cloudflare Worker fronting OpenAI, Gemini and Anthropic behind one credits ledger, per-app isolation (`X-App-ID`) and RevenueCat entitlements. Deployed as the Worker `openai-image-proxy` (D1 `openai-image-proxy`, R2 `openai-image-proxy-images`) at `mako.midgarcorp.cc`; those names are id-bound, mako is the service identity. Client apps (Pixie) live in `guitaripod/pixie`; add no app or UI code here.

## Layout
- `src/` (`handlers/`, `wallet/`, `auth.rs`, `api_keys.rs`, `ledgers.rs`, `midgar_token.rs`, `purchases.rs`, `storekit.rs`, `storage.rs`, `retention.rs`, `attribution.rs`, `privacy.rs`, `redact.rs`, `chat_alias.rs`, `art.rs`, `tenant.rs`), `migrations/`, `scripts/`, `test-vectors/`, `docs/` (`p3.07-wallet-design.md`).
- Chat routing by model prefix in `src/handlers/chat.rs`: `claude*`/`anthropic*` to Anthropic, `gpt*`/`o1`/`o3`/`o4`/`chatgpt*` to OpenAI, everything else to Gemini. A new provider extends `enum Provider`, `for_model()`, `secret_name()`, a `<provider>_generate()` fn and `model_prices()`.

## Commands
```
cargo check --target wasm32-unknown-unknown --all-targets
cargo test --lib
npx wrangler dev
npx wrangler deploy
npx wrangler tail
npx wrangler d1 execute openai-image-proxy --remote --file migrations/0XX_*.sql
npx wrangler secret put OPENAI_API_KEY
```
- Secrets (`OPENAI_API_KEY`, `GEMINI_API_KEY`, `ANTHROPIC_API_KEY`, `REVENUECAT_IOS_API_KEY_<APPID>`, ...) go in with `wrangler secret`, never config files. Never print them.
- `scripts/test-local.sh` builds, applies every migration to a fresh local D1, seeds users and aliases, and exercises the flows against `wrangler dev --local` with mock providers (`MOCK_PROVIDERS=1` and a non-production `ENVIRONMENT`, both set in a temporary `.dev.vars`, never in `wrangler.toml`). It sources `scripts/test-local-wallet.sh` and runs a stub `midgar-identity` (`scripts/local-identity/`). `scripts/test-local-verify.sh` uses a stub ledger (`scripts/local-ledger/`). `scripts/smoke-dreameater.sh` is the same check against a real deployment.
- `cargo test --lib` runs on the host target (the wallet saga with a crash after every write, on SQLite with all migrations; `wallet/refund_race_tests.rs` enumerates the interleavings of a credit and a refund). `.cargo/config.toml` builds the test SQLite without its global memory-stats lock and `Cargo.toml` optimizes it, because the interleaving search runs tens of thousands of in-memory databases across threads. CI type-checks tests for wasm but does not run them.
- Build failures: `cargo install worker-build` first.

## Rules that break things
- Deploy from arch: `cd ~/Dev/midgar && git pull --ff-only && cd services/intelligence`, then the migration and `npx wrangler deploy`. `~/Dev/rust/mako` on either machine is the read-only mirror; never deploy from it. Run `scripts/test-local.sh` on the mac (arch has no `shasum`). Deploy the gateway before mako when mako starts handing out a new gateway route.
- Migrations: tables have foreign keys, so order matters. Apply a migration before deploying code that reads its columns (the image purge reads `apps.purge_expired_images`; until the column exists it fails and deletes nothing).
- Onboard a tenant with data only: `migrations/0XX_seed_<app>.sql` (model on `014_seed_livingdex.sql` or `012_seed_helia.sql`): `apps` row plus optional `capability_costs` and `credit_packs`, then `wrangler d1 execute ... --remote --file`. Set `REVENUECAT_IOS_API_KEY_<APPID>` if the app has a premium tier.
- Privacy: never store or log the raw attribution token (SHA-256 fingerprint only); no IDFA, `ASIdentifierManager` or ATT prompt; apps running campaigns disclose it in `src/privacy.rs` (`ad_measurement_section`). No provider error body, prompt or request body is ever logged (`src/redact.rs`).
- `apps.store_prompts` and `apps.store_images` (default 1; `dreameater` 0): 0 keeps prompt text out of `usage_records`/`stored_images` and returns images as base64 without R2. `scripts/scrub-tenant.sh <app_id> --yes [--local]` removes stored data.
- Account deletion removes every table in `USER_DATA_TABLES` (`src/handlers/identity.rs`); add new per-user tables there (a test fails otherwise). `DELETE /v1/identity` erases the tenant partition first and fails if the wallet cannot answer.
- Image retention purge is opt-in per tenant (`apps.purge_expired_images`, default 0). Keep `pixie` (PixiePocket) at 0.
- Auth: only SHA-256 hashes of API keys are stored (`api_keys`); signing in on another device must not invalidate the first. No provider keys supplied by users; `openai_api_key` is no longer accepted.
- `/v1/images/generations` must match OpenAI's format exactly.
- Wallet linking is the `WALLET_LINKING` var in `wrangler.toml`, `off` by default: off, every account is `Account::Local` and `wallet_links` is never read. On, the first request with an API key and a valid `X-Midgar-Token` runs the link saga (`src/wallet/link.rs`). Every credit change goes through `Ledgers` (`src/ledgers.rs`); wallet failures answer 503 `wallet_unavailable`.
- `X-Midgar-Token` (`src/midgar_token.rs`): ES256, checks `iss`, `aud`, `exp`, `app`; JWKS from identity over the `IDENTITY` binding. A bad token is ignored and logged. Regenerate `test-vectors/midgar-token.json` with `node scripts/token-vectors/generate.mjs` after `npm ci` in `services/identity` (throwaway key, never a real one).
- `X-App-ID` accepts the mako id (`dreameater`) or the registry slug (`dream-eater`, `apps.registry_slug`); everything stored keeps the mako id.
- Provider URLs live in `src/providers/route.rs` only: every provider call asks `Routing::route(&Provider, tail)`. `AI_GATEWAY_BASE` unset (the committed state) is direct; set, chat and Gemini go through Cloudflare AI Gateway with `cf-aig-authorization` (secret `CF_AIG_TOKEN`) and payload logging off (and `cf-aig-collect-log: false` for `store_prompts = 0` tenants). OpenAI images and the realtime mint stay direct. Do not add the AI Gateway sentence to a privacy page until the gateway is on (pages live in `web/midgarcorp/src/content/legal/`).
- Internal wallet routes (`src/handlers/wallet_internal.rs`, `src/internal.rs`), `X-Midgar-Internal`, 404 when the secret is unset: `POST /internal/wallet/grant` (ledger; `WALLET_INTERNAL_SECRET`, and `LEDGER_GRANTS = "on"`) credits a verified consumable to a linked Midgar ID under `tx:<transactionId>`; `POST /internal/wallet/refund` (ledger; same secret and flag) takes a refunded pack's credits back from the wallet, down to zero, once per transaction under the `purchase_refunds` claim and `reverse:<transactionId>` key the RevenueCat refund path shares (`src/wallet/ledger_refund.rs`, `pack_refund.rs`; both call `pack_refund::claim_and_settle`, which claims first and reads the purchase row second, and every credit path creates its row through `purchase::record_pending`, whose one statement refuses a claimed transaction, so a credit and a refund end never credited or credited and reversed once); a refund for a transaction nothing credited yet is claimed anyway and every credit path (ledger grant `refunded`, validate 400 `refunded`, RevenueCat webhook) then refuses it; `POST /internal/wallet/erase_all` and `/merge` (identity; `WALLET_LIFECYCLE_SECRET`) erase a deleted ID's wallet and move a merged-away ID's wallet into the survivor's (`src/wallet/lifecycle.rs`, `absorb.rs`). The wallet object refuses writes once erased (409 `erased`) or sealed (409 `sealed`). Design: `docs/p3.07-wallet-design.md` section 12.
- StoreKit 2 verification uses the ledger's `POST /internal/verify-transaction` over the `LEDGER` binding (secret `LEDGER_VERIFY_SECRET`, same value on `midgar-ledger`).
- Rate limit: one concurrent request per user via `user_locks`; locks can get stuck.

## Conventions
- No compiler warnings. No code comments.
- Credit errors keep stable codes: `credit_pending` (409), `already_processed`, `unverifiable_purchase` (400), `insufficient_credits` (402).
- Chat aliases and art styles are inserted by the operator, never by a migration. Strict purchases (`apps.strict_purchases`, default 0) and the other tenant switches are data on the `apps` row, so a tenant that has not opted in behaves as before.

## More in docs/agent-notes.md
Providers; Onboarding a tenant; Apple Ads attribution (endpoints, tables, `store_net_share`); Tenant switches and v2 request shapes (privacy flags, chat aliases, art, strict purchases, hashed per-device keys, Midgar token, wallet linking saga and cron, StoreKit 2, image URLs and retention, local end-to-end run); Important Notes.
