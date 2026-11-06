use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use super::{encode, google_reason, payment_id_of, unavailable, Call, PlayApi, PlayTenant, Signer, Transport, APPLICATIONS_URL};
use crate::error::AppError;
use crate::storekit::VerifyFailure;
use crate::wallet::client::WalletTransport;
use crate::wallet::d1::{int, text, D1Exec};
use crate::wallet::ledger_refund::{self, Outcome};
use crate::wallet::log;

/// How far back Google lists voided purchases: 30 days, less a margin so a request is never refused as too old.
const LOOKBACK_MS: i64 = 30 * 24 * 3_600_000 - 10 * 60_000;

/// How far each run reaches back before the end of the last finished one. Google records a void a while after it
/// happens, and a run can fail or be skipped; the claim in `purchase_refunds` makes seeing an order twice harmless.
const OVERLAP_MS: i64 = 2 * 3_600_000;

/// How far behind now a window ends, so Google never reads `endTime` as a time in the future.
const END_LAG_MS: i64 = 60_000;

/// Results per page, Google's maximum.
const PAGE_SIZE: u32 = 1000;

/// Pages one run reads per package. The cron runs every 30 minutes, so this stays far inside the 6000 queries a day
/// and 30 queries per 30 seconds Google allows each package. A window with more pages goes on in the next run.
const PAGES_PER_RUN: u32 = 10;

/// One voided one-time purchase as `purchases.voidedpurchases.list` answers it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct VoidedPurchase {
    pub order_id: Option<String>,
    pub purchase_token: Option<String>,
    pub voided_time_millis: Option<String>,
}

impl VoidedPurchase {
    /// The payment id the voided order was credited under: `gp:<orderId>`, or the token's hash for an order without an
    /// id (a redeemed promo code). `None` when Google named neither.
    pub fn payment_id(&self) -> Option<String> {
        let order = self.order_id.as_deref().filter(|order| !order.trim().is_empty());
        let token = self.purchase_token.as_deref().filter(|token| !token.is_empty());
        match (order, token) {
            (None, None) => None,
            (order, token) => Some(payment_id_of(order, token.unwrap_or_default())),
        }
    }
}

/// One page of voided purchases and the token of the next one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoidedPage {
    pub purchases: Vec<VoidedPurchase>,
    pub next_page_token: Option<String>,
}

/// The span of times Google recorded voids in that one run asks about, in milliseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoidedWindow {
    pub start_ms: i64,
    pub end_ms: i64,
}

/// The window of a run at `now_ms`: from `OVERLAP_MS` before the end of the last finished run (the whole 30 days Google
/// keeps on the first run), never older than Google lists, up to a minute ago.
pub fn voided_window(swept_until_ms: Option<i64>, now_ms: i64) -> VoidedWindow {
    let oldest = now_ms - LOOKBACK_MS;
    let end_ms = now_ms - END_LAG_MS;
    let start_ms = swept_until_ms.map_or(oldest, |until| (until - OVERLAP_MS).max(oldest)).min(end_ms);
    VoidedWindow { start_ms, end_ms }
}

/// A window read across runs: the page to go on from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub window: VoidedWindow,
    pub page_token: String,
}

/// Where the sweep of one package stands, as `play_voided_sweeps` keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepState {
    /// The end of the last window read completely, `None` before the first.
    pub swept_until_ms: Option<i64>,
    /// A window whose later pages the next run reads.
    pub resume: Option<Resume>,
}

impl<T: Transport, S: Signer> PlayApi<'_, T, S> {
    /// One page of `purchases.voidedpurchases.list` for one-time products (`type=0`) voided in `window`, or the page
    /// `page_token` names. A page token Google refuses (400, an expired token) is `Rejected`; anything else but a
    /// readable 200 is `Unavailable`.
    pub async fn voided_page(&self, package: &str, window: &VoidedWindow, page_token: Option<&str>) -> Result<VoidedPage, VerifyFailure> {
        let mut url = format!(
            "{APPLICATIONS_URL}/{}/purchases/voidedpurchases?type=0&maxResults={PAGE_SIZE}&startTime={}&endTime={}",
            encode(package),
            window.start_ms,
            window.end_ms
        );
        if let Some(token) = page_token {
            url.push_str(&format!("&token={}", encode(token)));
        }
        let answer = self.authorized(&Call::get(url)).await?;
        match answer.status {
            200 => {}
            400 if page_token.is_some() => return Err(VerifyFailure::Rejected),
            status => return Err(unavailable(format!("Google answered {status} for the voided purchases{}", google_reason(&answer.body)))),
        }
        let purchases = match answer.body.get("voidedPurchases") {
            Some(list) => serde_json::from_value(list.clone()).map_err(|_| unavailable("Google answered unreadable voided purchases"))?,
            None => Vec::new(),
        };
        let next_page_token = answer
            .body
            .pointer("/tokenPagination/nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        Ok(VoidedPage { purchases, next_page_token })
    }
}

/// What one run of the voided purchases sweep came to for a tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoidedSweep {
    pub pages: u32,
    pub voided: u32,
    /// Credited packs whose credits this run took back.
    pub reversed: u32,
    /// Orders no credit had seen yet: recorded, so their validate call is refused.
    pub recorded: u32,
    /// Orders an earlier run or delivery had settled already.
    pub settled: u32,
    /// Orders whose reversal failed; they are asked about again by the next run.
    pub failed: u32,
    /// True when every page was read and every order settled, so the next run starts from this one.
    pub finished: bool,
}

/// Takes back the credits of every pack a voided Google Play order of the tenant bought. Google lists the one-time
/// purchases refunded, charged back or canceled in the run's window, page by page; each order goes through the refund
/// path an App Store refund takes (`ledger_refund::reverse_play_order`, the `purchase_refunds` claim under
/// `gp:<orderId>`), which makes a repeat harmless and refuses a later credit of the same order. A window with more pages
/// than one run reads, or a page with an order that could not be settled, goes on from that page in the next run; the
/// window only moves on once it was read through.
pub async fn sweep_voided<T: Transport, S: Signer, D: D1Exec, W: WalletTransport>(
    api: &PlayApi<'_, T, S>,
    db: &D,
    wallets: Option<&W>,
    tenant: &PlayTenant,
    now_ms: i64,
) -> Result<VoidedSweep, AppError> {
    let state = load_state(db, &tenant.package).await?;
    let mut report = VoidedSweep::default();
    let (window, mut page_token) = match &state.resume {
        Some(resume) => (resume.window, Some(resume.page_token.clone())),
        None => (voided_window(state.swept_until_ms, now_ms), None),
    };
    loop {
        if report.pages == PAGES_PER_RUN {
            log::info(&format!("google play voided sweep of {} goes on from page {PAGES_PER_RUN} in the next run", tenant.package));
            return Ok(report);
        }
        let page = match api.voided_page(&tenant.package, &window, page_token.as_deref()).await {
            Ok(page) => page,
            Err(VerifyFailure::Rejected) => {
                log::error(&format!("google play refused the saved page of {}'s voided purchases; the window starts over", tenant.package));
                save_resume(db, &tenant.package, None).await?;
                return Ok(report);
            }
            Err(failure) => {
                log::error(&format!("google play voided sweep of {} postponed: {failure:?}", tenant.package));
                return Ok(report);
            }
        };
        report.pages += 1;
        let failed_before = report.failed;
        for voided in &page.purchases {
            settle_one(db, wallets, tenant, voided, &mut report).await;
        }
        if report.failed > failed_before {
            let again = page_token.map(|token| Resume { window, page_token: token });
            save_resume(db, &tenant.package, again.as_ref()).await?;
            return Ok(report);
        }
        match page.next_page_token {
            Some(next) => {
                save_resume(db, &tenant.package, Some(&Resume { window, page_token: next.clone() })).await?;
                page_token = Some(next);
            }
            None => break,
        }
    }
    finish_window(db, &tenant.package, window.end_ms).await?;
    report.finished = true;
    Ok(report)
}

async fn settle_one<D: D1Exec, W: WalletTransport>(db: &D, wallets: Option<&W>, tenant: &PlayTenant, voided: &VoidedPurchase, report: &mut VoidedSweep) {
    let Some(payment_id) = voided.payment_id() else { return };
    report.voided += 1;
    match ledger_refund::reverse_play_order(db, wallets, &tenant.app_id, &payment_id).await {
        Ok(Outcome::Reversed { .. }) => report.reversed += 1,
        Ok(Outcome::Recorded) => report.recorded += 1,
        Ok(Outcome::Duplicate { .. }) | Ok(Outcome::Refused(_)) => report.settled += 1,
        Err(error) => {
            log::error(&format!("google play refund of {payment_id} could not be reversed: {error:?}"));
            report.failed += 1;
        }
    }
}

/// Where the sweep of a package stands; the default for one never swept.
pub async fn load_state(db: &impl D1Exec, package: &str) -> Result<SweepState, AppError> {
    let rows = db.rows("SELECT * FROM play_voided_sweeps WHERE package_name = ?1", &[package.into()]).await?;
    let Some(row) = rows.first() else { return Ok(SweepState::default()) };
    let resume = match (int(row, "window_start_ms"), int(row, "window_end_ms"), text(row, "page_token").filter(|token| !token.is_empty())) {
        (Some(start_ms), Some(end_ms), Some(token)) => Some(Resume { window: VoidedWindow { start_ms, end_ms }, page_token: token.to_string() }),
        _ => None,
    };
    Ok(SweepState { swept_until_ms: int(row, "swept_until_ms"), resume })
}

async fn ensure_row(db: &impl D1Exec, package: &str) -> Result<(), AppError> {
    db.exec(
        "INSERT OR IGNORE INTO play_voided_sweeps (package_name, updated_at) VALUES (?1, ?2)",
        &[package.into(), Utc::now().to_rfc3339().into()],
    )
    .await?;
    Ok(())
}

/// Keeps the page the next run goes on from, or forgets it.
async fn save_resume(db: &impl D1Exec, package: &str, resume: Option<&Resume>) -> Result<(), AppError> {
    ensure_row(db, package).await?;
    let (start, end, token) = match resume {
        Some(resume) => (Some(resume.window.start_ms), Some(resume.window.end_ms), Some(resume.page_token.as_str())),
        None => (None, None, None),
    };
    db.exec(
        "UPDATE play_voided_sweeps SET window_start_ms = ?2, window_end_ms = ?3, page_token = ?4, updated_at = ?5 WHERE package_name = ?1",
        &[package.into(), start.into(), end.into(), token.into(), Utc::now().to_rfc3339().into()],
    )
    .await?;
    Ok(())
}

/// Records that a window was read through to `end_ms`. It never moves back, so an older
/// run finishing late cannot undo a newer one.
async fn finish_window(db: &impl D1Exec, package: &str, end_ms: i64) -> Result<(), AppError> {
    ensure_row(db, package).await?;
    db.exec(
        "UPDATE play_voided_sweeps SET swept_until_ms = MAX(COALESCE(swept_until_ms, 0), ?2), window_start_ms = NULL, window_end_ms = NULL,
             page_token = NULL, updated_at = ?3
         WHERE package_name = ?1",
        &[package.into(), end_ms.into(), Utc::now().to_rfc3339().into()],
    )
    .await?;
    Ok(())
}
