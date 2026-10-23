use super::client::{Absorb, Totals, WalletTransport, Wallets};
use super::d1::{int, D1Exec, Stmt};
use super::links::{self, LinkState};
use super::log;
use super::ops::WalletOps;
use super::types::{AbsorbItem, TenantTotals};
use crate::error::AppError;

/// The longest chain of merged-away wallets a merge follows. Identity merges an ID away at most once, so a chain is as
/// long as the merges that have been delivered out of order; past this something is wrong.
const MAX_HOPS: usize = 8;

/// The most wallets one erasure reaches: the ID's own and the merged-away ones it absorbed.
const MAX_TREE: usize = 64;

/// What erasing a Midgar ID came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Erasure {
    /// The wallet object was erased and its rows in D1 removed.
    Erased { entries: i64, links: i64 },
    /// Nothing in D1 points at a wallet of this ID, so no object was touched.
    NoWallet,
}

/// What merging one Midgar ID into another came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The credits of `from` are in the wallet of `target`, which is the survivor or whatever the survivor was merged
    /// into since, and the links of `from` name it now.
    Moved { target: String, tenants: usize, credits: i64 },
    /// Nothing in D1 points at a wallet of `from`: it never linked, or this merge already finished.
    NoWallet,
    /// The survivor was deleted, so the wallet of `from` was erased instead, as the ledger does with its claims.
    Discarded,
    /// `from` and `to` are the same ID.
    Same,
}

/// Whether D1 knows a wallet of this Midgar ID: a link to it or a mirror row. An ID that never linked, which is every
/// abandoned identity identity sweeps, has no object, and asking for one would create it.
pub async fn holds_wallet(db: &impl D1Exec, midgar_id: &str) -> Result<bool, AppError> {
    let rows = db
        .rows(
            "SELECT (SELECT COUNT(*) FROM wallet_links WHERE midgar_id = ?1) AS links,
                    (SELECT COUNT(*) FROM wallet_summary WHERE midgar_id = ?1) AS summaries",
            &[midgar_id.into()],
        )
        .await?;
    Ok(rows.first().is_some_and(|row| int(row, "links").unwrap_or(0) + int(row, "summaries").unwrap_or(0) > 0))
}

/// The wallets an erasure of `midgar_id` reaches, the ID's own first and the wallets it absorbed after it, however
/// deep. Cycles cannot make it loop: a wallet is visited once.
async fn tree<W: WalletTransport>(client: &Wallets<'_, W>, midgar_id: &str) -> Result<Vec<String>, AppError> {
    let mut order = vec![midgar_id.to_string()];
    let mut next = 0;
    while next < order.len() {
        for source in client.state(&order[next]).await?.sources {
            if !order.contains(&source) {
                order.push(source);
            }
        }
        if order.len() > MAX_TREE {
            return Err(AppError::InternalError("a wallet absorbed more Midgar IDs than any merge can have produced".to_string()));
        }
        next += 1;
    }
    Ok(order)
}

/// Erases everything mako keeps for a deleted Midgar ID: every tenant partition of its wallet and of the wallets merged
/// into it earlier, and its `wallet_links` and `wallet_summary` rows. Idempotent and safe to retry after a crash at any
/// point: the wallets are erased deepest first and the ID's own last, so a rerun still finds the record of what it
/// absorbed; the rows go at the end, in one transaction. An erased wallet refuses every later write.
pub async fn erase_all<D: D1Exec, W: WalletTransport>(db: &D, wallets: &W, midgar_id: &str) -> Result<Erasure, AppError> {
    if !holds_wallet(db, midgar_id).await? {
        return Ok(Erasure::NoWallet);
    }
    let client = Wallets { transport: wallets };
    let mut entries = 0;
    for id in tree(&client, midgar_id).await?.iter().rev() {
        entries += client.erase_all(id).await?.entries;
    }
    let links = db
        .rows("SELECT COUNT(*) AS n FROM wallet_links WHERE midgar_id = ?1", &[midgar_id.into()])
        .await?
        .first()
        .and_then(|row| int(row, "n"))
        .unwrap_or(0);
    db.batch(&[
        Stmt::new("DELETE FROM wallet_links WHERE midgar_id = ?1", vec![midgar_id.into()]),
        Stmt::new("DELETE FROM wallet_summary WHERE midgar_id = ?1", vec![midgar_id.into()]),
    ])
    .await?;
    log::info(&format!("wallet erased id={} entries={} links={}", log::id_prefix(midgar_id), entries, links));
    Ok(Erasure::Erased { entries, links })
}

/// The wallet that finally holds a Midgar ID's credits: the ID itself, or the one it was merged into, and so on. `None`
/// when that wallet was erased. `from` is the ID being merged away; reaching it again is a cycle.
async fn survivor_of<W: WalletTransport>(client: &Wallets<'_, W>, start: &str, from: &str) -> Result<Option<String>, AppError> {
    let mut at = start.to_string();
    for _ in 0..MAX_HOPS {
        let state = client.state(&at).await?;
        if state.erased {
            return Ok(None);
        }
        match state.sealed_into {
            None => return Ok(Some(at)),
            Some(next) if next == from => return Err(AppError::InternalError("a merge chain leads back to the ID being merged".to_string())),
            Some(next) => at = next,
        }
    }
    Err(AppError::InternalError("a merge chain is longer than any run of merges can be".to_string()))
}

/// Moves the credits of a merged-away wallet to the first wallet along the chain that still takes them. `None` when
/// the chain ends in an erased wallet.
async fn absorb_along<W: WalletTransport>(
    client: &Wallets<'_, W>,
    first: &str,
    from: &str,
    items: &[AbsorbItem],
) -> Result<Option<(String, Vec<TenantTotals>)>, AppError> {
    let mut target = first.to_string();
    for _ in 0..MAX_HOPS {
        match client.absorb(&target, from, items).await? {
            Absorb::Done { totals, .. } => return Ok(Some((target, totals))),
            Absorb::Sealed(into) if into == from => {
                return Err(AppError::InternalError("a merge chain leads back to the ID being merged".to_string()));
            }
            Absorb::Sealed(into) => target = into,
            Absorb::Erased => return Ok(None),
        }
    }
    Err(AppError::InternalError("a merge chain is longer than any run of merges can be".to_string()))
}

/// Settles the links of the merged-away ID whose saga had already moved credits into its sealed wallet. The wallet is
/// sealed, so nothing can have changed since: a `moving` link whose `migrate` entry is in it is `linked` (its credits
/// are among the ones just absorbed), and a residue move whose entry is in it is finished. Without this the saga would
/// run again against the survivor, find no such entry there, and move the same credits a second time.
async fn settle_moved<D: D1Exec, W: WalletTransport>(db: &D, client: &Wallets<'_, W>, from: &str) -> Result<(), AppError> {
    for link in links::of_midgar(db, from).await? {
        if link.state == LinkState::Moving && link.moved_credits.is_some() {
            let key = links::migrate_key(&link.app_id, &link.user_id);
            if client.entry(from, &link.app_id, &key).await?.is_some() {
                links::mark_linked(db, &link.app_id, &link.user_id).await?;
            }
        }
        if link.residue_pending.is_some() {
            let key = links::residue_key(&link.app_id, &link.user_id, link.residue_moves);
            if client.entry(from, &link.app_id, &key).await?.is_some() {
                links::finish_residue(db, &link.app_id, &link.user_id, link.residue_moves).await?;
            }
        }
    }
    Ok(())
}

/// Folds the wallet of Midgar ID `from` into the wallet of `to`, when identity merged `from` away.
///
/// 1. Find the wallet that holds `to`'s credits now (`to` may have been merged away itself since). An erased one means
///    the survivor was deleted: `from` is erased instead.
/// 2. Seal `from`. A sealed wallet refuses every write, so its totals are fixed from here and no spend can race the
///    move; a repeat keeps the target of the first seal.
/// 3. Settle the sagas that had already moved credits into `from`, then absorb its tenants into the survivor in one
///    step, one `merge:<from>:<tenant>` entry each and a marker, all or nothing. A survivor that was merged away in
///    the meantime but took the credits before still answers as done, one that did not points further along the chain.
/// 4. In one D1 transaction, point the links of `from` at the wallet that holds the credits and drop its mirror rows;
///    then refresh the survivor's mirror.
///
/// Every step is idempotent and the order puts the D1 change last, so a crash anywhere is finished by running the
/// merge again; the gate (`from` still has a link or a mirror row) turns a finished merge into a no-op.
pub async fn merge<D: D1Exec, W: WalletTransport>(db: &D, wallets: &W, from: &str, to: &str) -> Result<Merged, AppError> {
    if from == to {
        return Ok(Merged::Same);
    }
    if !holds_wallet(db, from).await? {
        return Ok(Merged::NoWallet);
    }
    let client = Wallets { transport: wallets };
    let Some(survivor) = survivor_of(&client, to, from).await? else { return discard(db, wallets, from).await };
    let Some(target) = client.seal(from, &survivor).await?.sealed_into else { return discard(db, wallets, from).await };

    let items: Vec<AbsorbItem> = client
        .state(from)
        .await?
        .tenants
        .into_iter()
        .filter(|t| t.balance > 0)
        .map(|t| AbsorbItem { tenant: t.tenant, amount: t.balance })
        .collect();
    settle_moved(db, &client, from).await?;
    let Some((took, totals)) = absorb_along(&client, &target, from, &items).await? else { return discard(db, wallets, from).await };
    let Some(holder) = survivor_of(&client, &took, from).await? else { return discard(db, wallets, from).await };

    db.batch(&[
        Stmt::new("UPDATE wallet_links SET midgar_id = ?1 WHERE midgar_id = ?2", vec![holder.as_str().into(), from.into()]),
        Stmt::new("DELETE FROM wallet_summary WHERE midgar_id = ?1", vec![from.into()]),
    ])
    .await?;
    if holder == took {
        let ops = WalletOps { wallets, db };
        for t in &totals {
            ops.mirror(&holder, &t.tenant, Totals { balance: t.balance, purchased: t.purchased, spent: t.spent }).await;
        }
    }
    let credits = items.iter().map(|item| item.amount).sum();
    log::info(&format!("wallet merged from={} into={} tenants={} credits={}", log::id_prefix(from), log::id_prefix(&holder), items.len(), credits));
    Ok(Merged::Moved { target: holder, tenants: items.len(), credits })
}

/// The survivor of a merge is gone, so its credits are too: erase the merged-away wallet instead of moving them.
async fn discard<D: D1Exec, W: WalletTransport>(db: &D, wallets: &W, from: &str) -> Result<Merged, AppError> {
    erase_all(db, wallets, from).await?;
    Ok(Merged::Discarded)
}
