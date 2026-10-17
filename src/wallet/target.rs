use chrono::Utc;
use serde_json::Value;
use worker::{Env, Fetcher, Headers, Method, RequestInit};

use super::d1::D1Exec;
use super::link::account_of;
use super::links;
use super::log;
use crate::error::AppError;
use crate::ledgers::Account;

/// The secret that authorises the identity lookup, sent in `X-Midgar-Internal`.
const SECRET: &str = "IDENTITY_LOOKUP_SECRET";

/// The header the shared secret travels in, as identity and the ledger use it.
const INTERNAL_HEADER: &str = "X-Midgar-Internal";

/// The internal identity route that answers whether a Midgar ID exists.
const EXISTS_URL: &str = "https://identity.internal/internal/exists";

/// Whether identity holds a Midgar ID. The answer is only exists or not.
#[allow(async_fn_in_trait)]
pub trait IdentityLookup {
    async fn exists(&self, midgar_id: &str) -> Result<bool, AppError>;
}

/// The identity service behind the `IDENTITY` binding.
pub struct IdentityService {
    fetcher: Fetcher,
    secret: String,
}

impl IdentityService {
    /// Opens the binding and the shared secret; either missing means the lookup cannot be made.
    pub fn open(env: &Env) -> Result<Self, AppError> {
        let secret = env
            .secret(SECRET)
            .map(|s| s.to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::InternalError(format!("{SECRET} is not configured")))?;
        let fetcher = env.service("IDENTITY").map_err(|_| AppError::InternalError("IDENTITY binding is missing".to_string()))?;
        Ok(Self { fetcher, secret })
    }
}

impl IdentityLookup for IdentityService {
    async fn exists(&self, midgar_id: &str) -> Result<bool, AppError> {
        let headers = Headers::new();
        headers.set(INTERNAL_HEADER, &self.secret)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Get).with_headers(headers);
        let mut response = self.fetcher.fetch(format!("{EXISTS_URL}/{midgar_id}"), Some(init)).await?;
        if response.status_code() != 200 {
            return Err(AppError::InternalError(format!("identity lookup answered {}", response.status_code())));
        }
        let body = response.json::<Value>().await?;
        body.get("exists").and_then(Value::as_bool).ok_or_else(|| AppError::InternalError("identity lookup answered no verdict".to_string()))
    }
}

/// A Midgar ID is a UUID in its hyphenated form.
pub fn is_midgar_id(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()
}

/// Where a grant addressed to `id` goes. The id is whatever the sender knows the buyer as: a RevenueCat
/// `app_user_id`.
///
/// 1. A user with a link, in any state: that Midgar ID's wallet.
/// 2. A user without one: local, the never-upgraded case.
/// 3. A Midgar ID some user is linked to: that wallet. One no user is linked to is looked up in identity, and only
///    an ID identity holds gets a `provider = 'midgar'` user and a `linked` row, so money is never granted into an
///    ID nobody owns.
/// 4. Anything else is an error and the sender retries.
pub async fn credit_target(db: &impl D1Exec, identity: &impl IdentityLookup, app_id: &str, id: &str) -> Result<Account, AppError> {
    if let Some(link) = links::read(db, app_id, id).await? {
        return Ok(account_of(&link));
    }
    if links::user_exists(db, app_id, id).await? {
        return Ok(Account::local(app_id, id));
    }
    if !is_midgar_id(id) {
        return Err(AppError::InternalError(format!("no account for purchase user {}", log::id_prefix(id))));
    }
    let midgar_id = id.to_lowercase();
    if let Some(link) = links::oldest_for_midgar(db, app_id, &midgar_id).await? {
        return Ok(account_of(&link));
    }
    if !identity.exists(&midgar_id).await? {
        log::error(&format!("credit_target: identity does not know midgar id {} app={app_id}", log::id_prefix(&midgar_id)));
        return Err(AppError::InternalError(format!("unknown midgar id {}", log::id_prefix(&midgar_id))));
    }
    let now = Utc::now().to_rfc3339();
    let user_id = links::midgar_user(db, app_id, &midgar_id, &now).await?;
    links::insert_linked(db, app_id, &user_id, &midgar_id, &now).await?;
    let link = links::oldest_for_midgar(db, app_id, &midgar_id).await?;
    link.map(|l| account_of(&l)).ok_or_else(|| AppError::InternalError("wallet link vanished".to_string()))
}
