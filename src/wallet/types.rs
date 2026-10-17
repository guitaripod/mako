use serde::{Deserialize, Serialize};

/// The reserved tenant of the future shared pool; it is rejected everywhere until P4.00.
pub const POOL_TENANT: &str = "*";

/// The largest absolute amount one entry may carry, far below the JavaScript safe integer limit.
pub const MAX_AMOUNT: i64 = 1_000_000_000;

/// What a ledger entry represents; the string forms match the `entries.kind` CHECK constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Purchase,
    Bonus,
    Migrate,
    Spend,
    Refund,
    Reversal,
    Adjust,
    Marker,
}

impl Kind {
    /// The stored spelling of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Purchase => "purchase",
            Kind::Bonus => "bonus",
            Kind::Migrate => "migrate",
            Kind::Spend => "spend",
            Kind::Refund => "refund",
            Kind::Reversal => "reversal",
            Kind::Adjust => "adjust",
            Kind::Marker => "marker",
        }
    }

    /// Parses a stored kind, `None` for anything the schema would not accept.
    pub fn parse(value: &str) -> Option<Kind> {
        Some(match value {
            "purchase" => Kind::Purchase,
            "bonus" => Kind::Bonus,
            "migrate" => Kind::Migrate,
            "spend" => Kind::Spend,
            "refund" => Kind::Refund,
            "reversal" => Kind::Reversal,
            "adjust" => Kind::Adjust,
            "marker" => Kind::Marker,
            _ => return None,
        })
    }
}

/// A requested ledger movement; `amount` is signed and `key` is the idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub tenant: String,
    pub kind: Kind,
    pub amount: i64,
    pub source: String,
    #[serde(default)]
    pub reference: Option<String>,
    pub description: String,
}

/// An entry as stored, with its position and the tenant balance right after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEntry {
    pub seq: i64,
    pub key: String,
    pub tenant: String,
    pub kind: Kind,
    pub amount: i64,
    pub balance_after: i64,
    pub source: String,
    pub reference: Option<String>,
    pub description: String,
    pub at: i64,
}

/// The derived per-tenant totals kept in the `balances` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub balance: i64,
    pub purchased: i64,
    pub spent: i64,
}

/// The result of applying an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Applied { balance: i64, seq: i64 },
    Replayed { balance_after: i64, seq: i64 },
    Insufficient { balance: i64 },
}

/// The result of a clamped debit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reversed {
    pub taken: i64,
    pub balance: i64,
    pub seq: i64,
    pub replayed: bool,
}

/// The result of moving a D1 balance into the wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Migrated {
    pub amount: i64,
    pub balance: i64,
    pub seq: i64,
    pub replayed: bool,
}

/// The outcome of recomputing every tenant from the entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub ok: bool,
    pub tenants: usize,
    pub entries: usize,
    pub problems: Vec<String>,
}

/// Why a wallet operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletError {
    BadRequest(String),
    Conflict(String),
    Storage(String),
}

impl WalletError {
    /// The HTTP status the object answers with.
    pub fn status(&self) -> u16 {
        match self {
            WalletError::BadRequest(_) => 400,
            WalletError::Conflict(_) => 409,
            WalletError::Storage(_) => 500,
        }
    }

    /// The stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            WalletError::BadRequest(_) => "bad_request",
            WalletError::Conflict(_) => "idempotency_conflict",
            WalletError::Storage(_) => "storage_error",
        }
    }

    /// The human-readable detail.
    pub fn message(&self) -> &str {
        match self {
            WalletError::BadRequest(m) | WalletError::Conflict(m) | WalletError::Storage(m) => m,
        }
    }
}

impl std::fmt::Display for WalletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}
