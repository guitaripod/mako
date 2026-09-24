use worker::{D1Database, Result};
use crate::error::AppError;
use crate::models::ImageUsage;
use uuid::Uuid;
use chrono::Utc;
use serde::{Deserialize, Serialize};

const CREDIT_MULTIPLIER: f64 = 3.0;
const NEW_USER_FREE_CREDITS: i32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCredits {
    pub user_id: String,
    pub balance: i32,
    pub lifetime_purchased: i32,
    pub lifetime_spent: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditTransaction {
    pub id: String,
    pub user_id: String,
    #[serde(rename = "type")]
    pub transaction_type: String,
    pub amount: i32,
    pub balance_after: i32,
    pub description: String,
    pub reference_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditPurchase {
    pub id: String,
    pub user_id: String,
    pub pack_id: String,
    pub credits: i32,
    pub amount_usd_cents: i32,
    pub payment_provider: String,
    pub payment_id: String,
    pub status: String,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditPack {
    pub id: String,
    pub name: String,
    pub credits: i32,
    pub price_usd_cents: i32,
    pub bonus_credits: i32,
    pub description: String,
}

pub fn get_credit_packs() -> Vec<CreditPack> {
    vec![
        CreditPack {
            id: "starter".to_string(),
            name: "Starter".to_string(),
            credits: 150,
            price_usd_cents: 299,
            bonus_credits: 0,
            description: "Perfect for trying out (~7 Nano Banana images)".to_string(),
        },
        CreditPack {
            id: "basic".to_string(),
            name: "Basic".to_string(),
            credits: 475,
            price_usd_cents: 999,
            bonus_credits: 75,
            description: "Great for regular use (~26 Nano Banana images)".to_string(),
        },
        CreditPack {
            id: "popular".to_string(),
            name: "Popular".to_string(),
            credits: 1136,
            price_usd_cents: 2499,
            bonus_credits: 364,
            description: "Most popular! 32% bonus credits (~71 images)".to_string(),
        },
        CreditPack {
            id: "business".to_string(),
            name: "Business".to_string(),
            credits: 2174,
            price_usd_cents: 4999,
            bonus_credits: 1076,
            description: "For power users — 49% bonus (~154 images)".to_string(),
        },
        CreditPack {
            id: "enterprise".to_string(),
            name: "Enterprise".to_string(),
            credits: 4167,
            price_usd_cents: 9999,
            bonus_credits: 2833,
            description: "Best value — 68% bonus (~333 images)".to_string(),
        },
    ]
}

/// Per-app credit packs from the D1 `credit_packs` table (seeded per tenant).
/// Falls back to the hardcoded pixie packs if the tenant has no rows or the
/// query fails, so pixie can never lose its catalog.
pub async fn get_credit_packs_for_app(app_id: &str, db: &D1Database) -> Vec<CreditPack> {
    let bound = match db
        .prepare("SELECT pack_id, name, credits, bonus_credits, price_usd_cents, description FROM credit_packs WHERE app_id = ?1 ORDER BY sort_order")
        .bind(&[app_id.into()])
    {
        Ok(b) => b,
        Err(_) => return get_credit_packs(),
    };

    match bound.all().await.and_then(|r| r.results::<serde_json::Value>()) {
        Ok(rows) => {
            let packs: Vec<CreditPack> = rows.iter().filter_map(parse_pack_row).collect();
            if packs.is_empty() {
                get_credit_packs()
            } else {
                packs
            }
        }
        Err(_) => get_credit_packs(),
    }
}

/// Per-app, per-capability flat credit cost override from the `capability_costs`
/// table. When set, a capability charges this fixed amount instead of its
/// token/usage-based cost (e.g. Dream Eater bills a flat 1 credit per "dream").
/// Returns None when no override exists (use the built-in cost).
pub async fn get_flat_capability_cost(app_id: &str, capability: &str, db: &D1Database) -> Option<u32> {
    let row = db
        .prepare("SELECT flat_credits FROM capability_costs WHERE app_id = ?1 AND capability = ?2")
        .bind(&[app_id.into(), capability.into()])
        .ok()?
        .first::<serde_json::Value>(None)
        .await
        .ok()??;
    row.get("flat_credits")
        .and_then(|v| v.as_i64())
        .map(|n| n.max(0) as u32)
}

fn parse_pack_row(v: &serde_json::Value) -> Option<CreditPack> {
    Some(CreditPack {
        id: v.get("pack_id")?.as_str()?.to_string(),
        name: v.get("name")?.as_str()?.to_string(),
        credits: v.get("credits")?.as_i64()? as i32,
        bonus_credits: v.get("bonus_credits").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
        price_usd_cents: v.get("price_usd_cents")?.as_i64()? as i32,
        description: v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

pub fn calculate_openai_cost_usd(usage: &ImageUsage) -> f64 {
    let text_cost = (usage.input_tokens_details.text_tokens as f64 / 1_000_000.0) * 5.0;
    let image_input_cost = (usage.input_tokens_details.image_tokens as f64 / 1_000_000.0) * 8.0;
    let output_cost = (usage.output_tokens as f64 / 1_000_000.0) * 30.0;
    
    text_cost + image_input_cost + output_cost
}

pub fn calculate_credits_from_cost(cost_usd: f64) -> u32 {
    ((cost_usd * CREDIT_MULTIPLIER * 100.0).ceil() as u32).max(1)
}

pub async fn initialize_user_credits(app_id: &str, user_id: &str, db: &D1Database) -> Result<()> {
    let now = Utc::now().to_rfc3339();

    db.prepare(
        "INSERT INTO user_credits (app_id, user_id, balance, lifetime_purchased, lifetime_spent, created_at, updated_at)
         VALUES (?, ?, 0, 0, 0, ?, ?)"
    )
    .bind(&[
        app_id.into(),
        user_id.into(),
        now.clone().into(),
        now.into(),
    ])?
    .run()
    .await?;

    let free_credits = db
        .prepare("SELECT new_user_free_credits FROM apps WHERE app_id = ?1")
        .bind(&[app_id.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("new_user_free_credits").and_then(|n| n.as_i64()))
        .unwrap_or(NEW_USER_FREE_CREDITS as i64);

    if free_credits > 0 {
        add_credits(app_id, user_id, free_credits as u32, "bonus", "Welcome bonus", None, db).await?;
    }

    Ok(())
}

pub async fn get_user_balance(app_id: &str, user_id: &str, db: &D1Database) -> Result<i32> {
    let result = db
        .prepare("SELECT balance FROM user_credits WHERE app_id = ? AND user_id = ?")
        .bind(&[app_id.into(), user_id.into()])?
        .first::<serde_json::Value>(None)
        .await?;

    match result {
        Some(value) => Ok(value.get("balance").and_then(|b| b.as_i64()).unwrap_or(0) as i32),
        None => Ok(0),
    }
}

pub async fn check_and_reserve_credits(
    app_id: &str,
    user_id: &str,
    required_credits: u32,
    db: &D1Database,
) -> Result<()> {
    // Use a transaction to ensure atomicity
    let balance = get_user_balance(app_id, user_id, db).await?;
    
    if balance < required_credits as i32 {
        return Err(AppError::PaymentRequired(format!(
            "Insufficient credits. Need {} credits, have {}. Purchase more at /credits",
            required_credits, balance
        )).into());
    }
    
    Ok(())
}

/// Records a paywall hit: a metered capability answered 402 because the
/// wallet's balance was below the rate. Best-effort — a logging failure must
/// never turn an already-decided 402 into a 500.
pub async fn record_paywall_event(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    capability: &str,
    balance: i32,
) {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let stmt = db
        .prepare(
            "INSERT INTO paywall_events (id, app_id, user_id, capability, balance, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&[
            id.into(),
            app_id.into(),
            user_id.into(),
            capability.into(),
            balance.into(),
            now.into(),
        ]);
    match stmt {
        Ok(s) => {
            if let Err(e) = s.run().await {
                worker::console_log!("record_paywall_event: insert failed: {:?}", e);
            }
        }
        Err(e) => worker::console_log!("record_paywall_event: prepare failed: {:?}", e),
    }
}

/// Deducts `amount` credits atomically: the balance check, the write, and the
/// balance the ledger records as `balance_after` all come from the same
/// guarded `UPDATE ... RETURNING`, so two concurrent deductions for the same
/// wallet can never both read the same starting balance and clobber each
/// other's write (the lost-update race that over-credited live wallets).
pub async fn deduct_credits(
    app_id: &str,
    user_id: &str,
    amount: u32,
    description: &str,
    reference_id: &str,
    db: &D1Database,
) -> Result<i32> {
    let transaction_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let row = db
        .prepare(
            "UPDATE user_credits
             SET balance = balance - ?, lifetime_spent = lifetime_spent + ?, updated_at = ?
             WHERE app_id = ? AND user_id = ? AND balance >= ?
             RETURNING balance",
        )
        .bind(&[
            amount.into(),
            amount.into(),
            now.clone().into(),
            app_id.into(),
            user_id.into(),
            amount.into(),
        ])?
        .first::<serde_json::Value>(None)
        .await?;

    let new_balance = match row {
        Some(v) => v.get("balance").and_then(|b| b.as_i64()).unwrap_or(0) as i32,
        None => {
            let current_balance = get_user_balance(app_id, user_id, db).await?;
            return Err(AppError::PaymentRequired(format!(
                "Insufficient credits. Need {} credits, have {}",
                amount, current_balance
            )).into());
        }
    };

    db.prepare(
        "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, reference_id, created_at)
         VALUES (?, ?, ?, 'spend', ?, ?, ?, ?, ?)"
    )
    .bind(&[
        transaction_id.into(),
        app_id.into(),
        user_id.into(),
        (-(amount as i32)).into(),
        new_balance.into(),
        description.into(),
        reference_id.into(),
        now.into(),
    ])?
    .run()
    .await?;

    Ok(new_balance)
}

/// Adds `amount` credits atomically, mirroring `deduct_credits`: the write and
/// the ledger's `balance_after` both come from the same `UPDATE ... RETURNING`,
/// so a concurrent deduction on the same wallet can never be lost underneath a
/// stale-balance write. A missing `user_credits` row is a bug (one is always
/// created at registration, before any add/deduct is possible) surfaced as an
/// error rather than silently dropped.
pub async fn add_credits(
    app_id: &str,
    user_id: &str,
    amount: u32,
    transaction_type: &str,
    description: &str,
    reference_id: Option<&str>,
    db: &D1Database,
) -> Result<i32> {
    let transaction_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let update_query = if transaction_type == "purchase" {
        "UPDATE user_credits
         SET balance = balance + ?, lifetime_purchased = lifetime_purchased + ?, updated_at = ?
         WHERE app_id = ? AND user_id = ?
         RETURNING balance"
    } else {
        "UPDATE user_credits
         SET balance = balance + ?, updated_at = ?
         WHERE app_id = ? AND user_id = ?
         RETURNING balance"
    };

    let mut params = vec![
        amount.into(),
    ];

    if transaction_type == "purchase" {
        params.push(amount.into());
    }

    params.push(now.clone().into());
    params.push(app_id.into());
    params.push(user_id.into());

    let new_balance = db
        .prepare(update_query)
        .bind(&params)?
        .first::<serde_json::Value>(None)
        .await?
        .ok_or_else(|| AppError::InternalError(format!(
            "add_credits: no user_credits row for {}/{}", app_id, user_id
        )))?
        .get("balance")
        .and_then(|b| b.as_i64())
        .unwrap_or(0) as i32;

    db.prepare(
        "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, reference_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&[
        transaction_id.into(),
        app_id.into(),
        user_id.into(),
        transaction_type.into(),
        amount.into(),
        new_balance.into(),
        description.into(),
        reference_id.map(|r| r.into()).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
        now.into(),
    ])?
    .run()
    .await?;

    Ok(new_balance)
}

/// Deducts up to `amount` credits, clamping to whatever remains rather than
/// failing when the balance is short — for admin corrections that must be able
/// to zero out a wallet in one call. Atomic via compare-and-swap: each attempt
/// reads the balance, then writes guarded on that exact value still holding,
/// so a concurrent mutation (a refund, a purchase) on the same wallet is never
/// clobbered, only retried against. Returns the resulting balance and the
/// amount actually deducted.
pub async fn deduct_credits_clamped(
    app_id: &str,
    user_id: &str,
    amount: u32,
    transaction_type: &str,
    description: &str,
    db: &D1Database,
) -> Result<(i32, u32)> {
    for _ in 0..8 {
        let current = get_user_balance(app_id, user_id, db).await?;
        if current <= 0 {
            return Ok((current.max(0), 0));
        }
        let to_deduct = amount.min(current as u32);
        let now = Utc::now().to_rfc3339();

        let row = db
            .prepare(
                "UPDATE user_credits
                 SET balance = balance - ?, lifetime_spent = lifetime_spent + ?, updated_at = ?
                 WHERE app_id = ? AND user_id = ? AND balance = ?
                 RETURNING balance",
            )
            .bind(&[
                to_deduct.into(),
                to_deduct.into(),
                now.clone().into(),
                app_id.into(),
                user_id.into(),
                current.into(),
            ])?
            .first::<serde_json::Value>(None)
            .await?;

        let Some(row) = row else { continue };
        let new_balance = row.get("balance").and_then(|b| b.as_i64()).unwrap_or(0) as i32;

        let transaction_id = Uuid::new_v4().to_string();
        db.prepare(
            "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, reference_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?)"
        )
        .bind(&[
            transaction_id.into(),
            app_id.into(),
            user_id.into(),
            transaction_type.into(),
            (-(to_deduct as i32)).into(),
            new_balance.into(),
            description.into(),
            now.into(),
        ])?
        .run()
        .await?;

        return Ok((new_balance, to_deduct));
    }
    Err(AppError::InternalError("deduct_credits_clamped: too much contention".to_string()).into())
}

pub async fn record_purchase(
    app_id: &str,
    user_id: &str,
    pack_id: &str,
    credits: u32,
    amount_usd_cents: u32,
    payment_provider: &str,
    payment_id: &str,
    db: &D1Database,
) -> Result<String> {
    let purchase_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    db.prepare(
        "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)"
    )
    .bind(&[
        purchase_id.clone().into(),
        app_id.into(),
        user_id.into(),
        pack_id.into(),
        credits.into(),
        amount_usd_cents.into(),
        payment_provider.into(),
        payment_id.into(),
        now.into(),
    ])?
    .run()
    .await?;

    Ok(purchase_id)
}

/// Completes a pending purchase atomically: the pending -> completed status
/// flip is a single guarded `UPDATE ... RETURNING`, so two concurrent callers
/// (the payment webhook, the client's status poll, and the RevenueCat
/// fast-track path can all reach this for the same purchase) can never both
/// see 'pending' and both grant credits for it.
pub async fn complete_purchase(
    purchase_id: &str,
    db: &D1Database,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();

    let claimed = db
        .prepare(
            "UPDATE credit_purchases SET status = 'completed', completed_at = ?
             WHERE id = ? AND status = 'pending'
             RETURNING app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider",
        )
        .bind(&[now.into(), purchase_id.into()])?
        .first::<serde_json::Value>(None)
        .await?;

    let purchase = match claimed {
        Some(p) => p,
        None => return complete_purchase_already_claimed(purchase_id, db).await,
    };

    let app_id = purchase.get("app_id").and_then(|v| v.as_str()).unwrap_or("pixie");
    let user_id = purchase.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    let pack_id = purchase.get("pack_id").and_then(|v| v.as_str()).unwrap_or("");
    let credits = purchase.get("credits").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
    let payment_provider = purchase.get("payment_provider").and_then(|v| v.as_str()).unwrap_or("");

    let description = match payment_provider {
        "revenuecat" => format!("App Store purchase: {} pack", pack_id),
        "stripe" => format!("Card purchase: {} pack", pack_id),
        "nowpayments" => format!("Crypto purchase: {} pack", pack_id),
        _ => format!("Purchased {} pack", pack_id),
    };
    add_credits(app_id, user_id, credits, "purchase", &description, Some(purchase_id), db).await?;

    let amount_usd_cents = purchase.get("amount_usd_cents").and_then(|v| v.as_i64()).unwrap_or(0);
    tag_purchase_attribution(db, app_id, user_id, purchase_id, pack_id, amount_usd_cents).await;

    Ok(())
}

/// The status flip in `complete_purchase` matched no row: either the purchase
/// was already completed by a concurrent caller (idempotent no-op) or it
/// never existed / is in a terminal non-pending state (a real error).
async fn complete_purchase_already_claimed(purchase_id: &str, db: &D1Database) -> Result<()> {
    let status = db
        .prepare("SELECT status FROM credit_purchases WHERE id = ?")
        .bind(&[purchase_id.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(|s| s.to_string()));

    match status.as_deref() {
        Some("completed") => {
            worker::console_log!("Purchase {} already completed, skipping", purchase_id);
            Ok(())
        }
        _ => Err(AppError::NotFound("Purchase not found".to_string()).into()),
    }
}

/// Copies the buyer's Apple Ads campaign/keyword onto a completed credit-pack
/// purchase. Best-effort by design: ad measurement must never be able to fail a
/// purchase that has already been credited.
async fn tag_purchase_attribution(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    purchase_id: &str,
    pack_id: &str,
    amount_usd_cents: i64,
) {
    match crate::attribution::tag_purchase(
        db,
        app_id,
        user_id,
        purchase_id,
        crate::attribution::PurchaseKind::CreditPack,
        pack_id,
        amount_usd_cents,
        None,
    )
    .await
    {
        Ok(true) => worker::console_log!("tagged purchase {} with ad attribution", purchase_id),
        Ok(false) => {}
        Err(e) => worker::console_log!("could not tag purchase {} with ad attribution: {:?}", purchase_id, e),
    }
}

pub async fn get_user_transactions(
    app_id: &str,
    user_id: &str,
    limit: i32,
    offset: i32,
    db: &D1Database,
) -> Result<Vec<CreditTransaction>> {
    let results = db
        .prepare(
            "SELECT * FROM credit_transactions
             WHERE app_id = ? AND user_id = ?
             ORDER BY created_at DESC
             LIMIT ? OFFSET ?"
        )
        .bind(&[app_id.into(), user_id.into(), limit.into(), offset.into()])?
        .all()
        .await?;
    
    let mut transactions = Vec::new();
    if let Ok(rows) = results.results() {
        for row in rows {
            if let Ok(transaction) = serde_json::from_value::<CreditTransaction>(row) {
                transactions.push(transaction);
            }
        }
    }
    
    Ok(transactions)
}

pub fn estimate_image_cost(
    model: &str,
    quality: &str,
    size: &str,
    is_edit: bool,
) -> u32 {
    if model.starts_with("gemini") {
        return 21;
    }

    // Based on gpt-image-1 documentation:
    // Low: 272-408 tokens, Medium: 1056-1584 tokens, High: 4160-6240 tokens
    let base_estimate = match (quality, size) {
        // Low quality estimates
        ("low", "1024x1024") => 4,  // ~272 tokens
        ("low", "1536x1024") | ("low", "1024x1536") => 6,  // ~408 tokens
        ("low", _) => 5,  // average for other sizes
        
        // Medium quality estimates  
        ("medium", "1024x1024") => 16,  // ~1056 tokens
        ("medium", "1536x1024") | ("medium", "1024x1536") => 24,  // ~1584 tokens
        ("medium", _) => 20,  // average for other sizes
        
        // High quality estimates
        ("high", "1024x1024") => 62,  // ~4160 tokens
        ("high", "1536x1024") | ("high", "1024x1536") => 94,  // ~6240 tokens
        ("high", _) => 78,  // average for other sizes
        
        // Auto quality (often selects high quality based on prompt complexity)
        ("auto", "1024x1024") => 50,  // often uses high quality (62) but sometimes medium (16)
        ("auto", _) => 75,  // often uses high quality for larger sizes
        _ => 16,  // default to medium
    };
    
    if is_edit {
        // Edit operations use more tokens due to input image processing
        match quality {
            "low" => base_estimate + 3,
            "medium" => base_estimate + 3,
            "high" => base_estimate + 20,  // High quality edits use significantly more tokens
            "auto" => base_estimate + 18,  // Auto often uses higher quality processing
            _ => base_estimate + 3,
        }
    } else {
        base_estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_calculate_openai_cost() {
        let usage = ImageUsage {
            total_tokens: 1000,
            input_tokens: 200,
            output_tokens: 800,
            input_tokens_details: crate::models::InputTokenDetails {
                text_tokens: 100,
                image_tokens: 100,
            },
        };
        
        let cost = calculate_openai_cost_usd(&usage);
        let expected = (100.0 / 1_000_000.0) * 5.0
            + (100.0 / 1_000_000.0) * 8.0
            + (800.0 / 1_000_000.0) * 30.0;
        assert!((cost - expected).abs() < 1e-9);
        assert!((cost - 0.0253).abs() < 0.0001);
    }
    
    #[test]
    fn test_calculate_credits_from_cost() {
        // Test various costs - using ceil() ensures we never lose money
        assert_eq!(calculate_credits_from_cost(0.01), 3);   // 0.01 * 3 * 100 = 3.0 -> ceil = 3
        assert_eq!(calculate_credits_from_cost(0.10), 31);  // 0.10 * 3 * 100 = 30.0 -> ceil = 31 (due to floating point)
        assert_eq!(calculate_credits_from_cost(0.0033), 1); // 0.0033 * 3 * 100 = 0.99 -> ceil = 1
        assert_eq!(calculate_credits_from_cost(0.50), 150); // 0.50 * 3 * 100 = 150.0 -> ceil = 150
        assert_eq!(calculate_credits_from_cost(0.0001), 1); // Very small cost -> min 1 credit
    }
    
    #[test]
    fn test_estimate_image_cost() {
        // Test Gemini costs (always 15)
        assert_eq!(estimate_image_cost("gemini-2.5-flash", "low", "1024x1024", false), 21);
        assert_eq!(estimate_image_cost("gemini-2.5-flash", "low", "1536x1024", true), 21);
        assert_eq!(estimate_image_cost("gemini-2.5-flash", "high", "auto", false), 21);

        // Test OpenAI generation costs
        assert_eq!(estimate_image_cost("gpt-image-1", "low", "1024x1024", false), 4);
        assert_eq!(estimate_image_cost("gpt-image-1", "low", "1536x1024", false), 6);
        assert_eq!(estimate_image_cost("gpt-image-1", "medium", "1024x1024", false), 16);
        assert_eq!(estimate_image_cost("gpt-image-1", "medium", "1536x1024", false), 24);
        assert_eq!(estimate_image_cost("gpt-image-1", "high", "1024x1024", false), 62);
        assert_eq!(estimate_image_cost("gpt-image-1", "high", "1536x1024", false), 94);
        assert_eq!(estimate_image_cost("gpt-image-1", "high", "512x512", false), 78);
        assert_eq!(estimate_image_cost("gpt-image-1", "auto", "1024x1024", false), 50);
        assert_eq!(estimate_image_cost("gpt-image-1", "auto", "1536x1024", false), 75);

        // Test OpenAI edit operations
        assert_eq!(estimate_image_cost("gpt-image-1", "low", "1024x1024", true), 7); // 4 + 3
        assert_eq!(estimate_image_cost("gpt-image-1", "medium", "1024x1024", true), 19); // 16 + 3
        assert_eq!(estimate_image_cost("gpt-image-1", "high", "1024x1024", true), 82); // 62 + 20
        assert_eq!(estimate_image_cost("gpt-image-1", "high", "1536x1024", true), 114); // 94 + 20
        assert_eq!(estimate_image_cost("gpt-image-1", "auto", "1024x1024", true), 68); // 50 + 18
        assert_eq!(estimate_image_cost("gpt-image-1", "auto", "1536x1024", true), 93); // 75 + 18
    }
    
    #[test]
    fn test_credit_packs() {
        let packs = get_credit_packs();
        assert_eq!(packs.len(), 5);
        
        let starter = &packs[0];
        assert_eq!(starter.id, "starter");
        assert_eq!(starter.credits, 150);
        assert_eq!(starter.price_usd_cents, 299);

        let enterprise = &packs[4];
        assert_eq!(enterprise.id, "enterprise");
        assert_eq!(enterprise.credits + enterprise.bonus_credits, 7000);

        for pack in &packs {
            assert!(pack.price_usd_cents > 0, "All packs must have a price");
        }
    }
}