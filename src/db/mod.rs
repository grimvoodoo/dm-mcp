//! Campaign database: SQLite connection opener (PRAGMAs applied per docs/architecture.md)
//! and schema migration.
//!
//! The MCP runs one campaign per process, so there's one SQLite connection for the process
//! lifetime. Callers share it via `Arc<Mutex<Connection>>` through the [`DbHandle`] alias —
//! good enough for single-campaign-per-process, and simple enough that we don't need a
//! pool. Later phases may add a queue or specialised writer task if contention shows up.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::DbConfig;

pub mod schema;

/// Shared handle on the campaign DB connection. Cheap to clone — Arc bump.
pub type DbHandle = Arc<Mutex<Connection>>;

/// Open the campaign database at `cfg.path`, apply every PRAGMA from `cfg`, run migrations,
/// and return a shared handle. Creates the file if it doesn't exist.
pub fn open(cfg: &DbConfig) -> Result<DbHandle> {
    let mut conn = open_connection(&cfg.path, cfg)
        .with_context(|| format!("failed to open SQLite DB at {}", cfg.path.display()))?;
    schema::migrate(&mut conn).context("apply schema migrations")?;
    Ok(Arc::new(Mutex::new(conn)))
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

        let _db = open(&test_cfg(db_path.clone())).expect("open");
        assert!(db_path.exists(), "db file should be created on open");
    }

    #[test]
    fn reopen_is_idempotent() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_path = tmp.path().join("reopen.db");
        {
            let _first = open(&test_cfg(db_path.clone())).expect("first open");
        }
        let _second = open(&test_cfg(db_path.clone())).expect("second open");
        assert!(db_path.exists());
    }

    #[test]
    fn pragmas_are_applied() {
        let tmp = TempDir::new().expect("tmpdir");
        let db_path = tmp.path().join("pragma.db");
        let db = open(&test_cfg(db_path)).expect("open");
        let conn = db.lock().unwrap();

        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_uppercase(), "WAL");

        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            fk, 1,
            "foreign_keys should be ON (hard-coded for correctness)"
        );
    }
}
