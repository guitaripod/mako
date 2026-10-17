use super::books::Books;
use super::types::{
    Balance, Entry, Kind, Migrated, Outcome, Reversed, StoredEntry, Verification, WalletError, MAX_AMOUNT,
    POOL_TENANT,
};

/// The key under which a tenant's welcome bonus is claimed, by a bonus grant or a marker row.
pub fn welcome_key(tenant: &str) -> String {
    format!("welcome:{tenant}")
}

/// Rejects a tenant the object must never hold: empty or the reserved shared pool.
pub fn validate_tenant(tenant: &str) -> Result<(), WalletError> {
    if tenant.is_empty() {
        return Err(WalletError::BadRequest("tenant is required".into()));
    }
    if tenant == POOL_TENANT {
        return Err(WalletError::BadRequest("tenant '*' is reserved".into()));
    }
    Ok(())
}

/// Checks the shape of an entry: tenant, key, magnitude, and the sign each kind allows.
fn validate(entry: &Entry) -> Result<(), WalletError> {
    validate_tenant(&entry.tenant)?;
    if entry.key.is_empty() {
        return Err(WalletError::BadRequest("key is required".into()));
    }
    if entry.amount.abs() > MAX_AMOUNT {
        return Err(WalletError::BadRequest("amount is out of range".into()));
    }
    let sign_ok = match entry.kind {
        Kind::Purchase | Kind::Bonus | Kind::Refund => entry.amount > 0,
        Kind::Migrate => entry.amount >= 0,
        Kind::Spend => entry.amount < 0,
        Kind::Reversal => entry.amount <= 0,
        Kind::Adjust => true,
        Kind::Marker => entry.amount == 0,
    };
    if !sign_ok {
        return Err(WalletError::BadRequest(format!(
            "amount {} is not valid for kind {}",
            entry.amount,
            entry.kind.as_str()
        )));
    }
    Ok(())
}

/// Answers a repeated key: the stored result when it is the same request, a conflict otherwise.
/// A `migrate` key replays whatever amount it was first stored with.
fn replay(stored: &StoredEntry, entry: &Entry) -> Result<Outcome, WalletError> {
    let both_migrate = stored.kind == Kind::Migrate && entry.kind == Kind::Migrate;
    let same = stored.tenant == entry.tenant && stored.kind == entry.kind && stored.amount == entry.amount;
    if (both_migrate && stored.tenant == entry.tenant) || same {
        return Ok(Outcome::Replayed { balance_after: stored.balance_after, seq: stored.seq });
    }
    Err(WalletError::Conflict(format!("key {} was used for a different entry", entry.key)))
}

/// The totals after adding a signed amount of the given kind.
fn next_totals(current: &Balance, entry: &Entry) -> Result<Balance, WalletError> {
    let overflow = || WalletError::BadRequest("balance overflow".into());
    let balance = current.balance.checked_add(entry.amount).ok_or_else(overflow)?;
    let purchased = if entry.kind == Kind::Purchase {
        current.purchased.checked_add(entry.amount).ok_or_else(overflow)?
    } else {
        current.purchased
    };
    let spent = if entry.amount < 0 {
        current.spent.checked_add(-entry.amount).ok_or_else(overflow)?
    } else {
        current.spent
    };
    Ok(Balance { balance, purchased, spent })
}

/// Applies an entry exactly once. The whole run is synchronous: it reads the key and the tenant's
/// balance, then writes both, so nothing can interleave. A repeated key replays the stored result
/// and writes nothing; an overdraw answers `Insufficient` and writes nothing.
pub fn apply(books: &mut impl Books, entry: &Entry, now: i64) -> Result<Outcome, WalletError> {
    validate(entry)?;
    if let Some(stored) = books.find(&entry.key)? {
        return replay(&stored, entry);
    }
    let current = books.balance(&entry.tenant)?;
    let next = next_totals(&current, entry)?;
    if next.balance < 0 {
        return Ok(Outcome::Insufficient { balance: current.balance });
    }
    let seq = books.append(entry, &next, now)?;
    Ok(Outcome::Applied { balance: next.balance, seq })
}

/// A debit that takes at most what the tenant holds. `entry.amount` is the requested debit
/// (negative); the stored amount is what was taken, so a replay reports the same `taken`.
pub fn reverse(books: &mut impl Books, entry: &Entry, now: i64) -> Result<Reversed, WalletError> {
    validate(entry)?;
    if !matches!(entry.kind, Kind::Reversal | Kind::Adjust) || entry.amount >= 0 {
        return Err(WalletError::BadRequest("reverse needs kind reversal or adjust and a negative amount".into()));
    }
    if let Some(stored) = books.find(&entry.key)? {
        if stored.tenant != entry.tenant || stored.kind != entry.kind {
            return Err(WalletError::Conflict(format!("key {} was used for a different entry", entry.key)));
        }
        return Ok(Reversed { taken: -stored.amount, balance: stored.balance_after, seq: stored.seq, replayed: true });
    }
    let held = books.balance(&entry.tenant)?.balance;
    let taken = (-entry.amount).min(held);
    let clamped = Entry { amount: -taken, ..entry.clone() };
    match apply(books, &clamped, now)? {
        Outcome::Applied { balance, seq } => Ok(Reversed { taken, balance, seq, replayed: false }),
        Outcome::Replayed { balance_after, seq } => Ok(Reversed { taken, balance: balance_after, seq, replayed: true }),
        Outcome::Insufficient { .. } => Err(WalletError::Storage("a clamped debit overdrew".into())),
    }
}

/// How much of a D1 balance enters the wallet. With no welcome claimed in the tenant yet the whole
/// balance moves; once one exists, the part of the balance that is the welcome bonus is dropped
/// (clamped at the balance) so a reinstall cannot earn the bonus twice.
pub fn migrate_amount(moved: i64, welcome_part: i64, welcome_marker_exists: bool) -> i64 {
    let moved = moved.max(0);
    if welcome_marker_exists {
        moved - moved.min(welcome_part.max(0))
    } else {
        moved
    }
}

/// Moves a frozen D1 balance into the wallet under one key. The first run claims the tenant's
/// welcome marker if absent and appends the migrate entry in the same synchronous run; a repeat
/// of the key returns the stored result whatever amounts it carries.
pub fn migrate(
    books: &mut impl Books,
    tenant: &str,
    key: &str,
    moved: i64,
    welcome_part: i64,
    now: i64,
) -> Result<Migrated, WalletError> {
    validate_tenant(tenant)?;
    if key.is_empty() {
        return Err(WalletError::BadRequest("key is required".into()));
    }
    if moved < 0 || welcome_part < 0 {
        return Err(WalletError::BadRequest("moved and welcome_part must not be negative".into()));
    }
    if let Some(stored) = books.find(key)? {
        if stored.kind != Kind::Migrate || stored.tenant != tenant {
            return Err(WalletError::Conflict(format!("key {key} was used for a different entry")));
        }
        return Ok(Migrated { amount: stored.amount, balance: stored.balance_after, seq: stored.seq, replayed: true });
    }
    let claimed = books.find(&welcome_key(tenant))?.is_some();
    let amount = migrate_amount(moved, welcome_part, claimed);
    if !claimed {
        let marker = Entry {
            key: welcome_key(tenant),
            tenant: tenant.to_string(),
            kind: Kind::Marker,
            amount: 0,
            source: "welcome".into(),
            reference: None,
            description: "Welcome claim".into(),
        };
        apply(books, &marker, now)?;
    }
    let entry = Entry {
        key: key.to_string(),
        tenant: tenant.to_string(),
        kind: Kind::Migrate,
        amount,
        source: "d1".into(),
        reference: None,
        description: "Migrated from the app balance".into(),
    };
    match apply(books, &entry, now)? {
        Outcome::Applied { balance, seq } => Ok(Migrated { amount, balance, seq, replayed: false }),
        Outcome::Replayed { balance_after, seq } => Ok(Migrated { amount, balance: balance_after, seq, replayed: true }),
        Outcome::Insufficient { .. } => Err(WalletError::Storage("a migration overdrew".into())),
    }
}

/// Recomputes every tenant from its entries and compares with the stored rows: each entry's
/// `balance_after`, the final balance, `purchased` and `spent`.
pub fn verify(books: &impl Books) -> Result<Verification, WalletError> {
    let mut problems = Vec::new();
    let tenants = books.tenants()?;
    let mut total = 0;
    for tenant in &tenants {
        let rows = books.chronological(tenant)?;
        total += rows.len();
        let mut expected = Balance::default();
        for row in &rows {
            expected.balance += row.amount;
            if row.kind == Kind::Purchase {
                expected.purchased += row.amount;
            }
            if row.amount < 0 {
                expected.spent += -row.amount;
            }
            if row.balance_after != expected.balance {
                problems.push(format!(
                    "{tenant}: entry {} has balance_after {} but the running sum is {}",
                    row.seq, row.balance_after, expected.balance
                ));
            }
        }
        let stored = books.balance(tenant)?;
        if stored != expected {
            problems.push(format!("{tenant}: stored {stored:?} but entries give {expected:?}"));
        }
    }
    Ok(Verification { ok: problems.is_empty(), tenants: tenants.len(), entries: total, problems })
}
