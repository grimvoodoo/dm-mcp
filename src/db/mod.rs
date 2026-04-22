//! Campaign database: SQLite connection opener (PRAGMAs applied per docs/architecture.md)
//! and schema migration.
//!
//! The MCP runs one campaign per process, so there's one DB connection for the process
//! lifetime. Callers obtain a `Database` handle at startup and share it via `Arc`.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::DbConfig;

pub mod schema;

/// Wrapper around a single SQLite connection. Phase 2 holds the connection directly; when
/// we need concurrent access we'll add a mutex or switch to a pool.
pub struct Database {
    connection: Connection,
}

impl Database {
    /// Open the campaign database at `cfg.path`, apply every PRAGMA from `cfg`, and run
    /// migrations. Creates the file if it doesn't exist.
    pub fn open(cfg: &DbConfig) -> Result<Self> {
        let conn = open_connection(&cfg.path, cfg)
            .with_context(|| format!("failed to open SQLite DB at {}", cfg.path.display()))?;
        let mut db = Self { connection: conn };
        schema::migrate(&mut db.connection).context("apply schema migrations")?;
        Ok(db)
    }

    /// Borrow the underlying connection.
    #[allow(dead_code)] // consumed by tools in Phase 3+
    pub fn conn(&self) -> &Connection {
        &self.connection
    }
}

fn open_connection(path: &Path, cfg: &DbConfig) -> Result<Connection> {
    let conn = Connection::open(path).context("rusqlite open")?;
    apply_pragmas(&conn, cfg).context("apply PRAGMAs")?;
    Ok(conn)
}

/// Set the tuning PRAGMAs. `foreign_keys=ON` is hard-coded (correctness, not tuning);
/// everything else reads from `DbConfig` so operators can override via env vars.
fn apply_pragmas(conn: &Connection, cfg: &DbConfig) -> Result<()> {
    // These string values have already been validated by Config::from_env — they cannot
    // be arbitrary user input at this point.
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = {jm};
         PRAGMA synchronous  = {sy};
         PRAGMA mmap_size    = {mm};
         PRAGMA cache_size   = {cs};
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store   = MEMORY;",
        jm = cfg.journal_mode,
        sy = cfg.synchronous,
        mm = cfg.mmap_size,
        cs = cfg.cache_size,
    ))
    .context("PRAGMA batch")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_cfg(path: PathBuf) -> DbConfig {
        DbConfig {
            path,
            journal_mode: "WAL".into(),
            synchronous: "NORMAL".into(),
            mmap_size: 1024 * 1024,
            cache_size: -1024,
        }
    }

    #[test]
    fn open_creates_file_and_schema() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_path = tmp.path().join("test.db");
        assert!(!db_path.exists(), "db file should not exist yet");

        let _db = Database::open(&test_cfg(db_path.clone())).expect("open");
        assert!(db_path.exists(), "db file should be created on open");
    }

    #[test]
    fn reopen_is_idempotent() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_path = tmp.path().join("reopen.db");
        {
            let _first = Database::open(&test_cfg(db_path.clone())).expect("first open");
        }
        let _second = Database::open(&test_cfg(db_path.clone())).expect("second open");
        assert!(db_path.exists());
    }

    #[test]
    fn pragmas_are_applied() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_path = tmp.path().join("pragma.db");
        let db = Database::open(&test_cfg(db_path)).expect("open");

        let journal: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_uppercase(), "WAL");

        let fk: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            fk, 1,
            "foreign_keys should be ON (hard-coded for correctness)"
        );
    }
}
