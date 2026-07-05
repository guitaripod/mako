# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

**mako** is Midgar's shared, multi-tenant **AI-credits backend** — a Rust Cloudflare Worker fronting LLM/image providers (OpenAI, Gemini, Anthropic Claude) behind one credits ledger, per-app isolation (`X-App-ID`), and RevenueCat entitlement handling. It serves every Midgar app from one deployment.

> Deployed as the Cloudflare Worker `openai-image-proxy` (D1 db `openai-image-proxy`, R2 `openai-image-proxy-images`) at `mako.midgarcorp.cc`. The worker/D1/R2 keep the `openai-image-proxy` names (id-bound, invisible to clients); **mako** is the service identity.
>
> The client apps that consume this backend (Pixie iOS/Android/CLI) live in a **separate repo: `guitaripod/pixie`**. Do not add app/UI code here.

## Common Commands

```bash
cargo check --target wasm32-unknown-unknown   # type-check the worker (wasm)
npx wrangler dev                              # run locally with hot reload
npx wrangler deploy                           # deploy to production (openai-image-proxy)
npx wrangler tail                             # watch logs

# Database (D1 name is openai-image-proxy)
npx wrangler d1 execute openai-image-proxy --remote --file migrations/0XX_*.sql

# Secrets (MUST use wrangler secret, never config files)
npx wrangler secret put OPENAI_API_KEY        # also GEMINI_API_KEY, ANTHROPIC_API_KEY,
                                              # REVENUECAT_IOS_API_KEY_<APPID>, ...
```

## Providers (chat routing by model-id prefix — `src/handlers/chat.rs`)

- `claude*` / `anthropic*` → Anthropic Messages API (`ANTHROPIC_API_KEY`)
- `gpt*` / `o1` / `o3` / `o4` / `chatgpt*` → OpenAI (`OPENAI_API_KEY`)
- everything else → Gemini (`GEMINI_API_KEY`)

Add a provider by extending `enum Provider`, `for_model()`, `secret_name()`, a `<provider>_generate()` fn, and `model_prices()`.

## Onboarding a tenant (data-only, no deploy)

Add `migrations/0XX_seed_<app>.sql` (model on `migrations/014_seed_livingdex.sql` or `012_seed_helia.sql`): `apps` row + optional `capability_costs` + `credit_packs`, then `wrangler d1 execute openai-image-proxy --remote --file …`. Set `REVENUECAT_IOS_API_KEY_<APPID>` if the app has a premium tier.

## Important Notes

- **No automated tests** — validate with `npx wrangler dev` and against the CLI in `guitaripod/pixie`.
- **Rate limiting**: one concurrent request per user via `user_locks`; locks can get stuck.
- **API compatibility**: `/v1/images/generations` must match OpenAI's format exactly.
- **Build failures**: `cargo install worker-build` first.
- **Migrations**: tables have foreign keys — order matters.
- **No compiler warnings.**

# No Code comments
- DO NOT ADD CODE COMMENTS. THEY ARE BLOAT!!
