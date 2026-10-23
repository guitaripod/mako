use super::books::Books;
use super::ledger::{apply, validate_tenant};
use super::types::{AbsorbItem, Absorbed, Entry, Kind, Sealed, State, TenantTotals, WalletError, MAX_AMOUNT};

/// The `meta` key that marks a wallet whose Midgar ID was deleted.
pub const ERASED: &str = "erased";

/// The `meta` key that holds the Midgar ID a merged-away wallet was merged into.
pub const SEALED_INTO: &str = "sealed_into";

/// The prefix of the `meta` keys that record which Midgar IDs this wallet absorbed.
pub const ABSORBED: &str = "absorbed:";

/// The idempotency key of the credits one tenant of a merged-away wallet brought along.
pub fn merge_key(from: &str, tenant: &str) -> String {
    format!("merge:{from}:{tenant}")
}

/// The marker that says a merged-away wallet was absorbed here in full, in one step.
fn marker(from: &str) -> String {
    format!("{ABSORBED}{from}")
}

/// Why this wallet takes no new money, if it does not: erased, or sealed into another Midgar ID.
pub fn closed(books: &impl Books) -> Result<Option<WalletError>, WalletError> {
    if books.meta(ERASED)?.is_some() {
        return Ok(Some(WalletError::Erased));
    }
    Ok(books.meta(SEALED_INTO)?.map(WalletError::Sealed))
}

/// The totals of every tenant that has a balance row.
fn totals(books: &impl Books) -> Result<Vec<TenantTotals>, WalletError> {
    books
        .tenants()?
        .into_iter()
        .map(|tenant| {
            let b = books.balance(&tenant)?;
            Ok(TenantTotals { tenant, balance: b.balance, purchased: b.purchased, spent: b.spent })
        })
        .collect()
}

/// `GET /state`.
pub fn state(books: &impl Books) -> Result<State, WalletError> {
    let sources = books.meta_keys(ABSORBED)?.into_iter().filter_map(|key| key.strip_prefix(ABSORBED).map(str::to_string)).collect();
    Ok(State { sealed_into: books.meta(SEALED_INTO)?, erased: books.meta(ERASED)?.is_some(), sources, tenants: totals(books)? })
}

/// Seals the wallet into `into`: from now on it refuses every write, so its totals can no longer change and a merge
/// can move them exactly. Safe to repeat; a wallet already sealed keeps its first target and says so, an erased wallet
/// has nothing to seal.
pub fn seal(books: &mut impl Books, into: &str) -> Result<Sealed, WalletError> {
    if into.is_empty() {
        return Err(WalletError::BadRequest("into is required".into()));
    }
    if books.meta(ERASED)?.is_some() {
        return Ok(Sealed { erased: true, sealed_into: None, replayed: false });
    }
    if let Some(stored) = books.meta(SEALED_INTO)? {
        return Ok(Sealed { erased: false, sealed_into: Some(stored), replayed: true });
    }
    books.set_meta(SEALED_INTO, into)?;
    Ok(Sealed { erased: false, sealed_into: Some(into.to_string()), replayed: false })
}

/// Takes the credits of a merged-away wallet in one synchronous step: one `merge:<from>:<tenant>` entry per tenant and a
/// marker, all or nothing (everything that could fail is checked before the first write), so a repeat finds the marker and changes nothing. A wallet that is erased or sealed refuses
/// (after the repeat check, because a survivor that was itself merged away later still answers for what it took
/// before).
pub fn absorb(books: &mut impl Books, from: &str, items: &[AbsorbItem], now: i64) -> Result<Absorbed, WalletError> {
    if from.is_empty() {
        return Err(WalletError::BadRequest("from is required".into()));
    }
    let marker = marker(from);
    if books.meta(&marker)?.is_some() {
        return Ok(Absorbed { replayed: true, totals: touched(books, items)? });
    }
    if let Some(refusal) = closed(books)? {
        return Err(refusal);
    }
    for (n, item) in items.iter().enumerate() {
        validate_tenant(&item.tenant)?;
        if item.amount <= 0 || item.amount > MAX_AMOUNT {
            return Err(WalletError::BadRequest("an absorbed amount must be positive".into()));
        }
        if items[..n].iter().any(|earlier| earlier.tenant == item.tenant) {
            return Err(WalletError::BadRequest("a tenant may appear once in an absorb".into()));
        }
        if books.balance(&item.tenant)?.balance.checked_add(item.amount).is_none() {
            return Err(WalletError::BadRequest("balance overflow".into()));
        }
    }
    for item in items {
        let entry = Entry {
            key: merge_key(from, &item.tenant),
            tenant: item.tenant.clone(),
            kind: Kind::Adjust,
            amount: item.amount,
            source: "merge".into(),
            reference: Some(from.to_string()),
            description: "Merged from another Midgar ID".into(),
        };
        apply(books, &entry, now)?;
    }
    books.set_meta(&marker, &now.to_string())?;
    Ok(Absorbed { replayed: false, totals: touched(books, items)? })
}

/// The current totals of the tenants an absorb names.
fn touched(books: &impl Books, items: &[AbsorbItem]) -> Result<Vec<TenantTotals>, WalletError> {
    items
        .iter()
        .map(|item| {
            let b = books.balance(&item.tenant)?;
            Ok(TenantTotals { tenant: item.tenant.clone(), balance: b.balance, purchased: b.purchased, spent: b.spent })
        })
        .collect()
}

/// Erases every tenant and marks the wallet erased for good. The seal and the record of absorbed Midgar IDs go too.
pub fn erase_all(books: &mut impl Books, now: i64) -> Result<(usize, usize), WalletError> {
    let tenants = books.tenants()?;
    let mut entries = 0;
    for tenant in &tenants {
        entries += books.erase(tenant)?;
    }
    for key in books.meta_keys(ABSORBED)? {
        books.delete_meta(&key)?;
    }
    books.delete_meta(SEALED_INTO)?;
    books.set_meta(ERASED, &now.to_string())?;
    Ok((entries, tenants.len()))
}
