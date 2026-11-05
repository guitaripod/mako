//! Per-edit pricing for tenants that sell edits instead of model-priced credits (PixiePocket since 1.8.0, migration
//! 035). A tenant opts in with `apps.credits_per_edit`; its `image_tiers` rows name what a tier costs in edits and
//! which models serve it, in order. An edit is paid from the first source that covers it: a premium subscriber's
//! weekly allowance, then for a standard edit the daily edits of a person's first `runway_days` (the runway, which
//! starts at their first successful edit), then credits at `credits_per_edit` each. Allowance edits are claimed
//! before the provider call and given back when it fails, so a failure never costs anything.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker::{D1Database, Env};

use crate::auth::AuthedUser;
use crate::chat_alias::{resets_at, week_key, week_start};
use crate::error::AppError;

pub const STANDARD: &str = "standard";
pub const MAX: &str = "max";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPricing {
    pub credits_per_edit: u32,
    pub premium_weekly_edits: u32,
    pub runway_days: u32,
    pub runway_daily_edits: u32,
}

/// One model a tier tries, with the quality to ask it for when the provider takes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    pub model: String,
    pub quality: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub name: String,
    pub edits: u32,
    pub chain: Vec<ChainLink>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Premium,
    Runway,
}

impl Bucket {
    fn key(self) -> &'static str {
        match self {
            Bucket::Premium => "premium",
            Bucket::Runway => "runway",
        }
    }
}

/// Edits taken from an allowance for one request, given back with `release` when the request fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub bucket: Bucket,
    pub period: String,
    pub edits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Funding {
    Allowance(Claim),
    Credits(u32),
}

impl Funding {
    pub fn credits(&self) -> u32 {
        match self {
            Funding::Allowance(_) => 0,
            Funding::Credits(credits) => *credits,
        }
    }

    pub fn claim(&self) -> Option<&Claim> {
        match self {
            Funding::Allowance(claim) => Some(claim),
            Funding::Credits(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunwayPhase {
    NotStarted,
    Day(u32),
    Ended,
}

impl RunwayPhase {
    fn is_open(self) -> bool {
        !matches!(self, RunwayPhase::Ended)
    }
}

/// Where a person is in a runway of `days` days that started at `started_at`. A clock earlier than the start reads as
/// the first day, never as an ended runway.
pub fn runway_phase(started_at: Option<DateTime<Utc>>, now: DateTime<Utc>, days: u32) -> RunwayPhase {
    let Some(start) = started_at else {
        return RunwayPhase::NotStarted;
    };
    let elapsed = (now - start).num_days().max(0) as u32;
    if elapsed < days {
        RunwayPhase::Day(elapsed)
    } else {
        RunwayPhase::Ended
    }
}

/// The tier a request names. `max` and the GPT Image 2.5 and 2.6 ids are Max; everything else, `standard` and every
/// legacy Nano Banana or GPT Image id an old client sends, is Standard, so no shipped client lands on an unknown tier.
pub fn tier_name_for(model: &str) -> &'static str {
    if model == MAX || model.starts_with("gpt-image-2.5") || model.starts_with("gpt-image-2.6") {
        MAX
    } else {
        STANDARD
    }
}

/// A tier's `chain` column: a JSON array of `model` or `model:quality` entries, at least one.
pub fn parse_chain(raw: &str) -> Result<Vec<ChainLink>, AppError> {
    let entries: Vec<String> = serde_json::from_str(raw)
        .map_err(|e| AppError::InternalError(format!("image tier chain is not a JSON array of strings: {}", e)))?;
    let chain: Vec<ChainLink> = entries
        .iter()
        .map(|entry| match entry.split_once(':') {
            Some((model, quality)) => ChainLink { model: model.to_string(), quality: Some(quality.to_string()) },
            None => ChainLink { model: entry.clone(), quality: None },
        })
        .collect();
    if chain.is_empty() {
        return Err(AppError::InternalError("image tier chain is empty".to_string()));
    }
    Ok(chain)
}

pub fn day_key(now: DateTime<Utc>) -> String {
    now.date_naive().format("%Y-%m-%d").to_string()
}

fn day_resets_at(now: DateTime<Utc>) -> String {
    format!("{}T00:00:00Z", (now.date_naive() + Duration::days(1)).format("%Y-%m-%d"))
}

fn whole(value: &Value, column: &str) -> Option<u32> {
    value.get(column).and_then(|v| v.as_i64()).filter(|n| *n >= 0).map(|n| n as u32)
}

impl EditPricing {
    /// The tenant's per-edit pricing, or None when it keeps model-priced credits.
    pub async fn load(db: &D1Database, app_id: &str) -> Result<Option<Self>, AppError> {
        let row = db
            .prepare("SELECT credits_per_edit, premium_weekly_edits, runway_days, runway_daily_edits FROM apps WHERE app_id = ?1")
            .bind(&[app_id.into()])
            .map_err(|e| AppError::InternalError(format!("edit pricing: {:?}", e)))?
            .first::<Value>(None)
            .await
            .map_err(|e| AppError::InternalError(format!("edit pricing: {:?}", e)))?;
        Ok(row.as_ref().and_then(Self::from_row))
    }

    fn from_row(row: &Value) -> Option<Self> {
        let credits_per_edit = whole(row, "credits_per_edit").filter(|n| *n > 0)?;
        Some(Self {
            credits_per_edit,
            premium_weekly_edits: whole(row, "premium_weekly_edits").unwrap_or(0),
            runway_days: whole(row, "runway_days").unwrap_or(0),
            runway_daily_edits: whole(row, "runway_daily_edits").unwrap_or(0),
        })
    }

    fn has_runway(&self) -> bool {
        self.runway_days > 0 && self.runway_daily_edits > 0
    }
}

/// The tier a request for `model` lands on, from the tenant's `image_tiers`.
pub async fn load_tier(db: &D1Database, app_id: &str, model: &str) -> Result<Tier, AppError> {
    let name = tier_name_for(model);
    let row = db
        .prepare("SELECT edits, chain FROM image_tiers WHERE app_id = ?1 AND tier = ?2")
        .bind(&[app_id.into(), name.into()])
        .map_err(|e| AppError::InternalError(format!("image tier: {:?}", e)))?
        .first::<Value>(None)
        .await
        .map_err(|e| AppError::InternalError(format!("image tier: {:?}", e)))?
        .ok_or_else(|| AppError::BadRequest(format!("This app has no {} image tier", name)))?;
    let edits = whole(&row, "edits").filter(|n| *n > 0).unwrap_or(1);
    let chain = parse_chain(row.get("chain").and_then(|v| v.as_str()).unwrap_or(""))?;
    Ok(Tier { name: name.to_string(), edits, chain })
}

/// How a request of `n` images on `tier` is paid, claiming allowance edits when an allowance covers it.
pub async fn plan_funding(
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    pricing: &EditPricing,
    tier: &Tier,
    n: u32,
    now: DateTime<Utc>,
) -> Funding {
    let edits = tier.edits * n.max(1);
    let premium = pricing.premium_weekly_edits > 0 && crate::handlers::credits::is_premium_user(env, db, auth).await;
    if premium {
        let week = week_key(week_start(now));
        if try_claim(db, auth, Bucket::Premium, &week, edits, pricing.premium_weekly_edits).await {
            return Funding::Allowance(Claim { bucket: Bucket::Premium, period: week, edits });
        }
    } else if tier.name == STANDARD && pricing.has_runway() {
        let started = runway_start(db, auth).await;
        if runway_phase(started, now, pricing.runway_days).is_open() {
            let day = day_key(now);
            if try_claim(db, auth, Bucket::Runway, &day, edits, pricing.runway_daily_edits).await {
                return Funding::Allowance(Claim { bucket: Bucket::Runway, period: day, edits });
            }
        }
    }
    Funding::Credits(edits * pricing.credits_per_edit)
}

/// Takes `edits` from a bucket with a guarded upsert, so racing requests can never take more than `limit`. False
/// when the bucket cannot cover all of them.
async fn try_claim(db: &D1Database, auth: &AuthedUser, bucket: Bucket, period: &str, edits: u32, limit: u32) -> bool {
    if edits == 0 || edits > limit {
        return false;
    }
    let statement = db
        .prepare(
            "INSERT INTO edit_allowances (app_id, user_id, bucket, period, used) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(app_id, user_id, bucket, period) DO UPDATE SET used = used + ?5 WHERE used + ?5 <= ?6
             RETURNING used",
        )
        .bind(&[
            auth.app_id.clone().into(),
            auth.user_id.clone().into(),
            bucket.key().into(),
            period.into(),
            edits.into(),
            limit.into(),
        ]);
    match statement {
        Ok(stmt) => matches!(stmt.first::<Value>(None).await, Ok(Some(_))),
        Err(_) => false,
    }
}

/// Gives a failed request's allowance edits back.
pub async fn release(db: &D1Database, auth: &AuthedUser, claim: &Claim) {
    let statement = db
        .prepare(
            "UPDATE edit_allowances SET used = MAX(0, used - ?5)
             WHERE app_id = ?1 AND user_id = ?2 AND bucket = ?3 AND period = ?4",
        )
        .bind(&[
            auth.app_id.clone().into(),
            auth.user_id.clone().into(),
            claim.bucket.key().into(),
            claim.period.clone().into(),
            claim.edits.into(),
        ]);
    if let Ok(stmt) = statement {
        if let Err(e) = stmt.run().await {
            worker::console_log!("edits: releasing a claim failed: {:?}", e);
        }
    }
}

/// Starts the person's runway at their first successful edit; later edits leave the start where it is.
pub async fn note_success(db: &D1Database, auth: &AuthedUser, pricing: &EditPricing, now: DateTime<Utc>) {
    if !pricing.has_runway() {
        return;
    }
    let statement = db
        .prepare("INSERT OR IGNORE INTO runways (app_id, user_id, started_at) VALUES (?1, ?2, ?3)")
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into(), now.to_rfc3339().into()]);
    if let Ok(stmt) = statement {
        if let Err(e) = stmt.run().await {
            worker::console_log!("edits: starting the runway failed: {:?}", e);
        }
    }
}

async fn runway_start(db: &D1Database, auth: &AuthedUser) -> Option<DateTime<Utc>> {
    let row = db
        .prepare("SELECT started_at FROM runways WHERE app_id = ?1 AND user_id = ?2")
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into()])
        .ok()?
        .first::<Value>(None)
        .await
        .ok()
        .flatten()?;
    let raw = row.get("started_at")?.as_str()?;
    DateTime::parse_from_rfc3339(raw).ok().map(|t| t.with_timezone(&Utc))
}

async fn used(db: &D1Database, auth: &AuthedUser, bucket: Bucket, period: &str) -> u32 {
    let row = match db
        .prepare("SELECT used FROM edit_allowances WHERE app_id = ?1 AND user_id = ?2 AND bucket = ?3 AND period = ?4")
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into(), bucket.key().into(), period.into()])
    {
        Ok(stmt) => stmt.first::<Value>(None).await.ok().flatten(),
        Err(_) => None,
    };
    row.as_ref().and_then(|r| whole(r, "used")).unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierCost {
    pub tier: String,
    pub edits: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllowanceSummary {
    pub limit: u32,
    pub remaining: u32,
    pub resets_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunwaySummary {
    /// `available` before the first successful edit, `active` during the runway, `ended` after it.
    pub state: String,
    pub days: u32,
    pub daily_limit: u32,
    pub today_remaining: u32,
    pub resets_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
}

/// The edits part of `GET /v1/credits/balance`: what an edit costs, the balance in edits, the tiers, and the premium
/// allowance or the runway, whichever applies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditsSummary {
    pub credits_per_edit: u32,
    pub balance_edits: i32,
    pub tiers: Vec<TierCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub premium: Option<AllowanceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runway: Option<RunwaySummary>,
}

pub fn runway_summary(
    pricing: &EditPricing,
    started_at: Option<DateTime<Utc>>,
    used_today: u32,
    now: DateTime<Utc>,
) -> RunwaySummary {
    let phase = runway_phase(started_at, now, pricing.runway_days);
    let state = match phase {
        RunwayPhase::NotStarted => "available",
        RunwayPhase::Day(_) => "active",
        RunwayPhase::Ended => "ended",
    };
    let today_remaining = if phase.is_open() { pricing.runway_daily_edits.saturating_sub(used_today) } else { 0 };
    RunwaySummary {
        state: state.to_string(),
        days: pricing.runway_days,
        daily_limit: pricing.runway_daily_edits,
        today_remaining,
        resets_at: day_resets_at(now),
        ends_at: started_at.map(|start| (start + Duration::days(pricing.runway_days as i64)).to_rfc3339()),
    }
}

pub async fn summary(
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    pricing: &EditPricing,
    balance: i32,
    now: DateTime<Utc>,
) -> EditsSummary {
    let tiers = tier_costs(db, &auth.app_id).await;
    let premium = pricing.premium_weekly_edits > 0 && crate::handlers::credits::is_premium_user(env, db, auth).await;
    let (premium, runway) = if premium {
        let week = week_start(now);
        let used = used(db, auth, Bucket::Premium, &week_key(week)).await;
        let allowance = AllowanceSummary {
            limit: pricing.premium_weekly_edits,
            remaining: pricing.premium_weekly_edits.saturating_sub(used),
            resets_at: resets_at(week),
        };
        (Some(allowance), None)
    } else if pricing.has_runway() {
        let started = runway_start(db, auth).await;
        let used = used(db, auth, Bucket::Runway, &day_key(now)).await;
        (None, Some(runway_summary(pricing, started, used, now)))
    } else {
        (None, None)
    };
    EditsSummary {
        credits_per_edit: pricing.credits_per_edit,
        balance_edits: balance.max(0) / pricing.credits_per_edit as i32,
        tiers,
        premium,
        runway,
    }
}

async fn tier_costs(db: &D1Database, app_id: &str) -> Vec<TierCost> {
    let rows = match db
        .prepare("SELECT tier, edits FROM image_tiers WHERE app_id = ?1 ORDER BY edits")
        .bind(&[app_id.into()])
    {
        Ok(stmt) => stmt.all().await.ok().and_then(|r| r.results::<Value>().ok()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    rows.iter()
        .filter_map(|row| {
            Some(TierCost { tier: row.get("tier")?.as_str()?.to_string(), edits: whole(row, "edits")? })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
    }

    fn pixie() -> EditPricing {
        EditPricing { credits_per_edit: 21, premium_weekly_edits: 60, runway_days: 7, runway_daily_edits: 5 }
    }

    #[test]
    fn max_and_the_gpt_image_2_5_and_2_6_ids_are_max_and_every_other_id_is_standard() {
        for id in ["max", "gpt-image-2.5-sunburst", "gpt-image-2.5-flare", "gpt-image-2.6-sunburst", "gpt-image-2.6-flare"] {
            assert_eq!(tier_name_for(id), MAX, "{id}");
        }
        for id in ["standard", "gemini-2.5-flash", "gemini-3.1-flash-image", "gemini-3-pro-image", "nano-banana-pro", "gpt-image-1", "gpt-image-2", ""] {
            assert_eq!(tier_name_for(id), STANDARD, "{id}");
        }
    }

    #[test]
    fn a_chain_reads_models_with_and_without_a_quality() {
        let chain = parse_chain(r#"["gemini-nano-banana-2.1", "gpt-image-2:medium"]"#).unwrap();
        assert_eq!(chain, vec![
            ChainLink { model: "gemini-nano-banana-2.1".into(), quality: None },
            ChainLink { model: "gpt-image-2".into(), quality: Some("medium".into()) },
        ]);
        assert!(parse_chain("[]").is_err());
        assert!(parse_chain("gemini").is_err());
    }

    #[test]
    fn the_runway_is_open_for_seven_days_from_its_start_and_never_ends_on_a_clock_set_back() {
        let start = at(2, 15);
        assert_eq!(runway_phase(None, at(2, 15), 7), RunwayPhase::NotStarted);
        assert_eq!(runway_phase(Some(start), at(2, 16), 7), RunwayPhase::Day(0));
        assert_eq!(runway_phase(Some(start), at(9, 14), 7), RunwayPhase::Day(6));
        assert_eq!(runway_phase(Some(start), at(9, 15), 7), RunwayPhase::Ended);
        assert_eq!(runway_phase(Some(start), at(1, 9), 7), RunwayPhase::Day(0));
    }

    #[test]
    fn a_tenant_without_credits_per_edit_keeps_model_prices() {
        assert_eq!(EditPricing::from_row(&serde_json::json!({ "credits_per_edit": null })), None);
        assert_eq!(EditPricing::from_row(&serde_json::json!({ "credits_per_edit": 0 })), None);
        let row = serde_json::json!({ "credits_per_edit": 21, "premium_weekly_edits": 60, "runway_days": 7, "runway_daily_edits": 5 });
        assert_eq!(EditPricing::from_row(&row), Some(pixie()));
    }

    #[test]
    fn the_runway_summary_counts_today_down_and_says_when_it_ends() {
        let pricing = pixie();
        let fresh = runway_summary(&pricing, None, 0, at(2, 10));
        assert_eq!((fresh.state.as_str(), fresh.today_remaining, fresh.ends_at.clone()), ("available", 5, None));
        assert_eq!(fresh.resets_at, "2026-11-03T00:00:00Z");
        let active = runway_summary(&pricing, Some(at(2, 10)), 2, at(4, 9));
        assert_eq!((active.state.as_str(), active.today_remaining), ("active", 3));
        assert_eq!(active.ends_at.as_deref(), Some("2026-11-09T10:00:00+00:00"));
        let ended = runway_summary(&pricing, Some(at(2, 10)), 0, at(9, 10));
        assert_eq!((ended.state.as_str(), ended.today_remaining), ("ended", 0));
    }

    #[test]
    fn funding_reports_credits_only_when_no_allowance_covers_the_request() {
        let claim = Claim { bucket: Bucket::Runway, period: "2026-11-02".into(), edits: 1 };
        assert_eq!(Funding::Allowance(claim.clone()).credits(), 0);
        assert_eq!(Funding::Allowance(claim.clone()).claim(), Some(&claim));
        assert_eq!(Funding::Credits(105).credits(), 105);
        assert_eq!(Funding::Credits(105).claim(), None);
    }
}
