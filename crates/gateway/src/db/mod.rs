//! Storage for users, connections, permissions and login sessions, in the
//! spirit of Guacamole's JDBC authentication extensions.
//!
//! One code path serves PostgreSQL, MySQL/MariaDB and SQLite through sqlx's
//! `Any` driver. Queries are written with `?` placeholders and rewritten for
//! PostgreSQL by [`Database::sql`]; every integer column is `BIGINT` and ids
//! are strings, so rows decode the same way on every backend.

mod connections;
mod sessions;
mod users;

use std::borrow::Cow;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use sqlx::AnyPool;
use sqlx::any::AnyPoolOptions;
use sqlx::migrate::Migrator;

pub use connections::ConnectionRecord;
pub use users::{NewUser, User, UserUpdate};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");
static MYSQL_MIGRATIONS: Migrator = sqlx::migrate!("migrations/mysql");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    Mysql,
}

impl Dialect {
    pub fn from_url(url: &str) -> anyhow::Result<Self> {
        let scheme = url.split_once(':').map_or(url, |(s, _)| s);
        Ok(match scheme {
            "sqlite" => Self::Sqlite,
            "postgres" | "postgresql" => Self::Postgres,
            "mysql" | "mariadb" => Self::Mysql,
            other => bail!(
                "unsupported database URL scheme {other:?} (use postgres://, mysql:// or sqlite://)"
            ),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
        }
    }
}

/// Error from a repository operation.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Conflict,
    #[error(transparent)]
    Other(#[from] sqlx::Error),
}

impl DbError {
    fn from_sqlx(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::Database(db) if db.is_unique_violation() => Self::Conflict,
            _ => Self::Other(err),
        }
    }
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
    dialect: Dialect,
}

impl Database {
    /// Connects to `url` and applies pending migrations.
    pub async fn open(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let dialect = Dialect::from_url(url)?;
        let pool = AnyPoolOptions::new()
            .max_connections(max_connections.max(1))
            .connect(url)
            .await
            .with_context(|| format!("connecting to the {} database", dialect.name()))?;
        let db = Self { pool, dialect };
        let migrator = match dialect {
            Dialect::Sqlite => &SQLITE_MIGRATIONS,
            Dialect::Postgres => &POSTGRES_MIGRATIONS,
            Dialect::Mysql => &MYSQL_MIGRATIONS,
        };
        migrator
            .run(&db.pool)
            .await
            .context("applying database migrations")?;
        Ok(db)
    }

    /// URL of the SQLite database used when none is configured.
    pub fn sqlite_url(path: &Path) -> String {
        format!("sqlite://{}?mode=rwc", path.display())
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Adapts a query written with `?` placeholders to the backend.
    fn sql<'a>(&self, query: &'a str) -> Cow<'a, str> {
        if self.dialect != Dialect::Postgres || !query.contains('?') {
            return Cow::Borrowed(query);
        }
        let mut out = String::with_capacity(query.len() + 8);
        let mut n = 0;
        for c in query.chars() {
            if c == '?' {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            } else {
                out.push(c);
            }
        }
        Cow::Owned(out)
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Shorthand for building a query with dialect-adjusted placeholders.
macro_rules! query {
    ($db:expr, $sql:expr) => {
        sqlx::query::<sqlx::Any>(sqlx::AssertSqlSafe($db.sql($sql).into_owned()))
    };
}
pub(crate) use query;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh SQLite database in a temporary directory.
    pub async fn sqlite() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&Database::sqlite_url(&dir.path().join("test.db")), 2)
            .await
            .unwrap();
        (db, dir)
    }

    /// The databases to run repository tests against: SQLite always, plus
    /// PostgreSQL / MySQL when `MOLCAJETE_TEST_POSTGRES_URL` /
    /// `MOLCAJETE_TEST_MYSQL_URL` point at an empty, disposable database.
    pub async fn every_backend() -> Vec<(Database, Option<tempfile::TempDir>)> {
        let (db, dir) = sqlite().await;
        let mut all = vec![(db, Some(dir))];
        for var in ["MOLCAJETE_TEST_POSTGRES_URL", "MOLCAJETE_TEST_MYSQL_URL"] {
            if let Ok(url) = std::env::var(var) {
                let db = Database::open(&url, 2).await.unwrap();
                reset(&db).await;
                all.push((db, None));
            }
        }
        all
    }

    async fn reset(db: &Database) {
        for table in [
            "molcajete_session",
            "molcajete_connection_permission",
            "molcajete_connection_parameter",
            "molcajete_connection",
            "molcajete_user",
        ] {
            query!(db, &format!("DELETE FROM {table}"))
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }

    #[test]
    fn rewrites_placeholders_for_postgres_only() {
        assert_eq!(
            Dialect::from_url("postgresql://x/y").unwrap(),
            Dialect::Postgres
        );
        assert_eq!(Dialect::from_url("mariadb://x/y").unwrap(), Dialect::Mysql);
        assert_eq!(Dialect::from_url("sqlite://a.db").unwrap(), Dialect::Sqlite);
        assert!(Dialect::from_url("mssql://x").is_err());
    }

    #[tokio::test]
    async fn placeholders() {
        let (mut db, _dir) = sqlite().await;
        assert_eq!(db.sql("a = ? AND b = ?"), "a = ? AND b = ?");
        db.dialect = Dialect::Postgres;
        assert_eq!(db.sql("a = ? AND b = ?"), "a = $1 AND b = $2");
    }
}
