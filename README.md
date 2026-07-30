# mako

**mako** is Midgar's shared, multi-tenant AI-credits backend — a Rust Cloudflare Worker that fronts LLM/image providers (OpenAI, Gemini, Anthropic Claude) behind a single credits ledger, per-app isolation, and RevenueCat entitlement handling. It serves every Midgar app (Helia, Pay Day, Psybeam, Dream Eater, Living Dex, the Pixie image apps, …) from one deployment.

> Deployed as the Cloudflare Worker `openai-image-proxy` at `mako.midgarcorp.cc`. The worker/D1/R2 retain the `openai-image-proxy` names (id-bound, invisible to clients); **mako** is the service identity.

## What it does

- **Multi-tenant** via the `X-App-ID` header; every app has its own credits, packs, capability costs, and premium entitlement in D1.
- **Credits ledger** with a reserve/settle pattern and a scheduled sweep for orphaned reservations.
- **RevenueCat** entitlement verification — premium users bypass credits (`credits_charged: 0`).
- **Capabilities** exposed under `/v1/run/<capability>`:
  - `chat.completion` — text + vision, routed by model id to Gemini / OpenAI / **Anthropic Claude**.
  - `image` generation/editing (OpenAI, Gemini).
  - `realtime.translate` — reserved-minute realtime sessions.
- **Apple Ads attribution** — `POST /v1/attribution` exchanges an AdServices token with Apple server-side and records the campaign/ad group/keyword behind an install; purchases are tagged with it, and `GET /v1/attribution/keywords` reports installs, payers and revenue per keyword. No advertising identifier, no ATT prompt, and the token itself is never stored (only a SHA-256 fingerprint, for idempotency).

## Providers

Chat routing is by model-id prefix (`src/handlers/chat.rs`):

| Prefix | Provider | Secret |
|---|---|---|
| `claude*` / `anthropic*` | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `gpt*` / `o1` / `o3` / `o4` / `chatgpt*` | OpenAI | `OPENAI_API_KEY` |
| everything else | Gemini | `GEMINI_API_KEY` |

## Onboarding a tenant

Data-only, no deploy — add a `migrations/0XX_seed_<app>.sql` (see `migrations/014_seed_livingdex.sql`) with `apps` + optional `capability_costs` + `credit_packs` rows, then:

```sh
CLOUDFLARE_API_TOKEN=$(cat ~/.cloudflare-api-token) \
  npx wrangler d1 execute openai-image-proxy --remote --file migrations/0XX_seed_<app>.sql
```

Set the tenant's RevenueCat secret if it has a premium tier: `wrangler secret put REVENUECAT_IOS_API_KEY_<APPID>`.

## Develop / deploy

```sh
cargo check --target wasm32-unknown-unknown --all-targets   # type-check as CI does
cargo test --lib                              # unit tests (host target)
npx wrangler dev                              # local
npx wrangler deploy                           # production (openai-image-proxy)
```

See `docs/SETUP.md` for full setup and `openapi.yaml` for the API surface.
