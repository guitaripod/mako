use std::cell::RefCell;

use serde_json::Value;
use worker::{console_error, D1Database};
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

/// How long the app table is used before it is read again.
const ALIASES_TTL_MS: u64 = 5 * 60 * 1000;

/// How long after a failed read the next one waits.
const ALIASES_RETRY_MS: u64 = 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppRow {
    app_id: String,
    registry_slug: Option<String>,
}

/// The tenants' two spellings: mako's `apps.app_id` (`dreameater`) and the registry slug (`dream-eater`) that
/// MidgarKit 4 sends and the Midgar access token carries. Everything mako stores keeps the `app_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppAliases {
    rows: Vec<AppRow>,
}

impl AppAliases {
    /// Builds the table from `SELECT app_id, registry_slug FROM apps`; rows without an `app_id` are skipped.
    pub fn from_rows(rows: &[Value]) -> Self {
        let rows = rows
            .iter()
            .filter_map(|row| {
                let app_id = row.get("app_id")?.as_str()?.to_string();
                let registry_slug = row.get("registry_slug").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
                Some(AppRow { app_id, registry_slug })
            })
            .collect();
        Self { rows }
    }

    /// The `apps.app_id` a header value stands for: the value itself when it is an `app_id`, else the app whose
    /// registry slug it is, else the value unchanged (an unknown tenant behaves exactly as before).
    pub fn canonical(&self, raw: &str) -> String {
        if self.rows.iter().any(|row| row.app_id == raw) {
            return raw.to_string();
        }
        self.rows
            .iter()
            .find(|row| row.registry_slug.as_deref() == Some(raw))
            .map(|row| row.app_id.clone())
            .unwrap_or_else(|| raw.to_string())
    }

    /// The slug the Midgar access token carries for a canonical `app_id`; an app without one uses its `app_id`,
    /// which is what migration 030 sets for every tenant whose two names agree.
    pub fn registry_slug(&self, app_id: &str) -> String {
        self.rows
            .iter()
            .find(|row| row.app_id == app_id)
            .and_then(|row| row.registry_slug.clone())
            .unwrap_or_else(|| app_id.to_string())
    }
}

/// The alias table of one isolate and when it may be read again.
#[derive(Debug, Default)]
struct AliasCache {
    aliases: AppAliases,
    next_read_at: u64,
}

impl AliasCache {
    fn due(&self, now: u64) -> bool {
        now >= self.next_read_at
    }

    fn loaded(&mut self, aliases: AppAliases, now: u64) {
        self.aliases = aliases;
        self.next_read_at = now + ALIASES_TTL_MS;
    }

    fn failed(&mut self, now: u64) {
        self.next_read_at = now + ALIASES_RETRY_MS;
    }
}

thread_local! {
    static ALIASES: RefCell<AliasCache> = RefCell::new(AliasCache::default());
}

/// Reads the app table into the isolate's cache when it is older than five minutes. A failed read (for example a
/// database that predates migration 030) keeps the previous table, an empty one at first, which maps every header
/// value to itself, and is retried after a minute.
pub async fn warm_app_aliases(db: &D1Database) {
    let now = worker::Date::now().as_millis();
    if !ALIASES.with(|cache| cache.borrow().due(now)) {
        return;
    }
    let read = match db.prepare("SELECT app_id, registry_slug FROM apps ORDER BY app_id").all().await {
        Ok(result) => result.results::<Value>(),
        Err(e) => Err(e),
    };
    match read {
        Ok(rows) => ALIASES.with(|cache| cache.borrow_mut().loaded(AppAliases::from_rows(&rows), now)),
        Err(e) => {
            console_error!("app alias table could not be read: {:?}", e);
            ALIASES.with(|cache| cache.borrow_mut().failed(now));
        }
    }
}

/// The `apps.app_id` an `X-App-ID` value stands for, per the cached alias table.
pub fn canonical_app_id(raw: &str) -> String {
    ALIASES.with(|cache| cache.borrow().aliases.canonical(raw))
}

/// The registry slug of a canonical `apps.app_id`, per the cached alias table.
pub fn registry_slug(app_id: &str) -> String {
    ALIASES.with(|cache| cache.borrow().aliases.registry_slug(app_id))
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

    fn production_table() -> AppAliases {
        AppAliases::from_rows(&[
            json!({"app_id": "dreameater", "registry_slug": "dream-eater"}),
            json!({"app_id": "payday", "registry_slug": "pay-day"}),
            json!({"app_id": "pixie", "registry_slug": "pixiepocket"}),
            json!({"app_id": "helia", "registry_slug": "helia"}),
            json!({"app_id": "legacy", "registry_slug": null}),
        ])
    }

    #[test]
    fn both_spellings_resolve_to_the_stored_app_id() {
        let table = production_table();
        assert_eq!(table.canonical("dreameater"), "dreameater");
        assert_eq!(table.canonical("dream-eater"), "dreameater");
        assert_eq!(table.canonical("pay-day"), "payday");
        assert_eq!(table.canonical("pixiepocket"), "pixie");
        assert_eq!(table.canonical("pixie"), "pixie");
        assert_eq!(table.canonical("helia"), "helia");
    }

    #[test]
    fn an_unknown_value_is_left_alone() {
        let table = production_table();
        assert_eq!(table.canonical("scrubme"), "scrubme");
        assert_eq!(table.canonical(""), "");
        assert_eq!(table.canonical("Dream-Eater"), "Dream-Eater");
        assert_eq!(AppAliases::default().canonical("dream-eater"), "dream-eater");
    }

    #[test]
    fn an_app_id_wins_over_another_apps_slug() {
        let table = AppAliases::from_rows(&[
            json!({"app_id": "a", "registry_slug": "b"}),
            json!({"app_id": "b", "registry_slug": "c"}),
        ]);
        assert_eq!(table.canonical("b"), "b");
        assert_eq!(table.canonical("c"), "b");
    }

    #[test]
    fn the_token_slug_of_an_app_falls_back_to_its_app_id() {
        let table = production_table();
        assert_eq!(table.registry_slug("dreameater"), "dream-eater");
        assert_eq!(table.registry_slug("pixie"), "pixiepocket");
        assert_eq!(table.registry_slug("legacy"), "legacy");
        assert_eq!(table.registry_slug("unknown"), "unknown");
    }

    #[test]
    fn rows_without_an_app_id_and_empty_slugs_are_skipped() {
        let table = AppAliases::from_rows(&[json!({"registry_slug": "x"}), json!({"app_id": "y", "registry_slug": ""})]);
        assert_eq!(table.canonical("x"), "x");
        assert_eq!(table.registry_slug("y"), "y");
    }

    #[test]
    fn the_cache_is_read_when_empty_then_every_five_minutes_and_retried_after_a_minute() {
        let mut cache = AliasCache::default();
        assert!(cache.due(0));
        cache.loaded(production_table(), 1_000);
        assert!(!cache.due(1_000 + ALIASES_TTL_MS - 1));
        assert!(cache.due(1_000 + ALIASES_TTL_MS));
        cache.failed(10_000);
        assert!(!cache.due(10_000 + ALIASES_RETRY_MS - 1));
        assert!(cache.due(10_000 + ALIASES_RETRY_MS));
        assert_eq!(cache.aliases.canonical("pay-day"), "payday");
    }
}
