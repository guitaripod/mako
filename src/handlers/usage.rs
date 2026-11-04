use worker::{D1Database, Env, Request, Response, RouteContext, Result};
use crate::auth::{self, AuthedUser};
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Serialize, Deserialize)]
pub struct UserUsage {
    pub user_id: String,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub total_images: i64,
    pub period_start: String,
    pub period_end: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DetailedUsage {
    pub date: String,
    pub requests: i64,
    pub tokens: i64,
    pub images: i64,
}

pub async fn get_user_usage(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let user_id = ctx.param("user_id")
        .ok_or_else(|| AppError::BadRequest("Missing user_id parameter".to_string()))?
        .to_string();

    let env = ctx.env;
    let db = env.d1("DB")?;
    let app_id = match authorize_usage_reader(&req, &env, &db, &user_id).await {
        Ok(reader) => reader.app_id,
        Err(e) => return e.to_response(),
    };
    let url = req.url()?;
    let query_params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .into_owned()
        .collect();
    
    let period_start = query_params
        .get("start")
        .cloned()
        .unwrap_or_else(|| {
            let thirty_days_ago = chrono::Utc::now() - chrono::Duration::days(30);
            thirty_days_ago.to_rfc3339()
        });
    
    let period_end = query_params
        .get("end")
        .cloned()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    
    let stmt = db.prepare(
        "SELECT
            COUNT(*) as total_requests,
            COALESCE(SUM(total_tokens), 0) as total_tokens,
            COALESCE(SUM(image_count), 0) as total_images
         FROM usage_records
         WHERE app_id = ?
           AND user_id = ?
           AND created_at >= ?
           AND created_at <= ?
           AND error IS NULL"
    );

    let result = stmt
        .bind(&[app_id.clone().into(), user_id.clone().into(), period_start.clone().into(), period_end.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?;
    
    let usage = match result {
        Some(value) => UserUsage {
            user_id,
            total_requests: value.get("total_requests").and_then(|v| v.as_i64()).unwrap_or(0),
            total_tokens: value.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            total_images: value.get("total_images").and_then(|v| v.as_i64()).unwrap_or(0),
            period_start,
            period_end,
        },
        None => UserUsage {
            user_id,
            total_requests: 0,
            total_tokens: 0,
            total_images: 0,
            period_start,
            period_end,
        },
    };
    
    Response::from_json(&usage)
}

pub async fn get_user_usage_details(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let user_id = ctx.param("user_id")
        .ok_or_else(|| AppError::BadRequest("Missing user_id parameter".to_string()))?
        .to_string();

    let env = ctx.env;
    let db = env.d1("DB")?;
    let app_id = match authorize_usage_reader(&req, &env, &db, &user_id).await {
        Ok(reader) => reader.app_id,
        Err(e) => return e.to_response(),
    };
    let url = req.url()?;
    let query_params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .into_owned()
        .collect();
    
    let period_start = query_params
        .get("start")
        .cloned()
        .unwrap_or_else(|| {
            let thirty_days_ago = chrono::Utc::now() - chrono::Duration::days(30);
            thirty_days_ago.to_rfc3339()
        });
    
    let period_end = query_params
        .get("end")
        .cloned()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    
    let stmt = db.prepare(
        "SELECT
            DATE(created_at) as date,
            COUNT(*) as requests,
            COALESCE(SUM(total_tokens), 0) as tokens,
            COALESCE(SUM(image_count), 0) as images
         FROM usage_records
         WHERE app_id = ?
           AND user_id = ?
           AND created_at >= ?
           AND created_at <= ?
           AND error IS NULL
         GROUP BY DATE(created_at)
         ORDER BY date DESC"
    );

    let results = stmt
        .bind(&[app_id.clone().into(), user_id.clone().into(), period_start.clone().into(), period_end.clone().into()])?
        .all()
        .await?;
    
    let mut daily_usage = Vec::new();
    if let Ok(rows) = results.results() {
        for row in rows {
            if let Ok(value) = serde_json::from_value::<serde_json::Value>(row) {
                daily_usage.push(DetailedUsage {
                    date: value.get("date").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    requests: value.get("requests").and_then(|v| v.as_i64()).unwrap_or(0),
                    tokens: value.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    images: value.get("images").and_then(|v| v.as_i64()).unwrap_or(0),
                });
            }
        }
    }
    
    Response::from_json(&json!({
        "user_id": user_id,
        "period_start": period_start,
        "period_end": period_end,
        "daily_usage": daily_usage
    }))
}

pub async fn get_system_usage(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    let db = env.d1("DB")?;
    let app_id = match authorize_system_usage_reader(&req, &env, &db).await {
        Ok(admin) => admin.app_id,
        Err(e) => return e.to_response(),
    };
    let url = req.url()?;
    let query_params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .into_owned()
        .collect();
    
    let period_start = query_params
        .get("start")
        .cloned()
        .unwrap_or_else(|| {
            let thirty_days_ago = chrono::Utc::now() - chrono::Duration::days(30);
            thirty_days_ago.to_rfc3339()
        });
    
    let period_end = query_params
        .get("end")
        .cloned()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    
    let overall_stmt = db.prepare(
        "SELECT 
            COUNT(DISTINCT user_id) as total_users,
            COUNT(*) as total_requests,
            COALESCE(SUM(total_tokens), 0) as total_tokens,
            COALESCE(SUM(image_count), 0) as total_images,
            COALESCE(AVG(response_time_ms), 0) as avg_response_time
         FROM usage_records 
         WHERE app_id = ?
           AND created_at >= ? 
           AND created_at <= ?
           AND error IS NULL"
    );
    
    let overall_result = overall_stmt
        .bind(&[app_id.clone().into(), period_start.clone().into(), period_end.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?;
    
    let top_users_stmt = db.prepare(
        "SELECT 
            user_id,
            COUNT(*) as requests,
            COALESCE(SUM(total_tokens), 0) as tokens
         FROM usage_records 
         WHERE app_id = ?
           AND created_at >= ? 
           AND created_at <= ?
           AND error IS NULL
         GROUP BY user_id
         ORDER BY tokens DESC
         LIMIT 10"
    );
    
    let top_users_results = top_users_stmt
        .bind(&[app_id.clone().into(), period_start.clone().into(), period_end.clone().into()])?
        .all()
        .await?;
    
    let mut top_users = Vec::new();
    if let Ok(rows) = top_users_results.results() {
        for row in rows {
            if let Ok(value) = serde_json::from_value::<serde_json::Value>(row) {
                top_users.push(json!({
                    "user_id": value.get("user_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "requests": value.get("requests").and_then(|v| v.as_i64()).unwrap_or(0),
                    "tokens": value.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                }));
            }
        }
    }
    
    let error_stmt = db.prepare(
        "SELECT 
            COUNT(*) as error_count
         FROM usage_records 
         WHERE app_id = ?
           AND created_at >= ? 
           AND created_at <= ?
           AND error IS NOT NULL"
    );
    
    let error_result = error_stmt
        .bind(&[app_id.clone().into(), period_start.clone().into(), period_end.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?;
    
    let error_count = error_result
        .and_then(|v| v.get("error_count").and_then(|count| count.as_i64()))
        .unwrap_or(0);
    
    let overall_stats = overall_result.unwrap_or_else(|| json!({}));
    let total_requests = overall_stats.get("total_requests").and_then(|v| v.as_i64()).unwrap_or(0);
    let error_rate = if total_requests > 0 {
        (error_count as f64 / (total_requests + error_count) as f64) * 100.0
    } else {
        0.0
    };
    
    Response::from_json(&json!({
        "period_start": period_start,
        "period_end": period_end,
        "overall_stats": {
            "total_users": overall_stats.get("total_users").and_then(|v| v.as_i64()).unwrap_or(0),
            "total_requests": total_requests,
            "total_tokens": overall_stats.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "total_images": overall_stats.get("total_images").and_then(|v| v.as_i64()).unwrap_or(0),
            "avg_response_time": overall_stats.get("avg_response_time").and_then(|v| v.as_f64()).unwrap_or(0.0),
            "error_rate": error_rate,
        },
        "top_users": top_users
    }))
}

/// The caller, when it may read `user_id`'s usage: that user itself, or an admin of the same app.
async fn authorize_usage_reader(req: &Request, env: &Env, db: &D1Database, user_id: &str) -> std::result::Result<AuthedUser, AppError> {
    let caller = auth::authenticate_account(req, env, db).await?;
    if may_read_user_usage(&caller.user_id, caller.is_admin, user_id) {
        Ok(caller)
    } else {
        Err(AppError::Forbidden("You can only read your own usage".to_string()))
    }
}

/// The caller, when it may read the app-wide usage summary: an admin of its app. The summary is scoped to that app.
async fn authorize_system_usage_reader(req: &Request, env: &Env, db: &D1Database) -> std::result::Result<AuthedUser, AppError> {
    let caller = auth::authenticate_account(req, env, db).await?;
    if caller.is_admin {
        Ok(caller)
    } else {
        Err(AppError::Forbidden("Admin access required".to_string()))
    }
}

/// True when a caller with `caller_id` and `is_admin` may read the usage of `target_id`.
fn may_read_user_usage(caller_id: &str, is_admin: bool, target_id: &str) -> bool {
    is_admin || caller_id == target_id
}

#[cfg(test)]
mod tests {
    use super::may_read_user_usage;

    #[test]
    fn a_user_reads_only_its_own_usage() {
        assert!(may_read_user_usage("user-a", false, "user-a"));
        assert!(!may_read_user_usage("user-a", false, "user-b"));
    }

    #[test]
    fn an_admin_reads_any_user_of_its_app() {
        assert!(may_read_user_usage("admin", true, "user-b"));
    }
}
