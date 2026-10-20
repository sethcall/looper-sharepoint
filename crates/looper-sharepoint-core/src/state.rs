//! Local state DB: one SQLite file per configured library. See `specs/m1-design.md` §1.
//!
//! Each row holds the remote state as of the last applied delta (`r_*`) and the local state as of the last time we
//! wrote or verified the file (`l_*`, the "base"). Items are keyed by Graph item id, so renames and moves never
//! re-download.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

pub const SCHEMA_VERSION: i64 = 2;
/// Run history kept per library.
pub const MAX_RUNS: i64 = 200;

const MIGRATIONS: &[&str] = &[
    // v1
    r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE items (
  item_id        TEXT PRIMARY KEY,
  parent_id      TEXT,
  remote_name    TEXT NOT NULL,
  is_dir         INTEGER NOT NULL,
  is_root        INTEGER NOT NULL DEFAULT 0,
  r_etag         TEXT,
  r_quickxor     TEXT,
  r_size         INTEGER,
  r_mtime        TEXT,
  local_path     TEXT,
  local_key      TEXT,
  l_size         INTEGER,
  l_mtime_ns     INTEGER,
  l_quickxor     TEXT,
  state          TEXT NOT NULL,
  reason         TEXT,
  updated_at     TEXT NOT NULL
);
CREATE INDEX items_parent ON items(parent_id);
CREATE UNIQUE INDEX items_local_key ON items(local_key) WHERE local_key IS NOT NULL;
"#,
    // v2: run history. `record` is the engine's JSON run record; the other columns are for querying.
    r#"
CREATE TABLE runs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at  TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  record      TEXT NOT NULL
);
"#,
];

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("state DB schema version {0} is newer than this build supports ({SCHEMA_VERSION})")]
    TooNew(i64),
}

pub type Result<T> = std::result::Result<T, StateError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// Local copy matches remote (or the item is intentionally not materialized, e.g. filtered).
    Synced,
    /// Known remotely, local copy not yet written.
    Pending,
    /// Deliberately not synced (filtered, unrepresentable name, collision); see `reason`.
    Skipped,
    /// Local and remote both changed, or local exists and differs; not overwritten. See `reason`.
    Conflict,
    /// Last attempt failed; retried next run. See `reason`.
    Error,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::Conflict => "conflict",
            Self::Error => "error",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "synced" => Self::Synced,
            "skipped" => Self::Skipped,
            "conflict" => Self::Conflict,
            "error" => Self::Error,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub item_id: String,
    pub parent_id: Option<String>,
    pub remote_name: String,
    pub is_dir: bool,
    pub is_root: bool,
    pub r_etag: Option<String>,
    pub r_quickxor: Option<String>,
    pub r_size: Option<i64>,
    pub r_mtime: Option<String>,
    /// Relative to the library's local folder, `/`-separated, as written on disk.
    pub local_path: Option<String>,
    pub local_key: Option<String>,
    pub l_size: Option<i64>,
    pub l_mtime_ns: Option<i64>,
    pub l_quickxor: Option<String>,
    pub state: ItemState,
    pub reason: Option<String>,
}

impl ItemRow {
    pub fn new(item_id: &str, remote_name: &str, is_dir: bool) -> Self {
        Self {
            item_id: item_id.into(),
            parent_id: None,
            remote_name: remote_name.into(),
            is_dir,
            is_root: false,
            r_etag: None,
            r_quickxor: None,
            r_size: None,
            r_mtime: None,
            local_path: None,
            local_key: None,
            l_size: None,
            l_mtime_ns: None,
            l_quickxor: None,
            state: ItemState::Pending,
            reason: None,
        }
    }

    pub fn is_materialized(&self) -> bool {
        self.local_path.is_some()
    }

    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            item_id: r.get("item_id")?,
            parent_id: r.get("parent_id")?,
            remote_name: r.get("remote_name")?,
            is_dir: r.get("is_dir")?,
            is_root: r.get("is_root")?,
            r_etag: r.get("r_etag")?,
            r_quickxor: r.get("r_quickxor")?,
            r_size: r.get("r_size")?,
            r_mtime: r.get("r_mtime")?,
            local_path: r.get("local_path")?,
            local_key: r.get("local_key")?,
            l_size: r.get("l_size")?,
            l_mtime_ns: r.get("l_mtime_ns")?,
            l_quickxor: r.get("l_quickxor")?,
            state: ItemState::parse(&r.get::<_, String>("state")?),
            reason: r.get("reason")?,
        })
    }
}

pub struct StateDb {
    conn: Connection,
    path: PathBuf,
}

/// Well-known `meta` keys.
pub mod meta {
    pub const TENANT_ID: &str = "tenant_id";
    pub const SITE_ID: &str = "site_id";
    pub const DRIVE_ID: &str = "drive_id";
    pub const LOCAL_ROOT: &str = "local_root";
    pub const DELTA_LINK: &str = "delta_link";
    pub const LAST_SYNC_AT: &str = "last_sync_at";
    pub const LAST_FULL_SYNC_AT: &str = "last_full_sync_at";
    /// Two-way: paths deleted in SharePoint but edited locally, still to upload again once their name proves free
    /// (JSON array). Kept across runs so a failed attempt never falls back to a plain new upload.
    pub const REUPLOADS: &str = "reuploads";
}

impl StateDb {
    /// `<data_dir>/state/<first 16 hex of sha256(drive_id)>.sqlite`
    pub fn path_for(data_dir: &Path, drive_id: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(drive_id.as_bytes());
        let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        data_dir.join("state").join(format!("{name}.sqlite"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut db = Self { conn, path: path.to_owned() };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut db = Self { conn: Connection::open_in_memory()?, path: PathBuf::from(":memory:") };
        db.migrate()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StateError::TooNew(version));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn get(&self, item_id: &str) -> Result<Option<ItemRow>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM items WHERE item_id = ?1", [item_id], ItemRow::from_row)
            .optional()?)
    }

    pub fn root(&self) -> Result<Option<ItemRow>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM items WHERE is_root = 1", [], ItemRow::from_row)
            .optional()?)
    }

    pub fn by_local_key(&self, key: &str) -> Result<Option<ItemRow>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM items WHERE local_key = ?1", [key], ItemRow::from_row)
            .optional()?)
    }

    pub fn put(&self, row: &ItemRow) -> Result<()> {
        self.conn.execute(
            // Upsert on item_id only. INSERT OR REPLACE would also silently delete any *other* row whose
            // local_key collides; a collision must be an error instead.
            "INSERT INTO items (item_id, parent_id, remote_name, is_dir, is_root, r_etag, r_quickxor, r_size,
               r_mtime, local_path, local_key, l_size, l_mtime_ns, l_quickxor, state, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(item_id) DO UPDATE SET parent_id = excluded.parent_id, remote_name = excluded.remote_name,
               is_dir = excluded.is_dir, is_root = excluded.is_root, r_etag = excluded.r_etag,
               r_quickxor = excluded.r_quickxor, r_size = excluded.r_size, r_mtime = excluded.r_mtime,
               local_path = excluded.local_path, local_key = excluded.local_key, l_size = excluded.l_size,
               l_mtime_ns = excluded.l_mtime_ns, l_quickxor = excluded.l_quickxor, state = excluded.state,
               reason = excluded.reason, updated_at = excluded.updated_at",
            params![
                row.item_id,
                row.parent_id,
                row.remote_name,
                row.is_dir,
                row.is_root,
                row.r_etag,
                row.r_quickxor,
                row.r_size,
                row.r_mtime,
                row.local_path,
                row.local_key,
                row.l_size,
                row.l_mtime_ns,
                row.l_quickxor,
                row.state.as_str(),
                row.reason,
            ],
        )?;
        Ok(())
    }

    pub fn remove(&self, item_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM items WHERE item_id = ?1", [item_id])?;
        Ok(())
    }

    pub fn children(&self, parent_id: &str) -> Result<Vec<ItemRow>> {
        self.query("SELECT * FROM items WHERE parent_id = ?1", [parent_id])
    }

    /// The item and everything below it, parents before children.
    pub fn subtree(&self, item_id: &str) -> Result<Vec<ItemRow>> {
        self.query(
            "WITH RECURSIVE t(id, depth) AS (
               SELECT ?1, 0
               UNION ALL SELECT i.item_id, t.depth + 1 FROM items i JOIN t ON i.parent_id = t.id
             )
             SELECT items.* FROM items JOIN t ON items.item_id = t.id ORDER BY t.depth",
            [item_id],
        )
    }

    pub fn all(&self) -> Result<Vec<ItemRow>> {
        self.query("SELECT * FROM items", [])
    }

    pub fn in_states(&self, states: &[ItemState]) -> Result<Vec<ItemRow>> {
        let list = states.iter().map(|s| format!("'{}'", s.as_str())).collect::<Vec<_>>().join(",");
        self.query(&format!("SELECT * FROM items WHERE state IN ({list})"), [])
    }

    /// Number of files that currently have a local copy we manage.
    pub fn materialized_file_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE is_dir = 0 AND local_path IS NOT NULL",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn counts_by_state(&self) -> Result<Vec<(String, bool, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, is_dir, COUNT(*) FROM items WHERE is_root = 0 GROUP BY state, is_dir ORDER BY state")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Append a run record (JSON) and prune history to [`MAX_RUNS`].
    pub fn add_run(&self, started_at: &str, outcome: &str, record_json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (started_at, outcome, record) VALUES (?1, ?2, ?3)",
            params![started_at, outcome, record_json],
        )?;
        self.conn.execute(
            "DELETE FROM runs WHERE id <= (SELECT MAX(id) FROM runs) - ?1",
            [MAX_RUNS],
        )?;
        Ok(())
    }

    /// Most recent run records (JSON), newest first.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT record FROM runs ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The most recent run record (JSON) whose outcome is one of `outcomes`.
    pub fn last_run_with(&self, outcomes: &[&str]) -> Result<Option<String>> {
        let list = outcomes.iter().map(|o| format!("'{}'", o.replace('\'', ""))).collect::<Vec<_>>().join(",");
        Ok(self
            .conn
            .query_row(&format!("SELECT record FROM runs WHERE outcome IN ({list}) ORDER BY id DESC LIMIT 1"), [], |r| r.get(0))
            .optional()?)
    }

    /// Is a transaction open (between [`begin`](Self::begin) and `commit`/`rollback`)?
    pub fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    fn query<P: rusqlite::Params>(&self, sql: &str, p: P) -> Result<Vec<ItemRow>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, ItemRow::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_subtree() {
        let db = StateDb::open_in_memory().unwrap();
        let mut root = ItemRow::new("r", "root", true);
        root.is_root = true;
        db.put(&root).unwrap();
        let mut a = ItemRow::new("a", "A", true);
        a.parent_id = Some("r".into());
        a.local_path = Some("A".into());
        a.local_key = Some("a".into());
        db.put(&a).unwrap();
        let mut f = ItemRow::new("f", "f.txt", false);
        f.parent_id = Some("a".into());
        f.r_quickxor = Some("xyz".into());
        f.state = ItemState::Synced;
        db.put(&f).unwrap();

        assert_eq!(db.get("f").unwrap().unwrap(), f);
        assert!(db.root().unwrap().unwrap().is_root);
        let ids: Vec<_> = db.subtree("a").unwrap().into_iter().map(|r| r.item_id).collect();
        assert_eq!(ids, vec!["a", "f"]);
        assert_eq!(db.by_local_key("a").unwrap().unwrap().item_id, "a");

        let mut dup = ItemRow::new("b", "a", true);
        dup.local_key = Some("a".into());
        assert!(db.put(&dup).is_err(), "local_key must be unique");
    }

    #[test]
    fn run_history_is_pruned() {
        let db = StateDb::open_in_memory().unwrap();
        for i in 0..(MAX_RUNS + 5) {
            db.add_run("t", if i % 2 == 0 { "ok" } else { "failed" }, &format!("{{\"n\":{i}}}")).unwrap();
        }
        let recent = db.recent_runs(1000).unwrap();
        assert_eq!(recent.len() as i64, MAX_RUNS);
        assert_eq!(recent[0], format!("{{\"n\":{}}}", MAX_RUNS + 4));
        assert_eq!(db.last_run_with(&["failed"]).unwrap().unwrap(), format!("{{\"n\":{}}}", MAX_RUNS + 3));
    }

    #[test]
    fn meta_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = StateDb::path_for(dir.path(), "b!drive");
        {
            let db = StateDb::open(&p).unwrap();
            db.set_meta(meta::DELTA_LINK, "https://x").unwrap();
        }
        let db = StateDb::open(&p).unwrap();
        assert_eq!(db.get_meta(meta::DELTA_LINK).unwrap().as_deref(), Some("https://x"));
    }
}
