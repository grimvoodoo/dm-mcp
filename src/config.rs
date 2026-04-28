//! Runtime configuration loaded once at startup from `DMMCP_`-prefixed environment variables.
//!
//! Every performance knob has a low-latency default. See the README "Configuration" section
//! and `docs/architecture.md` for the rationale behind each default.

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};

/// Top-level runtime configuration. `db`, `content_dir`, and `log_level` are read at startup
/// by subsystems that come online in later phases; the env var contract is validated here in
/// Phase 1 regardless.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Config {
    pub db: DbConfig,
    pub http: HttpConfig,
    pub log_level: String,
    pub content_dir: Option<PathBuf>,
}

/// Fields are consumed when the SQLite connection is opened (Phase 2). Phase 1 only validates
/// that the env-var contract parses correctly.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub path: PathBuf,
    pub journal_mode: String,
    pub synchronous: String,
    pub mmap_size: i64,
    pub cache_size: i64,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub bind: IpAddr,
    pub port: u16,
    /// Optional bearer token. If set, requests to `/mcp` must carry
    /// `Authorization: Bearer <token>` (constant-time compared) or get a 401.
    /// Unset (the default) means no auth — only safe for loopback or trusted
    /// network deployments.
    pub auth_token: Option<String>,
    /// Maximum body size for `/mcp` POSTs. Defaults to 1 MiB. Tunable via
    /// `DMMCP_HTTP_MAX_BODY_BYTES`. Defends against an oversized POST OOMing
    /// the process.
    pub max_body_bytes: usize,
}

impl HttpConfig {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }
}

impl Config {
    /// Load configuration from the process environment. Missing variables fall back to defaults.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            db: DbConfig {
                path: env_or("DMMCP_DB_PATH", PathBuf::from("./campaign.db"), |v| {
                    Ok(PathBuf::from(v))
                })?,
                journal_mode: env_or(
                    "DMMCP_DB_JOURNAL_MODE",
                    "WAL".to_string(),
                    parse_journal_mode,
                )?,
                synchronous: env_or(
                    "DMMCP_DB_SYNCHRONOUS",
                    "NORMAL".to_string(),
                    parse_synchronous,
                )?,
                mmap_size: env_or("DMMCP_DB_MMAP_SIZE", 67_108_864_i64, parse_str)?,
                cache_size: env_or("DMMCP_DB_CACHE_SIZE", -32_768_i64, parse_str)?,
            },
            http: HttpConfig {
                // Default loopback (#28) — single-tenant project per CLAUDE.md.
                // Operators that need network exposure set DMMCP_HTTP_BIND=0.0.0.0
                // explicitly. Was 0.0.0.0 before — that change is documented in
                // the PR / README "Configuration" section.
                bind: env_or("DMMCP_HTTP_BIND", IpAddr::from([127, 0, 0, 1]), parse_str)?,
                port: env_or("DMMCP_HTTP_PORT", 3000_u16, parse_str)?,
                auth_token: match env::var("DMMCP_HTTP_AUTH_TOKEN") {
                    Ok(v) if !v.is_empty() => Some(v),
                    _ => None,
                },
                max_body_bytes: env_or("DMMCP_HTTP_MAX_BODY_BYTES", 1_048_576_usize, parse_str)?,
            },
            log_level: env_or("DMMCP_LOG_LEVEL", "info".to_string(), parse_log_level)?,
            content_dir: match env::var("DMMCP_CONTENT_DIR") {
                Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
                _ => None,
            },
        })
    }
}

fn env_or<T>(key: &str, default: T, parse: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => parse(&v).with_context(|| format!("failed to parse {key}={v}")),
        _ => Ok(default),
    }
}

fn parse_str<T: FromStr>(v: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    v.parse::<T>()
        .map_err(|e| anyhow::anyhow!("parse error: {e}"))
}

/// Validate a tracing filter directive (e.g. `info`, `debug`, `dm_mcp=trace,warn`).
///
/// `EnvFilter::try_new` alone is too permissive — it accepts a typoed level like
/// `warng` as if it were a target name with no level (silently degrading to the
/// default). To catch the common operator-typo case we additionally enforce:
/// any single bare word (no `=` or `,`) must be one of the documented level names.
/// More structured directives still go through `EnvFilter::try_new` for full syntax
/// validation.
fn parse_log_level(v: &str) -> Result<String> {
    const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "off"];
    let trimmed = v.trim();
    if !trimmed.contains('=') && !trimmed.contains(',') {
        if !LEVELS.contains(&trimmed.to_ascii_lowercase().as_str()) {
            anyhow::bail!(
                "unknown level {trimmed:?}; expected one of {LEVELS:?} or a per-target \
                 directive like `dm_mcp=debug,warn`"
            );
        }
        return Ok(trimmed.to_ascii_lowercase());
    }
    tracing_subscriber::EnvFilter::try_new(trimmed)
        .map_err(|e| anyhow::anyhow!("invalid tracing filter directive: {e}"))?;
    Ok(trimmed.to_string())
}

/// Accept any documented SQLite `journal_mode` value. Normalise to uppercase so the PRAGMA
/// string produced later is consistent regardless of how the operator typed the env var.
/// Reject unknown values up-front rather than letting the DB connection fail at startup of
/// whichever phase first opens it.
fn parse_journal_mode(v: &str) -> Result<String> {
    let upper = v.to_ascii_uppercase();
    match upper.as_str() {
        "DELETE" | "TRUNCATE" | "PERSIST" | "MEMORY" | "WAL" | "OFF" => Ok(upper),
        _ => anyhow::bail!(
            "unsupported SQLite journal_mode {v:?}; expected one of DELETE, TRUNCATE, PERSIST, MEMORY, WAL, OFF"
        ),
    }
}

/// Accept any documented SQLite `synchronous` value. Same rationale as `parse_journal_mode`.
fn parse_synchronous(v: &str) -> Result<String> {
    let upper = v.to_ascii_uppercase();
    match upper.as_str() {
        "OFF" | "NORMAL" | "FULL" | "EXTRA" => Ok(upper),
        _ => anyhow::bail!(
            "unsupported SQLite synchronous mode {v:?}; expected one of OFF, NORMAL, FULL, EXTRA"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acquire-release lock so test threads don't race on env vars.
    /// (std::sync::Mutex — tests can't run in parallel while mutating env, this keeps them serial.)
    use std::sync::{Mutex, OnceLock};
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_clean_env<F: FnOnce()>(f: F) {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let keys = [
            "DMMCP_DB_PATH",
            "DMMCP_DB_JOURNAL_MODE",
            "DMMCP_DB_SYNCHRONOUS",
            "DMMCP_DB_MMAP_SIZE",
            "DMMCP_DB_CACHE_SIZE",
            "DMMCP_HTTP_BIND",
            "DMMCP_HTTP_PORT",
            "DMMCP_HTTP_AUTH_TOKEN",
            "DMMCP_HTTP_MAX_BODY_BYTES",
            "DMMCP_LOG_LEVEL",
            "DMMCP_CONTENT_DIR",
        ];
        for k in keys {
            // SAFETY: the lock above prevents concurrent access in the test harness.
            unsafe { env::remove_var(k) };
        }
        f();
    }

    #[test]
    fn defaults_match_documented_values() {
        with_clean_env(|| {
            let cfg = Config::from_env().expect("default load");
            assert_eq!(cfg.db.path, PathBuf::from("./campaign.db"));
            assert_eq!(cfg.db.journal_mode, "WAL");
            assert_eq!(cfg.db.synchronous, "NORMAL");
            assert_eq!(cfg.db.mmap_size, 67_108_864);
            assert_eq!(cfg.db.cache_size, -32_768);
            // Default is loopback (#28) — single-tenant safe default. Operators that
            // need network exposure set DMMCP_HTTP_BIND=0.0.0.0 explicitly.
            assert_eq!(cfg.http.bind, IpAddr::from([127, 0, 0, 1]));
            assert_eq!(cfg.http.auth_token, None);
            assert_eq!(cfg.http.max_body_bytes, 1_048_576);
            assert_eq!(cfg.http.port, 3000);
            assert_eq!(cfg.log_level, "info");
            assert!(cfg.content_dir.is_none());
        });
    }

    #[test]
    fn env_overrides_are_respected() {
        with_clean_env(|| {
            // SAFETY: env_lock is held by with_clean_env.
            unsafe {
                env::set_var("DMMCP_DB_PATH", "/data/campaign.db");
                env::set_var("DMMCP_HTTP_PORT", "8080");
                env::set_var("DMMCP_HTTP_BIND", "127.0.0.1");
                env::set_var("DMMCP_DB_MMAP_SIZE", "134217728");
                env::set_var("DMMCP_LOG_LEVEL", "debug");
                env::set_var("DMMCP_CONTENT_DIR", "/tmp/content");
            }
            let cfg = Config::from_env().expect("override load");
            assert_eq!(cfg.db.path, PathBuf::from("/data/campaign.db"));
            assert_eq!(cfg.http.port, 8080);
            assert_eq!(cfg.http.bind, IpAddr::from([127, 0, 0, 1]));
            assert_eq!(cfg.db.mmap_size, 134_217_728);
            assert_eq!(cfg.log_level, "debug");
            assert_eq!(cfg.content_dir, Some(PathBuf::from("/tmp/content")));
        });
    }

    #[test]
    fn malformed_number_produces_error() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_HTTP_PORT", "not-a-number") };
            let err = Config::from_env().expect_err("should fail on bad number");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("DMMCP_HTTP_PORT"),
                "error should name the offending var: {msg}"
            );
        });
    }

    #[test]
    fn journal_mode_is_normalised_to_uppercase() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_DB_JOURNAL_MODE", "wal") };
            let cfg = Config::from_env().expect("mixed-case WAL should be accepted");
            assert_eq!(cfg.db.journal_mode, "WAL");
        });
    }

    #[test]
    fn journal_mode_rejects_garbage() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_DB_JOURNAL_MODE", "garbage") };
            let err = Config::from_env().expect_err("should fail on unknown journal mode");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("DMMCP_DB_JOURNAL_MODE"),
                "error should name the offending var: {msg}"
            );
            assert!(
                msg.contains("garbage"),
                "error should include the offending value: {msg}"
            );
        });
    }

    #[test]
    fn synchronous_is_normalised_to_uppercase() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_DB_SYNCHRONOUS", "full") };
            let cfg = Config::from_env().expect("mixed-case FULL should be accepted");
            assert_eq!(cfg.db.synchronous, "FULL");
        });
    }

    #[test]
    fn synchronous_rejects_garbage() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_DB_SYNCHRONOUS", "loose") };
            let err = Config::from_env().expect_err("should fail on unknown synchronous mode");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("DMMCP_DB_SYNCHRONOUS"),
                "error should name the offending var: {msg}"
            );
            assert!(
                msg.contains("loose"),
                "error should include the offending value: {msg}"
            );
        });
    }

    #[test]
    fn log_level_accepts_simple_directive() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_LOG_LEVEL", "warn") };
            let cfg = Config::from_env().expect("simple directive should parse");
            assert_eq!(cfg.log_level, "warn");
        });
    }

    #[test]
    fn log_level_accepts_per_target_directive() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_LOG_LEVEL", "dm_mcp=trace,warn") };
            let cfg = Config::from_env().expect("per-target directive should parse");
            assert_eq!(cfg.log_level, "dm_mcp=trace,warn");
        });
    }

    #[test]
    fn log_level_rejects_garbage() {
        with_clean_env(|| {
            // SAFETY: env_lock is held.
            unsafe { env::set_var("DMMCP_LOG_LEVEL", "warng") };
            let err = Config::from_env().expect_err("should fail on misspelled level");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("DMMCP_LOG_LEVEL"),
                "error should name the offending var: {msg}"
            );
            assert!(
                msg.contains("warng"),
                "error should include the offending value: {msg}"
            );
        });
    }
}
