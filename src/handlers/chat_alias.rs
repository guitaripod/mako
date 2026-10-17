use chrono::Utc;
use serde_json::{json, Value};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Response};
use crate::auth::AuthedUser;
use crate::chat_alias::{
    build_anthropic_body, build_gemini_body, build_openai_body, compose_system_prompt, compose_turns,
    free_remaining, key_replay_is_free, ledger_reference, needs_premium_check, parse_alias_request,
    parse_anthropic_reply, parse_gemini_reply, parse_openai_reply, plan_after_lost_weekly_race, plan_charge,
    week_key, week_start, AliasConfig, AliasRequest, ChargeKind, ChargePlan, Reply, ReplyError, Settlement, Turn,
};
use crate::ledgers::Ledgers;
use crate::error::AppError;
use crate::handlers::chat::{Provider, ANTHROPIC_MESSAGES_URL, ANTHROPIC_VERSION, GEMINI_BASE, OPENAI_CHAT_URL};
use crate::rate_limit::{check_and_acquire_lock, release_lock};

/// Serves a chat request that names a server-side alias. The caller has already
/// authenticated the user and applied the write rate limit; this resolves the
/// alias, charges per the alias row, and answers with the additive fields.
pub async fn handle(env: &Env, db: &D1Database, auth: &AuthedUser, body: &Value) -> Result<Response, AppError> {
    let request = parse_alias_request(body).map_err(AppError::BadRequest)?;
    let cfg = load_alias(db, &auth.app_id, &request.alias)
        .await?
        .ok_or_else(|| AppError::BadRequest("Unknown alias".to_string()))?;

    check_and_acquire_lock(&auth.app_id, &auth.user_id, db)
        .await
        .map_err(|_| AppError::RateLimitExceeded)?;
    let outcome = run(env, db, auth, &request, &cfg).await;
    let _ = release_lock(&auth.app_id, &auth.user_id, db).await;
    outcome
}

async fn run(
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    request: &AliasRequest,
    cfg: &AliasConfig,
) -> Result<Response, AppError> {
    let week = week_key(week_start(Utc::now()));
    let settlement = load_settlement(db, &auth.app_id, &auth.user_id, request).await?;
    let used_before = if cfg.weekly_free > 0 {
        weekly_used(db, &auth.app_id, &auth.user_id, &cfg.alias, &week).await
    } else {
        0
    };
    let premium = needs_premium_check(cfg, settlement)
        && crate::handlers::credits::is_premium_user(env, db, &auth.app_id, &auth.user_id).await;
    let mut plan = plan_charge(cfg, premium, used_before, settlement);

    ensure_affordable(env, db, auth, &plan).await?;

    let system = compose_system_prompt(&cfg.system_prompt, request.language.as_deref());
    let turns = compose_turns(&request.messages, &request.input);
    let reply = call_provider(env, cfg, &system, &turns).await?;

    let mut consumed_weekly = false;
    if plan.consumes_weekly {
        if take_from_weekly_bucket(db, auth, cfg, &week).await {
            consumed_weekly = true;
        } else {
            plan = plan_after_lost_weekly_race(cfg);
            ensure_affordable(env, db, auth, &plan).await?;
        }
    }

    let reference = ledger_reference(&request.idempotency_key);
    let mut charge_reference = None;
    if plan.credits > 0 {
        let key = crate::chat_alias::charge_key(&reference, settlement.key_seen, &uuid::Uuid::new_v4().to_string());
        Ledgers::new(env, db)
            .deduct_credits_keyed(&auth.account, plan.credits, &format!("chat.completion:{}", cfg.alias), &reference, &key)
            .await
            .map_err(AppError::from)?;
        charge_reference = Some(reference.clone());
    }

    record_settlement(db, auth, request, cfg, &plan, &reference, settlement).await;

    Ok(respond(cfg, &reply, &plan, charge_reference, free_remaining(cfg, used_before, consumed_weekly))?)
}

async fn ensure_affordable(env: &Env, db: &D1Database, auth: &AuthedUser, plan: &ChargePlan) -> Result<(), AppError> {
    if plan.credits == 0 {
        return Ok(());
    }
    let ledgers = Ledgers::new(env, db);
    if let Err(e) = ledgers.check_and_reserve_credits(&auth.account, plan.credits).await {
        let error = AppError::from(e);
        if matches!(error, AppError::PaymentRequired(_)) {
            let balance = ledgers.get_user_balance(&auth.account).await.unwrap_or(0);
            ledgers.record_paywall_event(&auth.account, "chat.completion", balance).await;
        }
        return Err(error);
    }
    Ok(())
}

fn respond(
    cfg: &AliasConfig,
    reply: &Reply,
    plan: &ChargePlan,
    charge_reference: Option<String>,
    remaining: Option<u32>,
) -> worker::Result<Response> {
    let mut body = json!({
        "content": reply.text,
        "model": cfg.model,
        "credits_charged": plan.credits,
        "usage": {
            "prompt_tokens": reply.prompt_tokens,
            "output_tokens": reply.output_tokens,
            "total_tokens": reply.prompt_tokens + reply.output_tokens,
        },
        "free_remaining": remaining,
        "alias": cfg.alias,
        "version": cfg.version,
    });
    if let Some(reference) = charge_reference {
        body["charge_reference"] = json!(reference);
    }
    Response::from_json(&body)
}

async fn load_alias(db: &D1Database, app_id: &str, alias: &str) -> Result<Option<AliasConfig>, AppError> {
    let row = db
        .prepare(
            "SELECT alias, model, system_prompt, response_schema, max_tokens, temperature, flat_credits,
                    weekly_free, premium_bypass, version
             FROM chat_aliases WHERE app_id = ?1 AND alias = ?2",
        )
        .bind(&[app_id.into(), alias.into()])?
        .first::<Value>(None)
        .await?;
    Ok(row.as_ref().and_then(AliasConfig::from_row))
}

async fn weekly_used(db: &D1Database, app_id: &str, user_id: &str, alias: &str, week: &str) -> u32 {
    let row = match db
        .prepare("SELECT used FROM weekly_usage WHERE app_id = ?1 AND user_id = ?2 AND alias = ?3 AND week_start = ?4")
        .bind(&[app_id.into(), user_id.into(), alias.into(), week.into()])
    {
        Ok(stmt) => stmt.first::<Value>(None).await.ok().flatten(),
        Err(_) => None,
    };
    row.and_then(|r| r.get("used").and_then(|u| u.as_i64())).unwrap_or(0).max(0) as u32
}

/// Takes one call from the weekly bucket with a guarded upsert, so two racing
/// calls can never take more than the alias allows. False when the bucket was
/// already full by the time this call finished.
async fn take_from_weekly_bucket(db: &D1Database, auth: &AuthedUser, cfg: &AliasConfig, week: &str) -> bool {
    let statement = db
        .prepare(
            "INSERT INTO weekly_usage (app_id, user_id, alias, week_start, used) VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(app_id, user_id, alias, week_start) DO UPDATE SET used = used + 1 WHERE used < ?5
             RETURNING used",
        )
        .bind(&[
            auth.app_id.clone().into(),
            auth.user_id.clone().into(),
            cfg.alias.clone().into(),
            week.into(),
            cfg.weekly_free.into(),
        ]);
    match statement {
        Ok(stmt) => matches!(stmt.first::<Value>(None).await, Ok(Some(_))),
        Err(_) => false,
    }
}

async fn load_settlement(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    request: &AliasRequest,
) -> Result<Settlement, AppError> {
    let replays = replays_of_key(db, app_id, user_id, &ledger_reference(&request.idempotency_key)).await?;
    let mut settlement = Settlement {
        key_seen: replays.is_some(),
        key_already_settled: key_replay_is_free(replays),
        ..Settlement::default()
    };
    if let Some(original) = &request.retry_of {
        settlement.retry_of_settled = reference_is_settled(db, app_id, user_id, &ledger_reference(original)).await?;
        settlement.retry_already_used = retry_was_used(db, app_id, user_id, original).await?;
    }
    Ok(settlement)
}

pub(crate) async fn replays_of_key(db: &D1Database, app_id: &str, user_id: &str, reference: &str) -> Result<Option<u32>, AppError> {
    let row = db
        .prepare("SELECT replays FROM chat_requests WHERE app_id = ?1 AND user_id = ?2 AND reference = ?3")
        .bind(&[app_id.into(), user_id.into(), reference.into()])?
        .first::<Value>(None)
        .await?;
    Ok(row.map(|r| r.get("replays").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u32))
}

/// True when the ledger or the request table already holds this reference.
async fn reference_is_settled(db: &D1Database, app_id: &str, user_id: &str, reference: &str) -> Result<bool, AppError> {
    let row = db
        .prepare(
            "SELECT 1 AS hit FROM credit_transactions WHERE app_id = ?1 AND user_id = ?2 AND reference_id = ?3
             UNION ALL
             SELECT 1 AS hit FROM chat_requests WHERE app_id = ?1 AND user_id = ?2 AND reference = ?3
             LIMIT 1",
        )
        .bind(&[app_id.into(), user_id.into(), reference.into()])?
        .first::<Value>(None)
        .await?;
    Ok(row.is_some())
}

async fn retry_was_used(db: &D1Database, app_id: &str, user_id: &str, original: &str) -> Result<bool, AppError> {
    let row = db
        .prepare("SELECT 1 AS hit FROM chat_requests WHERE app_id = ?1 AND user_id = ?2 AND retry_of = ?3 LIMIT 1")
        .bind(&[app_id.into(), user_id.into(), original.into()])?
        .first::<Value>(None)
        .await?;
    Ok(row.is_some())
}

/// Remembers an alias request so the same key or a retry of it is recognised.
async fn record_settlement(
    db: &D1Database,
    auth: &AuthedUser,
    request: &AliasRequest,
    cfg: &AliasConfig,
    plan: &ChargePlan,
    reference: &str,
    settlement: Settlement,
) {
    let granted_retry = if settlement.is_settled() && !settlement.key_already_settled {
        request.retry_of.as_deref()
    } else {
        None
    };
    remember_request(
        db,
        &RememberedRequest {
            app_id: &auth.app_id,
            user_id: &auth.user_id,
            reference,
            alias: &cfg.alias,
            kind: kind_label(plan.kind),
            retry_of: granted_retry,
            is_free_repeat: settlement.key_already_settled,
        },
    )
    .await;
}

pub(crate) struct RememberedRequest<'a> {
    pub app_id: &'a str,
    pub user_id: &'a str,
    pub reference: &'a str,
    pub alias: &'a str,
    pub kind: &'a str,
    pub retry_of: Option<&'a str>,
    pub is_free_repeat: bool,
}

/// Writes the `chat_requests` row for a settled request, or counts a free repeat
/// of one. Best-effort: the reply is already paid for.
pub(crate) async fn remember_request(db: &D1Database, request: &RememberedRequest<'_>) {
    let statement = if request.is_free_repeat {
        db.prepare("UPDATE chat_requests SET replays = replays + 1 WHERE app_id = ?1 AND user_id = ?2 AND reference = ?3")
            .bind(&[request.app_id.into(), request.user_id.into(), request.reference.into()])
    } else {
        db.prepare(
            "INSERT INTO chat_requests (app_id, user_id, reference, alias, kind, retry_of, replays, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
             ON CONFLICT(app_id, user_id, reference) DO UPDATE SET replays = 0, kind = excluded.kind, created_at = excluded.created_at",
        )
        .bind(&[
            request.app_id.into(),
            request.user_id.into(),
            request.reference.into(),
            request.alias.into(),
            request.kind.into(),
            request.retry_of.map(|r| r.into()).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            Utc::now().to_rfc3339().into(),
        ])
    };
    match statement {
        Ok(stmt) => {
            if let Err(e) = stmt.run().await {
                worker::console_log!("chat_requests write failed: {:?}", e);
            }
        }
        Err(e) => worker::console_log!("chat_requests prepare failed: {:?}", e),
    }
}

fn kind_label(kind: ChargeKind) -> &'static str {
    match kind {
        ChargeKind::AlreadySettled => "repeat",
        ChargeKind::Premium => "premium",
        ChargeKind::WeeklyFree => "weekly",
        ChargeKind::Flat => "paid",
    }
}

async fn call_provider(env: &Env, cfg: &AliasConfig, system: &str, turns: &[Turn]) -> Result<Reply, AppError> {
    if crate::mock::enabled(env) {
        return crate::mock::alias_reply(cfg, turns).map_err(reply_error);
    }
    let provider = Provider::for_model(&cfg.model);
    let api_key = env
        .secret(provider.secret_name())
        .map_err(|_| AppError::InternalError(format!("{} not configured", provider.secret_name())))?
        .to_string();

    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let (url, body, label): (String, Value, &str) = match provider {
        Provider::Gemini => {
            headers.set("x-goog-api-key", &api_key)?;
            (format!("{}/{}:generateContent", GEMINI_BASE, cfg.model), build_gemini_body(cfg, system, turns), "Gemini")
        }
        Provider::OpenAI => {
            headers.set("Authorization", &format!("Bearer {}", api_key))?;
            (OPENAI_CHAT_URL.to_string(), build_openai_body(cfg, system, turns), "OpenAI")
        }
        Provider::Anthropic => {
            headers.set("x-api-key", &api_key)?;
            headers.set("anthropic-version", ANTHROPIC_VERSION)?;
            (ANTHROPIC_MESSAGES_URL.to_string(), build_anthropic_body(cfg, system, turns), "Anthropic")
        }
    };
    let value = post_json(&url, headers, &body, label, matches!(provider, Provider::Anthropic)).await?;
    let parsed = match provider {
        Provider::Gemini => parse_gemini_reply(&value),
        Provider::OpenAI => parse_openai_reply(&value),
        Provider::Anthropic => parse_anthropic_reply(&value),
    };
    parsed.map_err(reply_error)
}

fn reply_error(error: ReplyError) -> AppError {
    match error {
        ReplyError::Truncated => AppError::Truncated,
        ReplyError::Blocked => AppError::ContentBlocked,
        ReplyError::Empty => AppError::InternalError("Empty AI response".to_string()),
    }
}

async fn post_json(
    url: &str,
    headers: Headers,
    body: &Value,
    label: &str,
    transient_statuses_back_off: bool,
) -> Result<Value, AppError> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body.to_string())));
    let request = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(request).send().await?;

    let status = resp.status_code();
    if status >= 400 {
        let detail = resp.text().await.unwrap_or_default();
        worker::console_log!("{} chat error {}", label, crate::redact::provider_error_summary(status, &detail));
        if transient_statuses_back_off && (status == 429 || status == 529) {
            return Err(AppError::RateLimitExceeded);
        }
        return Err(AppError::InternalError("AI provider error".to_string()));
    }
    resp.json()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to parse AI response: {}", e)))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WeeklyBalance {
    pub limit: u32,
    pub remaining: u32,
    pub resets_at: String,
}

/// The weekly free buckets this user has in the current UTC week, keyed by
/// alias, for tenants that have any alias with `weekly_free > 0`. None for every
/// other tenant (and when the alias tables do not exist yet), so their balance
/// response stays exactly as it was.
pub async fn weekly_balances(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
) -> Option<std::collections::BTreeMap<String, WeeklyBalance>> {
    let aliases = db
        .prepare("SELECT alias, weekly_free FROM chat_aliases WHERE app_id = ?1 AND weekly_free > 0")
        .bind(&[app_id.into()])
        .ok()?
        .all()
        .await
        .ok()?
        .results::<Value>()
        .ok()?;
    if aliases.is_empty() {
        return None;
    }
    let now = Utc::now();
    let week = week_start(now);
    let key = week_key(week);
    let reset = crate::chat_alias::resets_at(week);
    let mut buckets = std::collections::BTreeMap::new();
    for row in aliases {
        let alias = row.get("alias").and_then(|a| a.as_str())?.to_string();
        let limit = row.get("weekly_free").and_then(|l| l.as_i64()).unwrap_or(0).max(0) as u32;
        let used = weekly_used(db, app_id, user_id, &alias, &key).await;
        buckets.insert(
            alias,
            WeeklyBalance { limit, remaining: crate::chat_alias::weekly_remaining(limit, used), resets_at: reset.clone() },
        );
    }
    Some(buckets)
}
