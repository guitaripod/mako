use chrono::{Duration, Utc};
use worker::{D1Database, Env};

use super::client::{DoWallets, WalletTransport, Wallets};
use super::d1::D1Exec;
use super::links::{self, LinkRow, LinkState};
use super::log;
use super::ops::{Grant, WalletOps};
use super::types::Kind;
use crate::error::AppError;
use crate::ledgers::Account;
use crate::midgar_token::{MidgarCaller, Tier};

/// The wrangler variable that switches linking on. Anything but `on` leaves every account local.
pub const FLAG: &str = "WALLET_LINKING";

/// A `moving` row older than this is finished by the cron, since its request is long gone.
const STALE_AFTER_MINUTES: i64 = 5;

/// Rows one cron run resumes, and linked users one run sweeps.
pub const CRON_LIMIT: i64 = 20;

/// How often the freeze is retried when the balance row changed under it.
const FREEZE_ATTEMPTS: usize = 3;

/// True when `WALLET_LINKING` is `on`.
pub fn enabled(env: &Env) -> bool {
    super::resolve::flag_is_on(env.var(FLAG).ok().map(|v| v.to_string()).as_deref())
}

/// The account a link row stands for.
pub fn account_of(link: &LinkRow) -> Account {
    Account::Wallet { midgar_id: link.midgar_id.clone(), app_id: link.app_id.clone(), user_id: link.user_id.clone() }
}

/// The saga that links users to Midgar ID wallets, over any D1 and any wallet transport.
pub struct Linker<'a, D: D1Exec, W: WalletTransport> {
    pub db: &'a D,
    pub wallets: &'a W,
}

impl<D: D1Exec, W: WalletTransport> Linker<'_, D, W> {
    fn ops(&self) -> WalletOps<'_, W, D> {
        WalletOps { wallets: self.wallets, db: self.db }
    }

    /// The link of a user, in any state.
    pub async fn lookup(&self, app_id: &str, user_id: &str) -> Result<Option<LinkRow>, AppError> {
        links::read(self.db, app_id, user_id).await
    }

    /// Links `user_id` to the caller's Midgar ID (saga step 1 to 4) and returns the wallet account. Idempotent: a
    /// linked user is returned at once, a `moving` one is driven to the end, and a user already linked to another
    /// Midgar ID stays on that link and is logged as a mismatch.
    pub async fn ensure_linked(&self, app_id: &str, user_id: &str, caller: &MidgarCaller) -> Result<Account, AppError> {
        let now = Utc::now().to_rfc3339();
        links::insert_moving(self.db, app_id, user_id, &caller.midgar_id, &now).await?;
        let link = self.lookup(app_id, user_id).await?.ok_or_else(|| AppError::InternalError("wallet link vanished".to_string()))?;
        self.settle(link, Some(caller)).await
    }

    /// The account of an authenticated user: the wallet when a link exists (finishing it first when it is still
    /// `moving`), a fresh link when the caller carries a verified Midgar ID, local otherwise.
    pub async fn account_for_request(&self, app_id: &str, user_id: &str, caller: Option<&MidgarCaller>) -> Result<Account, AppError> {
        match (self.lookup(app_id, user_id).await?, caller) {
            (Some(link), caller) => self.settle(link, caller).await,
            (None, Some(caller)) => self.ensure_linked(app_id, user_id, caller).await,
            (None, None) => Ok(Account::local(app_id, user_id)),
        }
    }

    /// The wallet account of a user that has a link, `None` for one that does not. A `moving` link is finished
    /// first, so a spend never reaches a wallet that does not hold the migrated balance yet.
    pub async fn relink(&self, app_id: &str, user_id: &str) -> Result<Option<Account>, AppError> {
        match self.lookup(app_id, user_id).await? {
            Some(link) => Ok(Some(self.settle(link, None).await?)),
            None => Ok(None),
        }
    }

    /// Logs a caller whose Midgar ID differs from the link's, then carries on with the link. The fix for a
    /// legitimately merged ID is the merge hook, not a second link.
    fn note_mismatch(link: &LinkRow, caller: Option<&MidgarCaller>) {
        if let Some(caller) = caller.filter(|c| c.midgar_id != link.midgar_id) {
            log::info(&format!(
                "wallet_link_mismatch app={} linked={} presented={}",
                link.app_id,
                log::id_prefix(&link.midgar_id),
                log::id_prefix(&caller.midgar_id)
            ));
        }
    }

    /// Drives a link to `linked` and returns its wallet account.
    async fn settle(&self, link: LinkRow, caller: Option<&MidgarCaller>) -> Result<Account, AppError> {
        Self::note_mismatch(&link, caller);
        Ok(account_of(&self.complete(link).await?))
    }

    /// Saga steps 2 to 4 for a row that exists. Safe to run again after a crash at any point.
    pub async fn complete(&self, link: LinkRow) -> Result<LinkRow, AppError> {
        if link.state == LinkState::Linked {
            return Ok(link);
        }
        let mut link = self.frozen(link).await?;
        let moved = link.moved_credits.unwrap_or(0);
        let welcome_part = link.welcome_part.unwrap_or(0);
        let key = links::migrate_key(&link.app_id, &link.user_id);
        let migration = Wallets { transport: self.wallets }.migrate(&link.midgar_id, &link.app_id, &key, moved, welcome_part).await?;
        self.ops().mirror(&link.midgar_id, &link.app_id, migration.moved.totals).await;
        links::mark_linked(self.db, &link.app_id, &link.user_id).await?;
        link.state = LinkState::Linked;
        Ok(link)
    }

    /// Saga step 2: the row with its balance recorded and zeroed in D1.
    async fn frozen(&self, mut link: LinkRow) -> Result<LinkRow, AppError> {
        for _ in 0..FREEZE_ATTEMPTS {
            if link.moved_credits.is_some() {
                return Ok(link);
            }
            links::freeze(self.db, &link.app_id, &link.user_id, &Utc::now().to_rfc3339()).await?;
            link = self.lookup(&link.app_id, &link.user_id).await?.ok_or_else(|| AppError::InternalError("wallet link vanished".to_string()))?;
        }
        if link.moved_credits.is_some() {
            return Ok(link);
        }
        log::error(&format!("wallet link freeze kept losing to writes app={} user={}", link.app_id, link.user_id));
        Err(AppError::WalletUnavailable)
    }

    /// Resolves a request that carries only a Midgar token. The user is the oldest one linked to that Midgar ID in
    /// the tenant; a Midgar ID with none gets a `provider = 'midgar'` user and a `linked` row. The welcome bonus is
    /// granted once per Midgar ID and tenant, and only to an attested device.
    pub async fn native_account(&self, app_id: &str, caller: &MidgarCaller) -> Result<Account, AppError> {
        if let Some(link) = links::oldest_for_midgar(self.db, app_id, &caller.midgar_id).await? {
            return self.settle(link, Some(caller)).await;
        }
        let now = Utc::now().to_rfc3339();
        let user_id = links::midgar_user(self.db, app_id, &caller.midgar_id, &now).await?;
        if caller.tier == Tier::AppAttest {
            self.welcome(app_id, &caller.midgar_id).await?;
        }
        links::insert_linked(self.db, app_id, &user_id, &caller.midgar_id, &now).await?;
        let link = links::oldest_for_midgar(self.db, app_id, &caller.midgar_id).await?;
        let link = link.ok_or_else(|| AppError::InternalError("wallet link vanished".to_string()))?;
        self.settle(link, Some(caller)).await
    }

    /// Grants the tenant's welcome credits under `welcome:<tenant>`. A key already claimed, by an earlier welcome or
    /// by a migration that carried one, is the bonus being given once already, not an error.
    async fn welcome(&self, app_id: &str, midgar_id: &str) -> Result<(), AppError> {
        let credits = links::welcome_credits(self.db, app_id).await?;
        if credits <= 0 {
            return Ok(());
        }
        let grant = Grant {
            key: super::ledger::welcome_key(app_id),
            kind: Kind::Bonus,
            source: "welcome",
            amount: credits as u32,
            description: "Welcome bonus",
            reference: None,
        };
        match self.ops().grant(midgar_id, app_id, &grant).await {
            Ok(_) | Err(AppError::Conflict(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Cron: finishes links that stayed `moving` for more than five minutes. Each row carries the Midgar ID the token
    /// proved, so no token is needed. Returns how many were completed; a row that fails is logged and left for the
    /// next run.
    pub async fn resume_stale(&self, limit: i64) -> Result<u32, AppError> {
        let cutoff = (Utc::now() - Duration::minutes(STALE_AFTER_MINUTES)).to_rfc3339();
        let mut finished = 0;
        for link in links::stale_moving(self.db, &cutoff, limit).await? {
            let (app_id, user_id) = (link.app_id.clone(), link.user_id.clone());
            match self.complete(link).await {
                Ok(_) => finished += 1,
                Err(e) => log::error(&format!("wallet link resume failed app={app_id} user={user_id}: {e:?}")),
            }
        }
        Ok(finished)
    }

    /// Cron: moves credits that landed in a linked user's frozen D1 row into the wallet, with the same
    /// freeze-then-migrate pattern, under `migrate:<tenant>:<user>:<n>`. A crash anywhere replays the same key with
    /// the amount held in `residue_pending`. Returns how many moves finished.
    pub async fn sweep_residue(&self, limit: i64) -> Result<u32, AppError> {
        let mut finished = 0;
        for residue in links::residue_candidates(self.db, limit).await? {
            let (app_id, user_id) = (residue.link.app_id.clone(), residue.link.user_id.clone());
            match self.move_residue(residue).await {
                Ok(true) => finished += 1,
                Ok(false) => {}
                Err(e) => log::error(&format!("wallet residue sweep failed app={app_id} user={user_id}: {e:?}")),
            }
        }
        Ok(finished)
    }

    /// One residue move. False when the balance changed under the freeze and the next run should look again.
    async fn move_residue(&self, residue: links::Residue) -> Result<bool, AppError> {
        let mut link = residue.link;
        if link.residue_pending.is_none() {
            links::freeze_residue(self.db, &link.app_id, &link.user_id, &residue.stamp, &Utc::now().to_rfc3339()).await?;
            link = self.lookup(&link.app_id, &link.user_id).await?.ok_or_else(|| AppError::InternalError("wallet link vanished".to_string()))?;
        }
        let Some(pending) = link.residue_pending else { return Ok(false) };
        let key = links::residue_key(&link.app_id, &link.user_id, link.residue_moves);
        let migration = Wallets { transport: self.wallets }.migrate(&link.midgar_id, &link.app_id, &key, pending, 0).await?;
        self.ops().mirror(&link.midgar_id, &link.app_id, migration.moved.totals).await;
        links::finish_residue(self.db, &link.app_id, &link.user_id, link.residue_moves).await?;
        log::info(&format!("wallet residue moved app={} user={} credits={}", link.app_id, link.user_id, pending));
        Ok(true)
    }
}

/// The production linker over D1 and the `WALLET` namespace.
pub struct Live<'a> {
    pub db: &'a D1Database,
    pub wallets: DoWallets,
}

impl<'a> Live<'a> {
    /// Opens the bindings the linker needs; a missing `WALLET` binding is a deployment fault, reported as the wallet
    /// being unavailable.
    pub fn open(env: &Env, db: &'a D1Database) -> Result<Self, AppError> {
        let namespace = env.durable_object("WALLET").map_err(|_| AppError::WalletUnavailable)?;
        Ok(Self { db, wallets: DoWallets(namespace) })
    }

    pub fn linker(&self) -> Linker<'_, D1Database, DoWallets> {
        Linker { db: self.db, wallets: &self.wallets }
    }
}

/// `ensure_linked` as the design names it: links a user to the caller's Midgar ID on the production bindings.
pub async fn ensure_linked(env: &Env, db: &D1Database, app_id: &str, user_id: &str, caller: &MidgarCaller) -> Result<Account, AppError> {
    Live::open(env, db)?.linker().ensure_linked(app_id, user_id, caller).await
}
