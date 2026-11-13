# mako

Midgar's shared AI backend: a Rust Cloudflare Worker that puts OpenAI, Gemini and Anthropic Claude behind one credits wallet per Midgar ID, with each app isolated as its own tenant. Pay Day, Psybeam, Double Kick, Psywave, Dream Eater, Living Dex and the Pixie apps all run on one deployment.

> The Worker, its D1 database and R2 bucket keep their original `openai-image-proxy` names (bound by id, invisible to clients). It is reached at `api.midgarcorp.cc/v1/intelligence` through the Midgar gateway, and at `mako.midgarcorp.cc`.

## What it does

- **Tenants** by the `X-App-ID` header: each app has its own credit packs, capability costs and premium entitlement in D1.
- **Credits** reserve, then settle, with a scheduled sweep for orphaned reservations. Packs are App Store purchases whose signed transactions the app posts to the validate route, or Google Play purchases whose tokens mako checks with the Google Play Developer API; the wallet is credited once per store transaction.
- **Premium** users are not charged credits (`credits_charged: 0`). The purchase ledger lists the entitlements of the caller's Midgar ID; a strict tenant keeps the last answer for 24 hours in case the ledger cannot answer.
- **Capabilities** under `/v1/run/<capability>`: `chat.completion` (text and vision, routed by model id), `image` generation and editing (OpenAI, Gemini) and `realtime.translate` (short-lived tokens for metered realtime sessions).
- **Apple Ads attribution**: `POST /v1/attribution` exchanges an AdServices token with Apple and records the campaign, ad group and keyword behind an install; `GET /v1/attribution/keywords` reports installs, payers and revenue per keyword. No advertising identifier, no ATT prompt, and only a SHA-256 fingerprint of the token is stored.

## Providers

Chat is routed by model id prefix (`src/handlers/chat.rs`):

| Prefix | Provider | Secret |
|---|---|---|
| `claude*`, `anthropic*` | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `gpt*`, `o1`, `o3`, `o4`, `chatgpt*` | OpenAI | `OPENAI_API_KEY` |
| everything else | Gemini | `GEMINI_API_KEY` |

### The Anthropic ceiling

Anthropic's monthly credit carries features in every app, so one day's Anthropic spend is capped across all tenants
(`src/spend.rs`). Each answered Anthropic call adds its cost, from its token usage at the model's price in
`model_prices`, to the UTC day's row in `provider_spend`, in millionths of a dollar. Once the day's total reaches
`ANTHROPIC_DAILY_SPEND_LIMIT_USD` (`wrangler.toml`, 2.00 when unset or not a positive number), Anthropic-backed chat
answers `429 rate_limit_exceeded` before anything is reserved or charged, until midnight UTC. OpenAI and Gemini are
not counted or stopped. A count that cannot be read or written is logged and the call goes through. Read the figure:

```sh
npx wrangler d1 execute openai-image-proxy --remote \
  --command "SELECT day, micro_usd / 1000000.0 AS usd, calls FROM provider_spend ORDER BY day DESC LIMIT 14"
```

## Adding a tenant

Data only, no deploy: add `migrations/0XX_seed_<app>.sql` (`014_seed_livingdex.sql` is the model) with the `apps` row and any `capability_costs` and `credit_packs`, then:

```sh
CLOUDFLARE_API_TOKEN=$(cat ~/.cloudflare-api-token) \
  npx wrangler d1 execute openai-image-proxy --remote --file migrations/0XX_seed_<app>.sql
```

## Develop and deploy

```sh
cargo check --target wasm32-unknown-unknown --all-targets   # type-check as CI does
cargo test --lib                                            # unit tests
npx wrangler dev
npx wrangler deploy
```

`docs/SETUP.md` has the full setup and `openapi.yaml` the API.
