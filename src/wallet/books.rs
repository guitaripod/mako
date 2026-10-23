use super::types::{Balance, Entry, StoredEntry, WalletError};

/// Storage behind one wallet object. Every method is synchronous so a caller can read and write
/// without an `await` in between, which is what makes a run of them atomic inside the object.
pub trait Books {
    /// The entry stored under an idempotency key.
    fn find(&self, key: &str) -> Result<Option<StoredEntry>, WalletError>;

    /// The totals of one tenant, all zero for a tenant never seen.
    fn balance(&self, tenant: &str) -> Result<Balance, WalletError>;

    /// Writes the new totals and appends the entry in one step, returning its `seq`.
    fn append(&mut self, entry: &Entry, after: &Balance, now: i64) -> Result<i64, WalletError>;

    /// A tenant's entries, newest first, strictly older than `before` when given.
    fn entries(&self, tenant: &str, before: Option<i64>, limit: usize) -> Result<Vec<StoredEntry>, WalletError>;

    /// How many entries a tenant holds that move or record credits, claim markers not counted.
    fn count(&self, tenant: &str) -> Result<usize, WalletError>;

    /// A tenant's entries in the order they were written.
    fn chronological(&self, tenant: &str) -> Result<Vec<StoredEntry>, WalletError>;

    /// Every tenant that has entries or a balance row.
    fn tenants(&self) -> Result<Vec<String>, WalletError>;

    /// Deletes a tenant's entries and balance row, returning how many entries went.
    fn erase(&mut self, tenant: &str) -> Result<usize, WalletError>;

    /// A value kept in the object's `meta` table.
    fn meta(&self, key: &str) -> Result<Option<String>, WalletError>;

    /// Stores a `meta` value, replacing the old one.
    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), WalletError>;

    /// Removes a `meta` value.
    fn delete_meta(&mut self, key: &str) -> Result<(), WalletError>;

    /// The `meta` keys that start with `prefix`, in key order.
    fn meta_keys(&self, prefix: &str) -> Result<Vec<String>, WalletError>;
}
