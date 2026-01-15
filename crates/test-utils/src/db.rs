//! Test database utilities for Postgres + SQLx.
//!
//! Features:
//! - Per-test temporary database, named using the test name.
//! - Automatic migrations.
//! - Automatic cleanup on success.
//! - Keep DBs on failure or when `TEST_KEEP_DB` is set.
//!
//! Requirements:
//! - env var `TEST_ADMIN_DATABASE_URL` pointing to an "admin" DB
//!   (e.g. postgres://user:pass@localhost/postgres) with CREATE/DROP DATABASE
//!   permissions.

use std::{future::Future, pin::Pin};

use anyhow::Result;
use sqlx::{Connection, Executor, PgConnection, PgPool, postgres::PgPoolOptions};
use url::Url;
use uuid::Uuid;

/// Create a fresh temporary test database, run `f` with a connection pool to it,
/// then clean up afterward.
///
/// - DB name is derived from `test_name` + a random suffix.
/// - Migrations are applied to the new DB.
/// - On success and if `TEST_KEEP_DB` is **not** set, the DB is dropped.
/// - On error or if `TEST_KEEP_DB` **is** set, the DB is kept (and a message is logged).
///
/// If the test panics inside `f`, cleanup is **not** run (DB is kept), which is
/// usually what you want for debugging.
pub async fn with_test_db<F, T>(test_name: &str, f: F) -> Result<T>
where
    F: for<'a> FnOnce(&'a PgPool) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
{
    dotenvy::from_filename(".env").ok();

    // Admin URL: e.g. postgres://user:pass@localhost/postgres
    let admin_url = std::env::var("TEST_ADMIN_DATABASE_URL")
        .expect("TEST_ADMIN_DATABASE_URL must be set for DB tests");

    // Connect to admin DB
    let mut admin_conn = PgConnection::connect(&admin_url).await?;

    // Construct a safe, reasonably short DB name
    let db_name = make_db_name(test_name);

    // Create the database
    admin_conn
        .execute(format!(r#"CREATE DATABASE "{}""#, db_name).as_str())
        .await?;

    // Build a URL that points to the newly created DB
    let mut db_url = Url::parse(&admin_url)?;
    db_url.set_path(&format!("/{}", db_name));

    // Connect a pool to the test DB
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(db_url.as_str())
        .await?;

    // Run migrations from the ironflow crate.
    // Path is relative to CARGO_MANIFEST_DIR.
    sqlx::migrate!("../ironflow/migrations").run(&pool).await?;

    // Run the user-provided test body
    let result = f(&pool).await;

    let keep = std::env::var("TEST_KEEP_DB").is_ok();

    if result.is_ok() && !keep {
        // On success, and TEST_KEEP_DB not set, drop the DB.

        // Close the pool first to release all connections.
        pool.close().await;

        if let Err(e) = admin_conn
            .execute(format!(r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE);"#, db_name).as_str())
            .await
        {
            eprintln!(
                "[with_test_db] Failed to drop database '{}': {}",
                db_name, e
            );
        } else {
            eprintln!("[with_test_db] Dropped database '{}'", db_name);
        }
    } else {
        eprintln!(
            "[with_test_db] Keeping database '{}' (error or TEST_KEEP_DB set)",
            db_name
        );
    }

    result
}

/// Build a valid Postgres database name from a test name.
///
/// - lowercases
/// - replaces non-ascii-alphanumeric with '_'
/// - truncates to stay under Postgres's 63-char limit when combined with prefix + UUID.
fn make_db_name(test_name: &str) -> String {
    // sanitize: lowercase, non-alnum -> '_'
    let mut safe: String = test_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    // remove leading/trailing underscores (optional, cosmetic)
    while safe.starts_with('_') {
        safe.remove(0);
    }
    while safe.ends_with('_') {
        safe.pop();
    }

    // Postgres identifier limit is 63 bytes.
    // We'll use: "test_" + safe + "_" + 32-char hex UUID
    let prefix = "test_";
    let suffix_len = 1 + 32; // "_" + uuid_simple
    let max_ident = 63usize;

    let max_safe_len = max_ident
        .saturating_sub(prefix.len())
        .saturating_sub(suffix_len);

    if safe.len() > max_safe_len {
        safe.truncate(max_safe_len);
    }

    let uuid_part = Uuid::now_v7().simple(); // 32-char hex, time-ordered
    format!("{prefix}{safe}_{uuid_part}")
}

/// Macro to define a DB-backed async test.
///
/// Usage:
///
/// ```ignore
/// use test_utils::db_test;
///
/// db_test!(test_shutdown_releases_all_leases, |pool| {
///     // `pool` is &PgPool
///     sqlx::query!("SELECT 1").execute(pool).await?;
///     Ok(())
/// });
/// ```
///
/// This expands to:
/// - `#[tokio::test(flavor = "multi_thread")]`
/// - a call to `with_test_db(stringify!(test_name), |pool| async move { ... })`
#[macro_export]
macro_rules! db_test {
    ($name:ident, |$pool:ident| $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() -> anyhow::Result<()> {
            // Import the helper from this crate/module.
            // Adjust the path if you put `with_test_db` somewhere else.
            use $crate::db::with_test_db;

            let test_name = stringify!($name);

            with_test_db(test_name, |$pool| {
                let fut = async move { $body };
                Box::pin(fut)
            })
            .await
        }
    };
}
