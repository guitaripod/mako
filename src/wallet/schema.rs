/// The statements that create the object's tables, run one by one on every start. They are
/// idempotent, so an existing object keeps its data.
pub const STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS entries (
        seq           INTEGER PRIMARY KEY AUTOINCREMENT,
        idem_key      TEXT NOT NULL UNIQUE,
        tenant        TEXT NOT NULL,
        kind          TEXT NOT NULL CHECK (kind IN ('purchase','bonus','migrate','spend','refund','reversal','adjust','marker')),
        amount        INTEGER NOT NULL,
        balance_after INTEGER NOT NULL CHECK (balance_after >= 0),
        source        TEXT NOT NULL,
        reference     TEXT,
        description   TEXT NOT NULL,
        at            INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS entries_tenant_seq ON entries(tenant, seq)",
    "CREATE TABLE IF NOT EXISTS balances (
        tenant     TEXT PRIMARY KEY,
        balance    INTEGER NOT NULL CHECK (balance >= 0),
        purchased  INTEGER NOT NULL DEFAULT 0,
        spent      INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '1'), ('sharing', 'off')",
];
