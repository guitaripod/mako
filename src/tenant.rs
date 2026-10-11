use serde_json::Value;
use worker::D1Database;
use crate::error::AppError;

/// Per-tenant data-handling and billing switches read from the `apps` row.
/// Every field defaults to today's behaviour, so a tenant that has not been
/// configured, or a database that predates the columns, behaves as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantPolicy {
    pub store_prompts: bool,
    pub store_images: bool,
    pub art_credits: Option<u32>,
    pub strict_purchases: bool,
}

impl Default for TenantPolicy {
    fn default() -> Self {
        Self {
            store_prompts: true,
            store_images: true,
            art_credits: None,
            strict_purchases: false,
        }
    }
}

impl TenantPolicy {
    pub fn from_row(row: &Value) -> Self {
        let defaults = Self::default();
        Self {
            store_prompts: flag(row, "store_prompts", defaults.store_prompts),
            store_images: flag(row, "store_images", defaults.store_images),
            art_credits: row
                .get("art_credits")
                .and_then(|v| v.as_i64())
                .map(|n| n.max(0) as u32),
            strict_purchases: flag(row, "strict_purchases", defaults.strict_purchases),
        }
    }

    pub async fn load(db: &D1Database, app_id: &str) -> Result<Self, AppError> {
        let row = db
            .prepare("SELECT * FROM apps WHERE app_id = ?1")
            .bind(&[app_id.into()])?
            .first::<Value>(None)
            .await?;
        Ok(row.map(|r| Self::from_row(&r)).unwrap_or_default())
    }

    /// The prompt text a table may keep for this tenant: the text itself, or
    /// an empty string when the tenant keeps no prompts.
    pub fn prompt_for_storage<'a>(&self, prompt: &'a str) -> &'a str {
        if self.store_prompts {
            prompt
        } else {
            ""
        }
    }
}

fn flag(row: &Value, key: &str, default: bool) -> bool {
    match row.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(v) => v.as_i64().map(|n| n != 0).unwrap_or(default),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_row_without_the_new_columns_keeps_todays_behaviour() {
        let legacy_row = json!({"app_id": "pixie", "name": "Pixie", "premium_entitlement": null});
        assert_eq!(TenantPolicy::from_row(&legacy_row), TenantPolicy::default());
        assert!(TenantPolicy::default().store_prompts);
        assert!(TenantPolicy::default().store_images);
        assert_eq!(TenantPolicy::default().art_credits, None);
        assert!(!TenantPolicy::default().strict_purchases);
    }

    #[test]
    fn the_migration_defaults_store_everything() {
        let row = json!({"store_prompts": 1, "store_images": 1, "art_credits": null, "strict_purchases": 0});
        assert_eq!(TenantPolicy::from_row(&row), TenantPolicy::default());
    }

    #[test]
    fn dream_eater_row_turns_storage_off_and_prices_art() {
        let row = json!({"store_prompts": 0, "store_images": 0, "art_credits": 1, "strict_purchases": 1});
        let policy = TenantPolicy::from_row(&row);
        assert!(!policy.store_prompts);
        assert!(!policy.store_images);
        assert_eq!(policy.art_credits, Some(1));
        assert!(policy.strict_purchases);
    }

    #[test]
    fn prompt_storage_follows_the_flag() {
        let keeper = TenantPolicy::default();
        assert_eq!(keeper.prompt_for_storage("a dream"), "a dream");
        let forgetter = TenantPolicy { store_prompts: false, ..TenantPolicy::default() };
        assert_eq!(forgetter.prompt_for_storage("a dream"), "");
    }

    #[test]
    fn boolean_columns_are_understood() {
        let row = json!({"store_prompts": false});
        assert!(!TenantPolicy::from_row(&row).store_prompts);
    }
}
