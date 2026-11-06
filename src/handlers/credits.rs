use worker::{Request, Response, RouteContext, Result, Env};
use crate::error::AppError;
use crate::auth;
use crate::credits::{
    get_user_transactions, get_credit_packs, get_credit_packs_for_app,
    record_purchase, estimate_image_cost
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditBalanceResponse {
    pub balance: i32,
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<std::collections::BTreeMap<String, crate::handlers::chat_alias::WeeklyBalance>>,
    /// Edits, tiers and the premium allowance or runway, for a tenant that sells edits (migration 035).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<crate::edits::EditsSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateCostRequest {
    pub prompt: Option<String>,
    pub quality: String,
    pub size: String,
    pub n: Option<u8>,
    pub is_edit: Option<bool>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateCostResponse {
    pub estimated_credits: u32,
    pub estimated_usd: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurchaseRequest {
    pub pack_id: String,
    pub payment_provider: String,
    pub payment_id: String,
    pub payment_currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateStripeCheckoutRequest {
    pub pack_id: String,
    pub success_url: String,
    pub cancel_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateStripeCheckoutResponse {
    pub checkout_url: String,
    pub session_id: String,
    pub purchase_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StripeConfigResponse {
    pub publishable_key: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAdjustCreditsRequest {
    pub user_id: String,
    pub amount: i32,
    pub reason: String,
}

pub async fn get_balance(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let db = env.d1("DB")?;
    let auth = match auth::authenticate_with_token(&req, &env, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    if let Err(e) = crate::rate_limit::enforce_read_rate_limit(&env, &auth.app_id, &auth.user_id, "balance").await {
        return e.to_response();
    }

    let balance = match crate::ledgers::Ledgers::new(&env, &db).get_user_balance(&auth.account).await {
        Ok(balance) => balance,
        Err(e) => return crate::error::wallet_outage_response(e),
    };

    let weekly = crate::handlers::chat_alias::weekly_balances(&db, &auth.app_id, &auth.user_id).await;
    let edits = match crate::edits::EditPricing::load(&db, &auth.app_id).await {
        Ok(Some(pricing)) => Some(crate::edits::summary(&env, &db, &auth, &pricing, balance, chrono::Utc::now()).await),
        _ => None,
    };

    Response::from_json(&CreditBalanceResponse {
        balance,
        currency: "credits".to_string(),
        weekly,
        edits,
    })
}

pub async fn list_transactions(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    let url = req.url()?;
    let query_params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .into_owned()
        .collect();
    
    let page = query_params
        .get("page")
        .and_then(|p| p.parse::<i32>().ok())
        .unwrap_or(1)
        .max(1);
    
    let per_page = query_params
        .get("per_page")
        .and_then(|p| p.parse::<i32>().ok())
        .unwrap_or(50)
        .min(100);
    
    let offset = (page - 1) * per_page;

    let db = env.d1("DB")?;
    let auth = match auth::authenticate_account(&req, &env, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    let user_id = auth.user_id.clone();
    let app_id = auth.app_id.clone();
    if let Err(e) = crate::rate_limit::enforce_read_rate_limit(&env, &app_id, &user_id, "transactions").await {
        return e.to_response();
    }

    if let crate::ledgers::Account::Wallet { .. } = &auth.account {
        let ledgers = crate::ledgers::Ledgers::new(&env, &db);
        return match ledgers.wallet_transactions(&auth.account, offset, per_page).await {
            Ok((transactions, total)) => Response::from_json(&json!({
                "transactions": transactions,
                "total": total,
                "limit": per_page,
                "offset": offset,
                "page": page,
                "per_page": per_page,
            })),
            Err(e) => crate::error::wallet_outage_response(e),
        };
    }

    let count_stmt = db.prepare("SELECT COUNT(*) as total FROM credit_transactions WHERE app_id = ? AND user_id = ?");
    let count_result = count_stmt
        .bind(&[app_id.clone().into(), user_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .unwrap_or(json!({"total": 0}));
    let total = count_result.get("total").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    let transactions = get_user_transactions(&app_id, &user_id, per_page, offset, &db).await?;
    
    Response::from_json(&json!({
        "transactions": transactions,
        "total": total,
        "limit": per_page,
        "offset": offset,
        "page": page,
        "per_page": per_page,
    }))
}

pub async fn list_packs(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let app_id = auth::resolve_app_id(&req);
    let cache_key = format!("https://mako.midgarcorp.cc/__cache/credits/packs/{}", app_id);
    let cache = worker::Cache::default();
    if let Ok(Some(cached)) = cache.get(&cache_key, false).await {
        return Ok(cached);
    }
    let db = ctx.env.d1("DB")?;
    let packs = get_credit_packs_for_app(&app_id, &db).await;
    let mut resp = Response::from_json(&json!({ "packs": packs }))?;
    resp.headers_mut().set("Cache-Control", "public, max-age=60")?;
    if let Ok(copy) = resp.cloned() {
        let _ = cache.put(&cache_key, copy).await;
    }
    Ok(resp)
}

pub async fn estimate_cost(mut req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    let estimate_req: EstimateCostRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };
    
    let n = estimate_req.n.unwrap_or(1);
    let is_edit = estimate_req.is_edit.unwrap_or(false);
    let model = estimate_req.model.as_deref().unwrap_or("gemini-2.5-flash");

    let credits_per_image = estimate_image_cost(model, &estimate_req.quality, &estimate_req.size, is_edit);
    
    let total_credits = credits_per_image * n as u32;
    
    Response::from_json(&EstimateCostResponse {
        estimated_credits: total_credits,
        estimated_usd: format!("${:.2}", total_credits as f64 / 100.0),
        note: if model.starts_with("gemini") {
            format!("Gemini flat rate: {} credits per image", credits_per_image)
        } else {
            format!(
                "Actual cost may vary ±{} credits based on prompt complexity",
                (total_credits as f64 * 0.15) as u32 + 1
            )
        },
    })
}

pub async fn purchase_credits(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let auth = {
        let db = env.d1("DB")?;
        match auth::authenticate_account(&req, &env, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        }
    };
    let user_id = auth.user_id.clone();
    let app_id = auth.app_id.clone();

    let purchase_req: PurchaseRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };

    let db = env.d1("DB")?;
    let packs = get_credit_packs_for_app(&app_id, &db).await;
    let pack = packs
        .iter()
        .find(|p| p.id == purchase_req.pack_id)
        .ok_or_else(|| AppError::BadRequest("Invalid pack_id".to_string()))?;

    let total_credits = pack.credits + pack.bonus_credits;

    match purchase_req.payment_provider.as_str() {
        "stripe" => AppError::BadRequest(
            "Card payments start at POST /v1/credits/purchase/stripe, which creates the checkout session".to_string()
        ).to_response(),
        "nowpayments" => {
            if purchase_req.pack_id == "starter" {
                return AppError::BadRequest(
                    "Crypto payments are not available for the Starter pack due to minimum transaction requirements. Please use a credit/debit card or choose a larger pack.".to_string()
                ).to_response();
            }
            
            let purchase_id = record_purchase(
                &app_id,
                &user_id,
                &purchase_req.pack_id,
                total_credits as u32,
                pack.price_usd_cents as u32,
                &purchase_req.payment_provider,
                "",  // We'll update with payment_id after creating crypto payment
                &db,
            ).await?;
            
            let crypto_currency = purchase_req.payment_currency
                .as_deref()
                .unwrap_or("btc");
            
            let crypto_response = crate::crypto_payments::create_crypto_payment(
                &env,
                &purchase_id,
                &pack.name,
                pack.price_usd_cents as f64 / 100.0,
                total_credits as u32,
                crypto_currency,
            ).await?;
            
            // Update purchase with crypto payment_id
            db.prepare(
                "UPDATE credit_purchases SET payment_id = ? WHERE id = ?"
            )
            .bind(&[
                crypto_response.payment_id.clone().into(),
                purchase_id.into(),
            ])?
            .run()
            .await?;
            
            Response::from_json(&crypto_response)
        },
        _ => AppError::BadRequest("Invalid payment provider".to_string()).to_response()
    }
}

// Admin endpoints
pub async fn admin_adjust_credits(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let auth = {
        let db = env.d1("DB")?;
        match auth::authenticate_account(&req, &env, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        }
    };
    if !auth.is_admin {
        return AppError::Forbidden("Admin access required".to_string()).to_response();
    }
    let admin_user_id = auth.user_id.clone();
    let app_id = auth.app_id.clone();

    let adjust_req: AdminAdjustCreditsRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };
    
    let db = env.d1("DB")?;

    let exists = db
        .prepare("SELECT 1 FROM users WHERE app_id = ? AND id = ?")
        .bind(&[app_id.clone().into(), adjust_req.user_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?;
    if exists.is_none() {
        return AppError::NotFound("User not found in this app".to_string()).to_response();
    }

    let description = format!("Admin adjustment by {}: {}", admin_user_id, adjust_req.reason);
    
    let ledgers = crate::ledgers::Ledgers::new(&env, &db);
    let adjusted = match ledgers.account_for_user(&app_id, &adjust_req.user_id).await {
        Ok(target) => admin_adjust(&ledgers, &target, adjust_req.amount, &description).await,
        Err(e) => Err(e),
    };
    let new_balance = match adjusted {
        Ok(balance) => balance,
        Err(e) => return crate::error::wallet_outage_response(e),
    };
    
    Response::from_json(&json!({
        "user_id": adjust_req.user_id,
        "adjustment": adjust_req.amount,
        "new_balance": new_balance,
        "reason": adjust_req.reason,
    }))
}

/// Applies an admin adjustment to an account: a credit once under a fresh `admin:<uuid>` key, or a debit clamped to
/// what the account holds. Returns the balance after.
async fn admin_adjust(ledgers: &crate::ledgers::Ledgers<'_>, target: &crate::ledgers::Account, amount: i32, description: &str) -> Result<i32> {
    use crate::wallet::ops::{Grant, Reversal};
    let key = format!("admin:{}", uuid::Uuid::new_v4());
    if amount > 0 {
        let grant = Grant { key, kind: crate::wallet::Kind::Adjust, source: "admin", amount: amount as u32, description, reference: None };
        return Ok(ledgers.grant(target, &grant).await?.balance);
    }
    let reversal = Reversal { key, kind: crate::wallet::Kind::Adjust, source: "admin", amount: amount.unsigned_abs(), description, reference: None };
    Ok(ledgers.reverse(target, &reversal, "admin_adjustment").await?.0)
}

pub async fn admin_search_users(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let app_id = {
        let db = env.d1("DB")?;
        let auth = match auth::authenticate_account(&req, &env, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        };
        if !auth.is_admin {
            return AppError::Forbidden("Admin access required".to_string()).to_response();
        }
        auth.app_id
    };

    let url = req.url()?;
    let search = url.query_pairs()
        .find(|(k, _)| k == "search")
        .map(|(_, v)| v.to_string());
    
    let db = env.d1("DB")?;
    
    let results = if let Some(search_term) = search {
        // Clean up the search term - trim whitespace and normalize
        let cleaned_search = search_term.trim();
        if cleaned_search.is_empty() {
            return AppError::BadRequest("Search term cannot be empty".to_string()).to_response();
        }
        
        // Search by ID (exact match with trimmed spaces) or email (flexible matching)
        let query = "
            SELECT u.id, u.email, u.is_admin, u.created_at,
                   COALESCE(uc.balance, 0) as credits
            FROM users u
            LEFT JOIN user_credits uc ON uc.app_id = u.app_id AND uc.user_id = u.id
            WHERE u.app_id = ?4
              AND (u.id = ?1
               OR LOWER(TRIM(u.email)) = LOWER(TRIM(?1))
               OR LOWER(u.email) LIKE LOWER(?2)
               OR LOWER(u.email) LIKE LOWER(?3))
            ORDER BY
                CASE
                    WHEN u.id = ?1 OR LOWER(TRIM(u.email)) = LOWER(TRIM(?1)) THEN 0
                    ELSE 1
                END,
                u.created_at DESC
            LIMIT 10
        ";

        // Create variations for flexible email matching
        let email_prefix = format!("{}%", cleaned_search);
        let email_contains = format!("%{}%", cleaned_search);

        let stmt = db.prepare(query);
        stmt.bind(&[
            cleaned_search.into(),
            email_prefix.into(),
            email_contains.into(),
            app_id.clone().into()
        ])?
            .all()
            .await?
    } else {
        // Return recent users if no search term
        let query = "
            SELECT u.id, u.email, u.is_admin, u.created_at,
                   COALESCE(uc.balance, 0) as credits
            FROM users u
            LEFT JOIN user_credits uc ON uc.app_id = u.app_id AND uc.user_id = u.id
            WHERE u.app_id = ?1
            ORDER BY u.created_at DESC
            LIMIT 10
        ";

        let stmt = db.prepare(query);
        stmt.bind(&[app_id.clone().into()])?
            .all()
            .await?
    };
    
    let users: Vec<serde_json::Value> = results
        .results::<serde_json::Value>()?
        .into_iter()
        .map(|user| {
            json!({
                "id": user.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "email": user.get("email").and_then(|v| v.as_str()),
                "is_admin": user.get("is_admin").and_then(|v| v.as_i64()).map(|v| v != 0).unwrap_or(false),
                "credits": user.get("credits").and_then(|v| v.as_i64()).unwrap_or(0),
                "created_at": user.get("created_at").and_then(|v| v.as_str()).unwrap_or("")
            })
        })
        .collect();
    
    let users = add_wallet_balances(&env, &db, &app_id, users).await;
    Response::from_json(&users)
}

/// The wallet balances, purchases and spends of a tenant from the advisory `wallet_summary` mirror, all zero while
/// linking is off so the admin numbers stay what they were.
async fn wallet_summary_totals(env: &Env, db: &worker::D1Database, app_id: &str) -> Result<(i64, i64, i64)> {
    if !crate::wallet::link::enabled(env) {
        return Ok((0, 0, 0));
    }
    let row = db
        .prepare("SELECT COALESCE(SUM(balance), 0) AS balance, COALESCE(SUM(purchased), 0) AS purchased, COALESCE(SUM(spent), 0) AS spent FROM wallet_summary WHERE app_id = ?")
        .bind(&[app_id.into()])?
        .first::<serde_json::Value>(None)
        .await?;
    let read = |column: &str| row.as_ref().and_then(|r| r.get(column)).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok((read("balance"), read("purchased"), read("spent")))
}

/// Adds each listed user's wallet balance, from the mirror, to the credits the D1 row shows. Linking off leaves the
/// list untouched.
async fn add_wallet_balances(env: &Env, db: &worker::D1Database, app_id: &str, mut users: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if !crate::wallet::link::enabled(env) {
        return users;
    }
    for user in users.iter_mut() {
        let id = user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let statement = db
            .prepare(
                "SELECT s.balance AS balance FROM wallet_links l
                 JOIN wallet_summary s ON s.app_id = l.app_id AND s.midgar_id = l.midgar_id
                 WHERE l.app_id = ?1 AND l.user_id = ?2",
            )
            .bind(&[app_id.into(), id.into()]);
        let row = match statement {
            Ok(statement) => statement.first::<serde_json::Value>(None).await.ok().flatten(),
            Err(_) => None,
        };
        if let Some(balance) = row.and_then(|r| r.get("balance").and_then(|v| v.as_i64())) {
            let credits = user.get("credits").and_then(|v| v.as_i64()).unwrap_or(0);
            user["credits"] = json!(credits + balance);
        }
    }
    users
}

pub async fn admin_system_stats(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let db = env.d1("DB")?;
    let auth = match auth::authenticate_account(&req, &env, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    if !auth.is_admin {
        return AppError::Forbidden("Admin access required".to_string()).to_response();
    }
    let app_id = auth.app_id.clone();

    // Tenant-scoped statistics (per app_id)
    let total_users = db
        .prepare("SELECT COUNT(*) as count FROM users WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("count").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let total_credits_balance = db
        .prepare("SELECT SUM(balance) as total FROM user_credits WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("total").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let total_purchased = db
        .prepare("SELECT SUM(lifetime_purchased) as total FROM user_credits WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("total").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let total_spent = db
        .prepare("SELECT SUM(lifetime_spent) as total FROM user_credits WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("total").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let (wallet_balance, wallet_purchased, wallet_spent) = wallet_summary_totals(&env, &db, &app_id).await?;
    let total_credits_balance = total_credits_balance + wallet_balance;
    let total_purchased = total_purchased + wallet_purchased;
    let total_spent = total_spent + wallet_spent;

    let total_revenue = db
        .prepare("SELECT SUM(amount_usd_cents) as total FROM credit_purchases WHERE app_id = ? AND status = 'completed'")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("total").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let total_images = db
        .prepare("SELECT COUNT(*) as count FROM stored_images WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("count").and_then(|c| c.as_i64()))
        .unwrap_or(0);

    let openai_costs = db
        .prepare("SELECT SUM(openai_cost_cents) as total FROM stored_images WHERE app_id = ?")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("total").and_then(|c| c.as_i64()))
        .unwrap_or(0);
    
    Response::from_json(&json!({
        "users": {
            "total": total_users,
        },
        "credits": {
            "total_balance": total_credits_balance,
            "total_purchased": total_purchased,
            "total_spent": total_spent,
        },
        "revenue": {
            "total_usd": format!("${:.2}", total_revenue as f64 / 100.0),
            "openai_costs_usd": format!("${:.2}", openai_costs as f64 / 100.0),
            "gross_profit_usd": format!("${:.2}", (total_revenue - openai_costs) as f64 / 100.0),
            "profit_margin": format!("{:.1}%", ((total_revenue - openai_costs) as f64 / total_revenue as f64) * 100.0),
        },
        "images": {
            "total_generated": total_images,
        }
    }))
}

pub async fn crypto_payment_webhook(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    
    // Get the signature from headers
    let signature = req.headers().get("x-nowpayments-sig")?
        .ok_or_else(|| AppError::BadRequest("Missing signature header".to_string()))?;
    
    // Get the raw body for signature verification
    let body = req.text().await?;
    
    // Verify webhook signature
    if !crate::crypto_payments::verify_webhook_signature(&env, &signature, &body)? {
        return AppError::Unauthorized("Invalid webhook signature".to_string()).to_response();
    }
    
    // Parse the webhook data
    let webhook: crate::crypto_payments::NOWPaymentsWebhook = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid webhook body: {}", e)))?;
    
    // Only process confirmed payments
    if webhook.payment_status == "finished" || webhook.payment_status == "confirmed" {
        let db = env.d1("DB")?;
        
        // Complete the purchase using the order_id (which is our purchase_id)
        crate::ledgers::Ledgers::new(&env, &db).complete_purchase(&webhook.order_id).await?;
    }
    
    Response::ok("OK")
}

pub async fn get_purchase_status(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let purchase_id = ctx.param("purchase_id")
        .ok_or_else(|| AppError::BadRequest("Missing purchase_id".to_string()))?
        .to_string();
    let env = ctx.env;

    let db = env.d1("DB")?;
    let caller = match auth::authenticate_account(&req, &env, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };

    let purchase = match db
        .prepare("SELECT status, payment_provider, payment_id FROM credit_purchases WHERE id = ? AND app_id = ? AND user_id = ?")
        .bind(&[purchase_id.clone().into(), caller.app_id.clone().into(), caller.user_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
    {
        Some(row) => row,
        None => return AppError::NotFound("Purchase not found".to_string()).to_response(),
    };
    
    let status = purchase.get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    
    let payment_provider = purchase.get("payment_provider")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    
    // For crypto payments, get live status from NOWPayments
    if payment_provider == "nowpayments" && status == "pending" {
        let payment_id = purchase.get("payment_id")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        
        if !payment_id.is_empty() {
            match crate::crypto_payments::get_payment_status(&env, payment_id).await {
                Ok(crypto_status) => {
                    // If payment is finished, complete the purchase
                    if crypto_status == "finished" || crypto_status == "confirmed" {
                        crate::ledgers::Ledgers::new(&env, &db).complete_purchase(&purchase_id).await?;
                        
                        Response::from_json(&json!({
                            "purchase_id": purchase_id,
                            "status": "completed",
                            "payment_status": crypto_status,
                        }))
                    } else {
                        Response::from_json(&json!({
                            "purchase_id": purchase_id,
                            "status": status,
                            "payment_status": crypto_status,
                        }))
                    }
                },
                Err(_) => {
                    Response::from_json(&json!({
                        "purchase_id": purchase_id,
                        "status": status,
                    }))
                }
            }
        } else {
            Response::from_json(&json!({
                "purchase_id": purchase_id,
                "status": status,
            }))
        }
    }
    // For Stripe payments, get live status from Stripe
    else if payment_provider == "stripe" && status == "pending" {
        let session_id = purchase.get("payment_id")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        
        if !session_id.is_empty() {
            match crate::stripe_payments::get_checkout_session(&env, session_id).await {
                Ok(session) => {
                    if crate::stripe_payments::session_pays_for(&session, &purchase_id, crate::stripe_payments::test_mode_allowed(&env)) {
                        crate::ledgers::Ledgers::new(&env, &db).complete_purchase(&purchase_id).await?;
                        
                        Response::from_json(&json!({
                            "purchase_id": purchase_id,
                            "status": "completed",
                            "payment_status": session.payment_status,
                            "payment_method": "stripe",
                        }))
                    } else {
                        Response::from_json(&json!({
                            "purchase_id": purchase_id,
                            "status": status,
                            "payment_status": session.payment_status,
                            "payment_method": "stripe",
                        }))
                    }
                },
                Err(_) => {
                    Response::from_json(&json!({
                        "purchase_id": purchase_id,
                        "status": status,
                        "payment_method": "stripe",
                    }))
                }
            }
        } else {
            Response::from_json(&json!({
                "purchase_id": purchase_id,
                "status": status,
                "payment_method": "stripe",
            }))
        }
    } else {
        Response::from_json(&json!({
            "purchase_id": purchase_id,
            "status": status,
        }))
    }
}

pub async fn create_stripe_checkout(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let user_id = {
        let db = env.d1("DB")?;
        let auth = match auth::authenticate_account(&req, &env, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        };

        let email = db
            .prepare("SELECT email FROM users WHERE app_id = ? AND id = ?")
            .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into()])?
            .first::<serde_json::Value>(None)
            .await?
            .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(|s| s.to_string()));

        (auth.user_id, auth.app_id, email)
    };

    let checkout_req: CreateStripeCheckoutRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };
    
    let tenant_packs = {
        let db = env.d1("DB")?;
        get_credit_packs_for_app(&user_id.1, &db).await
    };
    let card = match card_pack(&tenant_packs, &checkout_req.pack_id) {
        Some(pack) => pack,
        None => return AppError::BadRequest(
            format!("The '{}' pack is not available for card payments in this app.", checkout_req.pack_id)
        ).to_response(),
    };
    let pack = &card;

    // Get Stripe price ID for this pack
    let stripe_price_id = crate::stripe_payments::get_stripe_price_id(&env, &checkout_req.pack_id)
        .ok_or_else(|| AppError::BadRequest(
            format!("The '{}' pack is not available for card payments. Please try a different pack or payment method.", checkout_req.pack_id)
        ))?;
    
    let total_credits = pack.credits + pack.bonus_credits;
    
    let db = env.d1("DB")?;
    
    // Record the purchase in pending state
    let purchase_id = record_purchase(
        &user_id.1,
        &user_id.0,
        &checkout_req.pack_id,
        total_credits as u32,
        pack.price_usd_cents as u32,
        "stripe",
        "", // We'll update with session_id after creating
        &db,
    ).await?;
    
    // Create Stripe checkout session
    let session = crate::stripe_payments::create_checkout_session(
        &env,
        &purchase_id,
        &checkout_req.pack_id,
        &pack.name,
        total_credits as u32,
        &stripe_price_id,
        &checkout_req.success_url,
        &checkout_req.cancel_url,
        user_id.2.as_deref(),
    ).await?;
    
    // Update purchase with Stripe session ID
    db.prepare(
        "UPDATE credit_purchases SET payment_id = ? WHERE id = ?"
    )
    .bind(&[
        session.id.clone().into(),
        purchase_id.clone().into(),
    ])?
    .run()
    .await?;
    
    Response::from_json(&CreateStripeCheckoutResponse {
        checkout_url: session.url.unwrap_or_else(|| "".to_string()),
        session_id: session.id,
        purchase_id,
    })
}

pub async fn stripe_webhook(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    
    // Get Stripe signature header
    let signature = req.headers().get("stripe-signature")?
        .ok_or_else(|| AppError::BadRequest("Missing stripe-signature header".to_string()))?;
    
    // Extract timestamp from signature
    let sig_parts: Vec<&str> = signature.split(',').collect();
    let mut timestamp = "";
    for part in &sig_parts {
        let kv: Vec<&str> = part.split('=').collect();
        if kv.len() == 2 && kv[0] == "t" {
            timestamp = kv[1];
            break;
        }
    }
    
    // Get the raw body for signature verification
    let body = req.text().await?;
    
    // Verify webhook signature
    if !crate::stripe_payments::verify_webhook_signature(&env, &signature, &body, timestamp)? {
        return AppError::Unauthorized("Invalid webhook signature".to_string()).to_response();
    }
    
    // Parse the webhook event
    let event: crate::stripe_payments::StripeWebhookEvent = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid webhook body: {}", e)))?;
    
    worker::console_log!("Received Stripe webhook event: {}", event.event_type);
    
    // Handle different event types
    match event.event_type.as_str() {
        "checkout.session.completed" => {
            // Extract the session from the event
            let session: crate::stripe_payments::StripeCheckoutSession = serde_json::from_value(event.data.object)
                .map_err(|e| AppError::InternalError(format!("Failed to parse session: {}", e)))?;
            
            if let Some(purchase_id) = session.metadata.get("purchase_id") {
                if crate::stripe_payments::session_pays_for(&session, purchase_id, crate::stripe_payments::test_mode_allowed(&env)) {
                    let db = env.d1("DB")?;
                    crate::ledgers::Ledgers::new(&env, &db).complete_purchase(purchase_id).await?;
                    worker::console_log!("Completed purchase: {}", purchase_id);
                } else if !session.livemode {
                    worker::console_log!("Ignored a test-mode Stripe session in production for purchase {}", purchase_id);
                }
            }
        },
        "payment_intent.succeeded" => {
            worker::console_log!("Payment intent succeeded");
        },
        "payment_intent.payment_failed" => {
            worker::console_log!("Payment intent failed");
        },
        _ => {
            worker::console_log!("Unhandled event type: {}", event.event_type);
        }
    }
    
    Response::ok("OK")
}

pub async fn get_stripe_config(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    
    // Check if Stripe is configured
    let publishable_key = env.var("STRIPE_PUBLISHABLE_KEY")
        .ok()
        .map(|k| k.to_string())
        .unwrap_or_default();
    
    let enabled = !publishable_key.is_empty() && 
                  env.secret("STRIPE_SECRET_KEY").is_ok();
    
    Response::from_json(&StripeConfigResponse {
        publishable_key,
        enabled,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatePurchaseRequest {
    pub pack_id: String,
    pub purchase_token: String,
    pub product_id: String,
    pub platform: String,
    /// The StoreKit 2 signed transaction (JWS) of the purchase, which the ledger verifies. Builds from before
    /// MidgarStore send none; their packs are credited from Apple's notification through the ledger's grant instead.
    #[serde(default)]
    pub signed_transaction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatePurchaseResponse {
    pub success: bool,
    pub purchase_id: String,
    pub credits_added: u32,
    pub new_balance: i32,
}

/// `POST /v1/credits/purchase/validate` and its original path `POST /v1/credits/purchase/revenuecat/validate`, which
/// the apps in the field call: binds the claimed product to the claimed pack of this tenant and credits the pack once
/// the ledger has verified the StoreKit 2 transaction (`storekit::validate_signed`). A request without a signed
/// transaction comes from a build older than MidgarStore and is answered `credit_pending` without a grant: Apple's
/// notification reaches the ledger, whose grant credits the pack to the user named in the transaction's
/// `appAccountToken`.
pub async fn validate_purchase(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;

    let auth = {
        let db = env.d1("DB")?;
        match auth::authenticate_account(&req, &env, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        }
    };
    let app_id = auth.app_id.clone();

    let validate_req: ValidatePurchaseRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };

    if validate_req.platform != "ios" && validate_req.platform != "android" {
        return AppError::BadRequest("Invalid platform. Must be 'ios' or 'android'".to_string()).to_response();
    }

    if validate_req.purchase_token.trim().is_empty() || validate_req.product_id.trim().is_empty() {
        return AppError::BadRequest("Missing purchase_token or product_id".to_string()).to_response();
    }

    let db = env.d1("DB")?;

    let product_prefix = db
        .prepare("SELECT store_product_prefix FROM apps WHERE app_id = ?1")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("store_product_prefix").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let expected_product = format!("{}{}", product_prefix, validate_req.pack_id);
    if !product_prefix.is_empty() && validate_req.product_id != expected_product {
        return AppError::BadRequest("product_id does not match pack_id".to_string()).to_response();
    }

    let packs = get_credit_packs_for_app(&app_id, &db).await;
    let pack = packs
        .iter()
        .find(|p| p.id == validate_req.pack_id)
        .ok_or_else(|| AppError::BadRequest("Invalid pack_id".to_string()))?;

    if validate_req.signed_transaction.as_deref().is_some_and(|signed| !signed.trim().is_empty()) {
        return crate::handlers::storekit::validate_signed(&req, &env, &db, &auth, &validate_req, pack).await;
    }
    AppError::CreditPending(crate::storekit::PENDING_MESSAGE.to_string()).to_response()
}

/// True if the caller holds the app's configured premium entitlement, as the purchase ledger lists it for the caller's
/// Midgar ID (non-spoofable: the ledger verified every transaction with Apple). Only apps with a non-null
/// `premium_entitlement` ask the ledger; everyone else returns false with a single indexed D1 read, and so does a
/// caller without a Midgar ID.
///
/// A tenant that is not on strict purchases treats a ledger that cannot answer as not premium, so an outage never hands
/// out free unlimited usage. A strict tenant remembers each answer and, while the ledger cannot answer, uses the last
/// one from the previous 24 hours instead of treating a paying user as free.
pub async fn is_premium_user(env: &Env, db: &worker::D1Database, auth: &auth::AuthedUser) -> bool {
    let app_id = auth.app_id.as_str();
    let user_id = auth.user_id.as_str();
    let row = match db
        .prepare("SELECT * FROM apps WHERE app_id = ?1")
        .bind(&[app_id.into()])
    {
        Ok(stmt) => stmt.first::<serde_json::Value>(None).await.ok().flatten(),
        Err(_) => None,
    };
    let row = match row {
        Some(r) => r,
        None => return false,
    };
    let required = row
        .get("premium_entitlement")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let Some(required) = required else {
        return false;
    };
    let Some(midgar_id) = premium_midgar_id(auth) else {
        return false;
    };
    let policy = crate::tenant::TenantPolicy::from_row(&row);
    let strict = policy.strict_purchases || policy.strict_premium;
    let answer = ledger_grants_premium(env, app_id, midgar_id, &premium_tokens(user_id, midgar_id), &required).await;
    match (answer, strict) {
        (Some(premium), true) => {
            store_premium_cache(db, app_id, user_id, premium).await;
            premium
        }
        (Some(premium), false) => premium,
        (None, true) => read_premium_cache(db, app_id, user_id).await.unwrap_or(false),
        (None, false) => false,
    }
}

/// The Midgar ID whose purchases count for this caller: the verified token's, else the wallet the account is linked to.
fn premium_midgar_id(auth: &auth::AuthedUser) -> Option<&str> {
    auth.midgar.as_ref().map(|caller| caller.midgar_id.as_str()).or_else(|| auth.account.midgar_id())
}

/// Whether the purchase ledger lists the tenant's premium entitlement for this Midgar ID, in either environment.
/// `None` when the ledger could not answer for an environment that did not grant it, so the caller can tell an outage
/// from a no.
async fn ledger_grants_premium(env: &Env, app_id: &str, midgar_id: &str, also_tokens: &[String], required: &str) -> Option<bool> {
    let Ok(ledger) = crate::storekit::LedgerService::open(env) else {
        return None;
    };
    let slug = crate::tenant::registry_slug(app_id);
    let mut answered = true;
    for environment in PREMIUM_ENVIRONMENTS {
        match ledger.entitlements(midgar_id, environment, also_tokens).await {
            Ok(held) if crate::storekit::grants_entitlement(&held, &slug, required) => return Some(true),
            Ok(_) => {}
            Err(failure) => {
                answered = false;
                worker::console_log!("premium: the ledger could not list entitlements for {}: {:?}", app_id, failure);
            }
        }
    }
    answered.then_some(false)
}

/// The caller's other ids whose purchases are theirs too: the mako user id, which app builds from before MidgarStore
/// wrote into the `appAccountToken` of every purchase. The ledger, which gets every renewal from Apple, counts those
/// subscriptions under that token. Only a UUID can be a token.
fn premium_tokens(user_id: &str, midgar_id: &str) -> Vec<String> {
    if user_id.eq_ignore_ascii_case(midgar_id) || uuid::Uuid::parse_str(user_id).is_err() {
        return Vec::new();
    }
    vec![user_id.to_lowercase()]
}

/// The ledger environments whose entitlements count. Sandbox subscriptions count too, so App Review and TestFlight
/// buyers get what they paid for.
const PREMIUM_ENVIRONMENTS: [&str; 2] = ["Production", "Sandbox"];

async fn store_premium_cache(db: &worker::D1Database, app_id: &str, user_id: &str, premium: bool) {
    let stmt = db
        .prepare("UPDATE users SET premium_cached = ?1, premium_checked_at = ?2 WHERE id = ?3 AND app_id = ?4")
        .bind(&[
            (premium as i32).into(),
            chrono::Utc::now().to_rfc3339().into(),
            user_id.into(),
            app_id.into(),
        ]);
    if let Ok(stmt) = stmt {
        if let Err(e) = stmt.run().await {
            worker::console_log!("premium cache write failed: {:?}", e);
        }
    }
}

async fn read_premium_cache(db: &worker::D1Database, app_id: &str, user_id: &str) -> Option<bool> {
    let row = db
        .prepare("SELECT premium_checked_at, premium_cached FROM users WHERE id = ?1 AND app_id = ?2")
        .bind(&[user_id.into(), app_id.into()])
        .ok()?
        .first::<serde_json::Value>(None)
        .await
        .ok()??;
    let checked_at = row.get("premium_checked_at").and_then(|v| v.as_str());
    let cached = row.get("premium_cached").and_then(|v| v.as_i64()).map(|n| n != 0);
    crate::purchases::cached_premium(checked_at, cached, chrono::Utc::now())
}

/// The pack a card purchase sells: the app's own pack with this id, when its price equals the price of the Stripe
/// price object `get_stripe_price_id` charges for that id (the reference catalog in `get_credit_packs`), so the
/// card is charged exactly what the app's catalog lists and credited exactly what it promises.
fn card_pack(tenant_packs: &[crate::credits::CreditPack], pack_id: &str) -> Option<crate::credits::CreditPack> {
    let reference_price = get_credit_packs().into_iter().find(|p| p.id == pack_id)?.price_usd_cents;
    tenant_packs
        .iter()
        .find(|p| p.id == pack_id && p.price_usd_cents == reference_price)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(id: &str, credits: i32, price: i32) -> crate::credits::CreditPack {
        crate::credits::CreditPack {
            id: id.to_string(),
            name: id.to_string(),
            credits,
            price_usd_cents: price,
            bonus_credits: 0,
            description: String::new(),
        }
    }

    #[test]
    fn a_card_purchase_credits_the_apps_own_pack_at_the_stripe_price() {
        let pixie = vec![pack("starter", 210, 299), pack("basic", 840, 999)];
        assert_eq!(card_pack(&pixie, "starter").map(|p| p.credits), Some(210));
    }

    #[test]
    fn a_card_purchase_refuses_a_pack_whose_price_differs_from_the_stripe_price() {
        let tenant = vec![pack("starter", 30, 199)];
        assert!(card_pack(&tenant, "starter").is_none());
    }

    #[test]
    fn a_card_purchase_refuses_a_pack_the_app_does_not_sell_or_stripe_cannot_charge() {
        let dreameater = vec![pack("small", 5, 299)];
        assert!(card_pack(&dreameater, "starter").is_none());
        assert!(card_pack(&dreameater, "small").is_none());
    }

    #[test]
    fn a_premium_check_also_counts_purchases_tagged_with_the_mako_user_id() {
        let user = "6ED586B2-0000-4000-8000-000000000001";
        assert_eq!(premium_tokens(user, "4c49d445-0000-4000-8000-000000000002"), vec![user.to_lowercase()]);
        assert!(premium_tokens(user, &user.to_lowercase()).is_empty());
        assert!(premium_tokens("u-1", "4c49d445-0000-4000-8000-000000000002").is_empty());
    }

    #[test]
    fn a_balance_without_weekly_buckets_serializes_exactly_as_before() {
        let response = CreditBalanceResponse { balance: 5, currency: "credits".to_string(), weekly: None, edits: None };
        assert_eq!(serde_json::to_string(&response).unwrap(), r#"{"balance":5,"currency":"credits"}"#);
    }

    #[test]
    fn weekly_buckets_are_additive() {
        let mut weekly = std::collections::BTreeMap::new();
        weekly.insert(
            "reading.v2".to_string(),
            crate::handlers::chat_alias::WeeklyBalance { limit: 3, remaining: 2, resets_at: "2026-10-12T00:00:00Z".to_string() },
        );
        let response = CreditBalanceResponse { balance: 5, currency: "credits".to_string(), weekly: Some(weekly), edits: None };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["balance"], 5);
        assert_eq!(value["currency"], "credits");
        assert_eq!(value["weekly"]["reading.v2"]["limit"], 3);
        assert_eq!(value["weekly"]["reading.v2"]["remaining"], 2);
        assert_eq!(value["weekly"]["reading.v2"]["resets_at"], "2026-10-12T00:00:00Z");
    }
}
