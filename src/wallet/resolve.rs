use super::d1::D1Exec;
use super::link::account_of;
use super::links;
use super::target::{self, IdentityLookup};
use crate::error::AppError;
use crate::ledgers::Account;

/// True only for the exact value `on`; a missing variable, `off` or anything else leaves wallets disabled.
pub fn flag_is_on(value: Option<&str>) -> bool {
    value == Some("on")
}

/// The account a stored user has right now, without running the saga: the wallet when a link row exists, local
/// otherwise. With linking off the answer is local and the database is not read.
pub async fn account_for_user(linking: bool, db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<Account, AppError> {
    if !linking {
        return Ok(Account::local(app_id, user_id));
    }
    Ok(match links::read(db, app_id, user_id).await? {
        Some(row) => account_of(&row),
        None => Account::local(app_id, user_id),
    })
}

/// Where a grant addressed to `id` goes. With linking off every id is a local user and nothing is read or asked.
pub async fn credit_target(
    linking: bool,
    db: &impl D1Exec,
    identity: &impl IdentityLookup,
    app_id: &str,
    id: &str,
) -> Result<Account, AppError> {
    if !linking {
        return Ok(Account::local(app_id, id));
    }
    target::credit_target(db, identity, app_id, id).await
}
