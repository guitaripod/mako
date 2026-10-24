use worker::{Request, Response, RouteContext, Result, Env, Fetch, Method, Headers};
use crate::error::AppError;
use crate::auth;
use crate::wallet::pack_credit::{credit_pack, refund_pack, Credit};
use crate::wallet::pack_refund::{self, Settlement};
use crate::credits::{
    get_user_transactions, get_credit_packs, get_credit_packs_for_app,
    record_purchase, record_and_complete_purchase, estimate_image_cost
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditBalanceResponse {
    pub balance: i32,
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<std::collections::BTreeMap<String, crate::handlers::chat_alias::WeeklyBalance>>,
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

    Response::from_json(&CreditBalanceResponse {
        balance,
        currency: "credits".to_string(),
        weekly,
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
    
    // Find the pack
    let packs = get_credit_packs();
    let pack = packs
        .iter()
        .find(|p| p.id == purchase_req.pack_id)
        .ok_or_else(|| AppError::BadRequest("Invalid pack_id".to_string()))?;
    
    let total_credits = pack.credits + pack.bonus_credits;
    
    let db = env.d1("DB")?;
    
    match purchase_req.payment_provider.as_str() {
        "stripe" => {
            let purchase_id = record_purchase(
                &app_id,
                &user_id,
                &purchase_req.pack_id,
                total_credits as u32,
                pack.price_usd_cents as u32,
                &purchase_req.payment_provider,
                &purchase_req.payment_id,
                &db,
            ).await?;
            
            Response::from_json(&json!({
                "purchase_id": purchase_id,
                "status": "pending",
                "credits": total_credits,
                "amount_usd": format!("${:.2}", pack.price_usd_cents as f64 / 100.0),
            }))
        },
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

pub async fn get_purchase_status(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let purchase_id = ctx.param("purchase_id")
        .ok_or_else(|| AppError::BadRequest("Missing purchase_id".to_string()))?
        .to_string();
    let env = ctx.env;
    
    let db = env.d1("DB")?;
    
    // Get purchase details
    let purchase = db
        .prepare("SELECT status, payment_provider, payment_id FROM credit_purchases WHERE id = ?")
        .bind(&[purchase_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .ok_or_else(|| AppError::NotFound("Purchase not found".to_string()))?;
    
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
                    // If payment is completed, complete the purchase
                    if session.payment_status == "paid" {
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
    
    // Find the pack
    let packs = get_credit_packs();
    let pack = packs
        .iter()
        .find(|p| p.id == checkout_req.pack_id)
        .ok_or_else(|| AppError::BadRequest("Invalid pack_id".to_string()))?;
    
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
            
            // Only process if payment is successful
            if session.payment_status == "paid" {
                // Extract purchase_id from metadata
                if let Some(purchase_id) = session.metadata.get("purchase_id") {
                    let db = env.d1("DB")?;
                    crate::ledgers::Ledgers::new(&env, &db).complete_purchase(purchase_id).await?;
                    worker::console_log!("Completed purchase: {}", purchase_id);
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
pub struct ValidateRevenueCatPurchaseRequest {
    pub pack_id: String,
    pub purchase_token: String,
    pub product_id: String,
    pub platform: String, // "ios" or "android"
    /// The StoreKit 2 signed transaction (JWS) of the purchase. When present, mako verifies it through the ledger
    /// instead of asking RevenueCat.
    #[serde(default)]
    pub signed_transaction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateRevenueCatPurchaseResponse {
    pub success: bool,
    pub purchase_id: String,
    pub credits_added: u32,
    pub new_balance: i32,
}

pub async fn validate_revenuecat_purchase(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
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

    let validate_req: ValidateRevenueCatPurchaseRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };
    
    // Validate platform
    if validate_req.platform != "ios" && validate_req.platform != "android" {
        return AppError::BadRequest("Invalid platform. Must be 'ios' or 'android'".to_string()).to_response();
    }

    // An empty token or product id would make the substring verification below
    // (body.contains) trivially true. Reject before any RevenueCat call.
    if validate_req.purchase_token.trim().is_empty() || validate_req.product_id.trim().is_empty() {
        return AppError::BadRequest("Missing purchase_token or product_id".to_string()).to_response();
    }

    let db = env.d1("DB")?;

    // The product id must be this app's pack-product prefix + the claimed pack id.
    // This binds the price the user paid to the credits they receive, so a real
    // cheap-pack receipt cannot be replayed to claim an expensive pack.
    let product_prefix = db
        .prepare("SELECT rc_product_prefix FROM apps WHERE app_id = ?1")
        .bind(&[app_id.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("rc_product_prefix").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let expected_product = format!("{}{}", product_prefix, validate_req.pack_id);
    if !product_prefix.is_empty() && validate_req.product_id != expected_product {
        return AppError::BadRequest("product_id does not match pack_id".to_string()).to_response();
    }

    // Find the pack (per-app catalog)
    let packs = get_credit_packs_for_app(&app_id, &db).await;
    let pack = packs
        .iter()
        .find(|p| p.id == validate_req.pack_id)
        .ok_or_else(|| AppError::BadRequest("Invalid pack_id".to_string()))?;

    let total_credits = pack.credits + pack.bonus_credits;

    if validate_req.signed_transaction.as_deref().is_some_and(|signed| !signed.trim().is_empty()) {
        return crate::handlers::storekit::validate_signed(&req, &env, &db, &auth, &validate_req, pack).await;
    }

    // Check if this purchase token has already been used
    let existing_purchase = db.prepare(
        "SELECT id, status FROM credit_purchases WHERE app_id = ? AND payment_id = ? AND payment_provider = 'revenuecat'"
    )
    .bind(&[app_id.clone().into(), validate_req.purchase_token.clone().into()])?
    .first::<serde_json::Value>(None)
    .await?;
    
    if let Some(existing) = existing_purchase {
        let status = existing.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "completed" {
            return AppError::AlreadyProcessed("Purchase has already been processed".to_string()).to_response();
        }
    }
    
    if let Some(refunded) = pack_refund::refunded_error(&db, &app_id, &validate_req.purchase_token).await? {
        return refunded.to_response();
    }

    // Verify with RevenueCat BEFORE granting anything. The authenticated webhook
    // (revenuecat_webhook) is the authoritative, idempotent source of truth; this
    // client path is a fast-track that must never grant on an unverified or failed check.
    let revenuecat_customer = auth.account.midgar_id().unwrap_or(&user_id).to_string();
    let validation_result = validate_with_revenuecat(
        &env,
        &db,
        &app_id,
        &revenuecat_customer,
        &validate_req.purchase_token,
        &validate_req.product_id,
        &validate_req.platform,
    ).await;

    match validation_result {
        Ok(PurchaseVerification::Verified) => {}
        Ok(PurchaseVerification::Rejected) => {
            return AppError::UnverifiablePurchase("Purchase could not be verified with RevenueCat".to_string()).to_response();
        }
        Ok(PurchaseVerification::Defer) => {
            // Not yet visible in RevenueCat. Do NOT grant here; the authenticated
            // webhook is the authoritative, idempotent fallback.
            worker::console_log!("RevenueCat purchase not yet visible, deferring to webhook");
            return AppError::CreditPending(crate::storekit::PENDING_MESSAGE.to_string()).to_response();
        }
        Err(e) => {
            // RevenueCat unreachable: do NOT grant. The webhook will reconcile this purchase.
            worker::console_log!("RevenueCat validation unavailable, deferring to webhook: {}", e);
            return AppError::CreditPending(crate::storekit::PENDING_MESSAGE.to_string()).to_response();
        }
    }

    let ledgers = crate::ledgers::Ledgers::new(&env, &db);
    let purchase_id = if let crate::ledgers::Account::Wallet { .. } = &auth.account {
        let credited = credit_pack(
            &db,
            &ledgers,
            &app_id,
            &user_id,
            &validate_req.pack_id,
            total_credits as u32,
            pack.price_usd_cents as u32,
            &validate_req.purchase_token,
        )
        .await;
        match credited {
            Ok(Credit::Credited { purchase_id }) => purchase_id,
            Ok(Credit::Duplicate) => {
                return AppError::AlreadyProcessed("Purchase has already been processed".to_string()).to_response();
            }
            Ok(Credit::Refunded) | Err(AppError::Refunded(_)) => {
                return AppError::Refunded(crate::storekit::REFUNDED_MESSAGE.to_string()).to_response();
            }
            Err(e) => {
                worker::console_log!("RevenueCat fast-track could not credit the wallet, deferring to webhook: {:?}", e);
                return AppError::CreditPending(crate::storekit::PENDING_MESSAGE.to_string()).to_response();
            }
        }
    } else {
        record_and_complete_purchase(
            &app_id,
            &user_id,
            &validate_req.pack_id,
            total_credits as u32,
            pack.price_usd_cents as u32,
            &validate_req.purchase_token,
            &db,
        ).await?
    };

    let new_balance = match ledgers.get_user_balance(&auth.account).await {
        Ok(balance) => balance,
        Err(e) => return crate::error::wallet_outage_response(e),
    };

    Response::from_json(&ValidateRevenueCatPurchaseResponse {
        success: true,
        purchase_id,
        credits_added: total_credits as u32,
        new_balance,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct RevenueCatSubscriber {
    entitlements: RevenueCatEntitlements,
    subscriber: RevenueCatSubscriberInfo,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevenueCatEntitlements {
    #[serde(flatten)]
    all: std::collections::HashMap<String, RevenueCatEntitlement>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevenueCatEntitlement {
    expires_date: Option<String>,
    purchase_date: String,
    product_identifier: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevenueCatSubscriberInfo {
    original_app_user_id: String,
    #[serde(default)]
    non_subscriptions: std::collections::HashMap<String, Vec<RevenueCatNonSubscription>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevenueCatNonSubscription {
    id: String,
    is_sandbox: bool,
    purchase_date: String,
    store: String,
}

/// True if `user_id` holds the app's configured premium entitlement, verified
/// against the RevenueCat REST API (non-spoofable). Only apps with a non-null
/// `premium_entitlement` incur the RC call; everyone else returns false with a
/// single indexed D1 read.
///
/// A tenant that is not on strict purchases keeps the original rule: any active
/// entitlement counts, and any error/404 => false (charge normally), so a
/// transient RC issue never silently hands out free unlimited usage. A strict
/// tenant must hold its own configured entitlement, and when RevenueCat cannot be
/// reached the last answer from the previous 24 hours is used instead of treating
/// a paying user as free.
pub async fn is_premium_user(
    env: &Env,
    db: &worker::D1Database,
    app_id: &str,
    user_id: &str,
) -> bool {
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
    let project_id = row.get("rc_project_id").and_then(|v| v.as_str()).unwrap_or("");
    let required = match required {
        Some(r) if !project_id.is_empty() => r,
        _ => return false,
    };

    let entitlements = fetch_active_entitlements(env, app_id, project_id, user_id).await;
    if !crate::tenant::TenantPolicy::from_row(&row).strict_purchases {
        return entitlements.map(|items| !items.is_empty()).unwrap_or(false);
    }

    match entitlements {
        Some(items) => {
            let premium = holds_required_entitlement(env, app_id, project_id, &items, &required).await;
            store_premium_cache(db, app_id, user_id, premium).await;
            premium
        }
        None => read_premium_cache(db, app_id, user_id).await.unwrap_or(false),
    }
}

fn revenuecat_api_key(env: &Env, app_id: &str) -> Option<String> {
    let key_name = format!("REVENUECAT_IOS_API_KEY_{}", app_id.to_uppercase());
    env.secret(&key_name)
        .or_else(|_| env.secret("REVENUECAT_IOS_API_KEY"))
        .ok()
        .map(|k| k.to_string())
}

/// GETs a RevenueCat v2 resource; None on any transport error, non-200 or bad JSON.
async fn revenuecat_get(api_key: &str, url: &str) -> Option<serde_json::Value> {
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {}", api_key)).ok()?;
    let mut init = worker::RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init(url, &init).ok()?;
    let mut resp = Fetch::Request(request).send().await.ok()?;
    if resp.status_code() != 200 {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// The customer's active entitlements, or None when they could not be fetched.
async fn fetch_active_entitlements(
    env: &Env,
    app_id: &str,
    project_id: &str,
    user_id: &str,
) -> Option<Vec<serde_json::Value>> {
    let api_key = revenuecat_api_key(env, app_id)?;
    let url = format!(
        "https://api.revenuecat.com/v2/projects/{}/customers/{}/active_entitlements",
        project_id, user_id
    );
    revenuecat_get(&api_key, &url)
        .await?
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
}

/// True when one of the active entitlements is `required`. RevenueCat's v2 list
/// gives internal entitlement ids, so an item that does not name the entitlement
/// is looked up once to learn its lookup key.
async fn holds_required_entitlement(
    env: &Env,
    app_id: &str,
    project_id: &str,
    items: &[serde_json::Value],
    required: &str,
) -> bool {
    use crate::purchases::{entitlement_id_to_resolve, item_names_entitlement, resolved_entitlement_matches};
    if items.iter().any(|item| item_names_entitlement(item, required)) {
        return true;
    }
    let Some(api_key) = revenuecat_api_key(env, app_id) else { return false };
    for item in items {
        let Some(entitlement_id) = entitlement_id_to_resolve(item, required) else { continue };
        let url = format!(
            "https://api.revenuecat.com/v2/projects/{}/entitlements/{}",
            project_id, entitlement_id
        );
        if let Some(entitlement) = revenuecat_get(&api_key, &url).await {
            if resolved_entitlement_matches(&entitlement, required) {
                return true;
            }
        }
    }
    false
}

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

/// Outcome of verifying a client-reported purchase against RevenueCat.
enum PurchaseVerification {
    /// The transaction and product were both found in the customer's RevenueCat
    /// purchases — safe to grant on the fast-track client path.
    Verified,
    /// RevenueCat answered, but this transaction/product is not present in the
    /// customer's record. Never grant: the token is fake, replayed, or claims a
    /// different product than was actually bought.
    Rejected,
    /// RevenueCat could not confirm right now (customer/purchase not yet
    /// propagated, or the API was unreachable). Do not grant on this path; the
    /// authenticated webhook is the authoritative, idempotent fallback.
    Defer,
}

/// Verifies that `purchase_token` and `product_id` both belong to a real purchase
/// on this RevenueCat customer. The store transaction id is high-entropy, so its
/// literal presence anywhere in the customer's purchases payload is proof the
/// purchase is real and belongs to them; requiring `product_id` to also appear
/// blocks pack-substitution (paying for a cheap pack, claiming an expensive one).
async fn validate_with_revenuecat(
    env: &Env,
    db: &worker::D1Database,
    app_id: &str,
    user_id: &str,
    purchase_token: &str,
    product_id: &str,
    platform: &str,
) -> Result<PurchaseVerification> {
    if crate::mock::enabled(env) {
        return Ok(PurchaseVerification::Verified);
    }
    let key_name = format!("REVENUECAT_IOS_API_KEY_{}", app_id.to_uppercase());
    let api_key = env
        .secret(&key_name)
        .or_else(|_| env.secret("REVENUECAT_IOS_API_KEY"))
        .map_err(|_| format!("RevenueCat API key not configured for {}", app_id))?
        .to_string();

    let project_id = db
        .prepare("SELECT rc_project_id FROM apps WHERE app_id = ?1")
        .bind(&[app_id.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("rc_project_id").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .ok_or_else(|| format!("rc_project_id not configured for {}", app_id))?;

    let url = format!(
        "https://api.revenuecat.com/v2/projects/{}/customers/{}/purchases",
        project_id, user_id
    );

    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {}", api_key))?;
    headers.set("Content-Type", "application/json")?;
    headers.set("X-Platform", platform)?;

    let mut init = worker::RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);

    let request = Request::new_with_init(&url, &init)?;
    let mut response = Fetch::Request(request).send().await?;

    let status_code = response.status_code();
    if status_code == 404 {
        worker::console_log!("RevenueCat customer/purchases not found; deferring to webhook");
        return Ok(PurchaseVerification::Defer);
    }
    if status_code < 200 || status_code >= 300 {
        let error_text = response.text().await.unwrap_or_default();
        worker::console_log!("RevenueCat API error: {}", crate::redact::provider_error_summary(status_code, &error_text));
        return Err(format!("RevenueCat API error: {}", status_code).into());
    }

    let body = response.text().await?;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(items) = parsed.get("items").and_then(|v| v.as_array()) else {
        worker::console_log!("RevenueCat purchases response had no items; deferring to webhook");
        return Ok(PurchaseVerification::Defer);
    };

    let matched = items.iter().find(|item| {
        item.get("store_purchase_identifier").and_then(|v| v.as_str()) == Some(purchase_token)
    });
    let Some(purchase) = matched else {
        worker::console_log!("RevenueCat purchase token not found for customer {}; rejecting fast-track", user_id);
        return Ok(PurchaseVerification::Rejected);
    };

    let rc_product_id = purchase.get("product_id").and_then(|v| v.as_str()).unwrap_or_default();
    match rc_store_identifier(&api_key, &project_id, rc_product_id).await {
        Ok(Some(store_identifier)) if store_identifier == product_id => Ok(PurchaseVerification::Verified),
        Ok(Some(store_identifier)) => {
            worker::console_log!(
                "RevenueCat product mismatch for customer {}: transaction is {} but {} was claimed; rejecting",
                user_id, store_identifier, product_id
            );
            Ok(PurchaseVerification::Rejected)
        }
        _ => {
            worker::console_log!("RevenueCat product {} could not be resolved; deferring to webhook", rc_product_id);
            Ok(PurchaseVerification::Defer)
        }
    }
}

/// Resolves a RevenueCat v2 internal product id (e.g. `proda9db…`) to its App
/// Store SKU. The v2 `/customers/{id}/purchases` payload carries only the
/// internal id, never the store SKU — that lives solely on the `/products/{id}`
/// resource as `store_identifier` — so the purchased product can only be bound
/// to the pack the client paid for by this second lookup. Returns `None` on any
/// non-2xx so the caller defers to the webhook rather than reject a real buyer.
async fn rc_store_identifier(
    api_key: &str,
    project_id: &str,
    rc_product_id: &str,
) -> Result<Option<String>> {
    if rc_product_id.is_empty() {
        return Ok(None);
    }
    let url = format!(
        "https://api.revenuecat.com/v2/projects/{}/products/{}",
        project_id, rc_product_id
    );
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {}", api_key))?;
    let mut init = worker::RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init(&url, &init)?;
    let mut response = Fetch::Request(request).send().await?;
    if response.status_code() < 200 || response.status_code() >= 300 {
        return Ok(None);
    }
    let body = response.text().await?;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    Ok(parsed
        .get("store_identifier")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueCatWebhookEvent {
    pub api_version: String,
    pub event: RevenueCatEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueCatEvent {
    pub app_user_id: String,
    pub currency: Option<String>,
    pub entitlement_id: Option<String>,
    pub environment: String,
    pub event_timestamp_ms: i64,
    pub id: String,
    pub is_family_share: bool,
    pub offer_code: Option<String>,
    pub price: Option<f64>,
    pub price_in_purchased_currency: Option<f64>,
    pub product_id: String,
    pub purchased_at_ms: i64,
    pub store: String,
    pub takehome_percentage: Option<f64>,
    pub transaction_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
}

/// Resolve which tenant a RevenueCat product belongs to by matching the product
/// id against each app's `rc_product_prefix`. Longest matching prefix wins (most
/// specific tenant). Returns (app_id, matched_prefix).
async fn resolve_tenant_by_product(db: &worker::D1Database, product_id: &str) -> Option<(String, String)> {
    crate::storekit::tenant_of_product(db, product_id).await.ok().flatten()
}

/// Tags a paid RevenueCat event that the credits ledger does not itself record —
/// a subscription, or a product that resolves to no credit pack — with the
/// buyer's Apple Ads campaign and keyword.
///
/// Keyed on the store transaction id, and skipped entirely when a
/// `credit_purchases` row already exists for that transaction, so a keyword's
/// revenue is counted exactly once no matter which path credited the purchase.
/// Best-effort: a webhook must still return 200 to RevenueCat.
async fn tag_revenuecat_revenue(db: &worker::D1Database, event: &RevenueCatEvent) {
    let app_id = match db
        .prepare("SELECT app_id FROM users WHERE id = ?1")
        .bind(&[event.app_user_id.clone().into()])
    {
        Ok(stmt) => match stmt.first::<serde_json::Value>(None).await {
            Ok(Some(row)) => row.get("app_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            _ => String::new(),
        },
        Err(_) => String::new(),
    };
    if app_id.is_empty() {
        worker::console_log!("No mako identity for RevenueCat user {}; not tagging", event.app_user_id);
        return;
    }

    let already_in_ledger = match db
        .prepare("SELECT id FROM credit_purchases WHERE app_id = ?1 AND payment_id = ?2 AND payment_provider = 'revenuecat'")
        .bind(&[app_id.clone().into(), event.transaction_id.clone().into()])
    {
        Ok(stmt) => matches!(stmt.first::<serde_json::Value>(None).await, Ok(Some(_))),
        Err(_) => false,
    };
    if already_in_ledger {
        return;
    }

    let kind = if event.entitlement_id.is_some() {
        crate::attribution::PurchaseKind::Subscription
    } else {
        crate::attribution::PurchaseKind::Unknown
    };
    let gross = crate::attribution::usd_to_cents(event.price);
    let net = crate::attribution::net_cents(gross, event.takehome_percentage);
    let purchase_id = format!("rc:{}", event.transaction_id);

    match crate::attribution::tag_purchase(
        db,
        &app_id,
        &event.app_user_id,
        &purchase_id,
        kind,
        &event.product_id,
        gross,
        net,
    )
    .await
    {
        Ok(true) => worker::console_log!("tagged {} {} with ad attribution", kind.as_str(), purchase_id),
        Ok(false) => {}
        Err(e) => worker::console_log!("could not tag {} with ad attribution: {:?}", purchase_id, e),
    }
}

pub async fn revenuecat_webhook(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    
    let auth_header = req.headers().get("Authorization")?;
    
    let expected_auth = env.secret("REVENUECAT_WEBHOOK_AUTH")
        .map_err(|_| AppError::InternalError("REVENUECAT_WEBHOOK_AUTH not configured".to_string()))?
        .to_string();
    
    if !crate::purchases::webhook_authorized(auth_header.as_deref(), &expected_auth) {
        return AppError::Unauthorized("Invalid webhook authorization".to_string()).to_response();
    }
    
    let webhook_event: RevenueCatWebhookEvent = match req.json().await {
        Ok(event) => event,
        Err(e) => return AppError::BadRequest(format!("Invalid webhook body: {}", e)).to_response(),
    };
    
    let event = &webhook_event.event;
    worker::console_log!("RevenueCat webhook: {} for user {}", event.event_type, event.app_user_id);

    let db = env.d1("DB")?;
    let tenant = resolve_tenant_by_product(&db, &event.product_id).await;
    let strict = match &tenant {
        Some((app_id, _)) => crate::tenant::TenantPolicy::load(&db, app_id)
            .await
            .map(|p| p.strict_purchases)
            .unwrap_or(false),
        None => false,
    };
    let production = crate::purchases::is_production(env.var("ENVIRONMENT").ok().map(|v| v.to_string()).as_deref());

    use crate::purchases::{classify_event, should_ignore_event, EventAction};
    match classify_event(&event.event_type, strict) {
        EventAction::CreditPack => {
            let Some((app_id, prefix)) = tenant else {
                worker::console_log!("No tenant matched product {}", event.product_id);
                tag_revenuecat_revenue(&db, event).await;
                return Response::ok("OK");
            };
            if should_ignore_event(strict, production, &event.environment) {
                worker::console_log!("Ignoring sandbox event {} in production", event.id);
                return Response::ok("OK");
            }
            credit_pack_from_event(&env, &db, event, &app_id, &prefix).await?;
        },
        EventAction::TagRevenueOnly => {
            tag_revenuecat_revenue(&db, event).await;
        },
        EventAction::ReversePackRefund => {
            if let Some((app_id, prefix)) = tenant {
                if should_ignore_event(strict, production, &event.environment) {
                    worker::console_log!("Ignoring sandbox event {} in production", event.id);
                    return Response::ok("OK");
                }
                reverse_pack_refund(&env, &db, event, &app_id, &prefix).await?;
            }
        },
        EventAction::LogOnly => {
            if event.event_type == "BILLING_ISSUE" {
                worker::console_log!("Billing issue for user {}", event.app_user_id);
            } else {
                worker::console_log!("Subscription event {} for user {}", event.event_type, event.app_user_id);
            }
        },
        EventAction::Ignore => {
            worker::console_log!("Unhandled RevenueCat event type: {}", event.event_type);
        }
    }
    
    Response::ok("OK")
}

/// Credits the credit pack a purchase event names, once per store transaction:
/// the client validate call records the same `(app, "revenuecat", transaction id)`
/// row, so whichever path arrives first credits and the other finds it.
async fn credit_pack_from_event(
    env: &Env,
    db: &worker::D1Database,
    event: &RevenueCatEvent,
    app_id: &str,
    prefix: &str,
) -> Result<()> {
    let pack_id = event
        .product_id
        .strip_prefix(prefix)
        .unwrap_or(&event.product_id)
        .to_string();

    let packs = get_credit_packs_for_app(app_id, db).await;
    let pack = match packs.iter().find(|p| p.id == pack_id) {
        Some(p) => p,
        None => {
            worker::console_log!("Unknown pack {} for app {} (product {})", pack_id, app_id, event.product_id);
            tag_revenuecat_revenue(db, event).await;
            return Ok(());
        }
    };

    if pack_refund::is_claimed(db, app_id, &event.transaction_id).await? {
        worker::console_log!("Transaction {} was refunded; not credited", event.transaction_id);
        return Ok(());
    }

    let total_credits = pack.credits + pack.bonus_credits;

    let ledgers = crate::ledgers::Ledgers::new(env, db);
    if ledgers.linking() {
        let target = ledgers.credit_target(app_id, &event.app_user_id).await?;
        if let crate::ledgers::Account::Wallet { user_id, .. } = &target {
            return credit_pack_to_wallet(&ledgers, db, event, app_id, user_id, pack, &pack_id).await;
        }
    }

    let existing = db.prepare(
        "SELECT id FROM credit_purchases WHERE app_id = ? AND payment_id = ? AND payment_provider = 'revenuecat'"
    )
    .bind(&[app_id.into(), event.transaction_id.clone().into()])?
    .first::<serde_json::Value>(None)
    .await?;

    if existing.is_some() {
        worker::console_log!("Transaction {} already processed", event.transaction_id);
        return Ok(());
    }

    let purchase_id = match record_and_complete_purchase(
        app_id,
        &event.app_user_id,
        &pack_id,
        total_credits as u32,
        pack.price_usd_cents as u32,
        &event.transaction_id,
        db,
    )
    .await
    {
        Ok(id) => id,
        Err(e) if crate::error::is_refunded(&e) => {
            worker::console_log!("Transaction {} was refunded; not credited", event.transaction_id);
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    worker::console_log!("Processed RevenueCat purchase {} for app {} user {}", purchase_id, app_id, event.app_user_id);
    Ok(())
}

/// Credits a pack to a wallet account. A purchase already settled, whatever its final status, is not credited again;
/// one left `pending` by a crash is finished here, and the wallet grant is idempotent by the store transaction id. A
/// transaction whose refund was claimed, before the row or while it was being completed, is not credited.
async fn credit_pack_to_wallet(
    ledgers: &crate::ledgers::Ledgers<'_>,
    db: &worker::D1Database,
    event: &RevenueCatEvent,
    app_id: &str,
    user_id: &str,
    pack: &crate::credits::CreditPack,
    pack_id: &str,
) -> Result<()> {
    let credits = (pack.credits + pack.bonus_credits) as u32;
    match credit_pack(db, ledgers, app_id, user_id, pack_id, credits, pack.price_usd_cents as u32, &event.transaction_id).await? {
        Credit::Credited { purchase_id } => {
            worker::console_log!("Processed RevenueCat purchase {} for app {} wallet user {}", purchase_id, app_id, user_id);
        }
        Credit::Duplicate => worker::console_log!("Transaction {} already processed", event.transaction_id),
        Credit::Refunded => worker::console_log!("Transaction {} was refunded; not credited", event.transaction_id),
    }
    Ok(())
}

/// Takes back the credits of a refunded credit-pack purchase, down to a zero
/// balance, once per transaction. A product that is not a credit pack is left
/// alone, so subscriptions are never touched. The claim row in
/// `purchase_refunds` is written first and the purchase read after
/// (`pack_refund::claim_and_settle`), so no credit can slip in between, and
/// it is released again if the reversal fails so RevenueCat's retry can finish
/// the job. The claim, the reversal key and the purchase lookup are shared with
/// the ledger's refund route (`wallet::ledger_refund`), so a refund that reaches
/// both is reversed once. A refund for a transaction no purchase was recorded
/// for stays claimed, so the credit paths refuse that transaction when it
/// arrives later; a purchase still `pending` is finished first, then reversed.
async fn reverse_pack_refund(
    env: &Env,
    db: &worker::D1Database,
    event: &RevenueCatEvent,
    app_id: &str,
    prefix: &str,
) -> Result<()> {
    let pack_id = event.product_id.strip_prefix(prefix).unwrap_or(&event.product_id);
    let packs = get_credit_packs_for_app(app_id, db).await;
    if !packs.iter().any(|p| p.id == pack_id) {
        worker::console_log!("Refund for {} is not a credit pack; ignored", event.product_id);
        return Ok(());
    }

    let ledgers = crate::ledgers::Ledgers::new(env, db);
    match refund_pack(db, &ledgers, app_id, pack_id, &event.transaction_id).await? {
        Settlement::Tombstone => {
            worker::console_log!("Refund for transaction {} has no credited purchase; recorded so it is never credited", event.transaction_id);
        }
        Settlement::Settled(_) => worker::console_log!("Refund for transaction {} already reversed", event.transaction_id),
        Settlement::Reversed { purchase, taken } => {
            worker::console_log!("Reversed {} of {} credits for refunded transaction {}", taken, purchase.credits, event.transaction_id);
        }
        Settlement::Declined(_) => worker::console_log!("Refund for transaction {} is in no account this path reverses", event.transaction_id),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_balance_without_weekly_buckets_serializes_exactly_as_before() {
        let response = CreditBalanceResponse { balance: 5, currency: "credits".to_string(), weekly: None };
        assert_eq!(serde_json::to_string(&response).unwrap(), r#"{"balance":5,"currency":"credits"}"#);
    }

    #[test]
    fn weekly_buckets_are_additive() {
        let mut weekly = std::collections::BTreeMap::new();
        weekly.insert(
            "reading.v2".to_string(),
            crate::handlers::chat_alias::WeeklyBalance { limit: 3, remaining: 2, resets_at: "2026-10-12T00:00:00Z".to_string() },
        );
        let response = CreditBalanceResponse { balance: 5, currency: "credits".to_string(), weekly: Some(weekly) };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["balance"], 5);
        assert_eq!(value["currency"], "credits");
        assert_eq!(value["weekly"]["reading.v2"]["limit"], 3);
        assert_eq!(value["weekly"]["reading.v2"]["remaining"], 2);
        assert_eq!(value["weekly"]["reading.v2"]["resets_at"], "2026-10-12T00:00:00Z");
    }
}
