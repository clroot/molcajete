//! Storage for users, groups, connections, folders, grants and login
//! sessions, in the spirit of Guacamole's JDBC authentication extensions.
//!
//! One code path serves PostgreSQL, MySQL/MariaDB and SQLite through sqlx's
//! `Any` driver. Queries are written with `?` placeholders and rewritten for
//! PostgreSQL by [`Database::sql`]; every integer column is `BIGINT` and ids
//! are strings, so rows decode the same way on every backend.

mod connections;
mod folders;
mod grants;
mod groups;
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
pub use folders::Folder;
pub use grants::{Access, Principal, Target};
pub use groups::{Group, GroupUpdate};
pub use users::{NewUser, Role, User, UserUpdate};

static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("migrations/postgres");
static MYSQL_MIGRATIONS: Migrator = sqlx::migrate!("migrations/mysql");

fn migrator(dialect: Dialect) -> &'static Migrator {
    match dialect {
        Dialect::Sqlite => &SQLITE_MIGRATIONS,
        Dialect::Postgres => &POSTGRES_MIGRATIONS,
        Dialect::Mysql => &MYSQL_MIGRATIONS,
    }
}

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
    /// The change would leave no enabled administrator; nothing changed.
    #[error("the last administrator would be removed")]
    LastAdmin,
    /// The request does not make sense, e.g. a folder moved into itself.
    #[error("{0}")]
    Invalid(String),
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
        migrator(dialect)
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
        query!(db, "UPDATE molcajete_connection_group SET parent_id = NULL")
            .execute(&db.pool)
            .await
            .unwrap();
        for table in [
            "molcajete_session",
            "molcajete_connection_permission",
            "molcajete_connection_group_permission",
            "molcajete_group_connection_permission",
            "molcajete_group_connection_group_permission",
            "molcajete_group_member",
            "molcajete_group",
            "molcajete_connection_parameter",
            "molcajete_connection",
            "molcajete_connection_group",
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

    /// Every table, children first.
    const TABLES: &[&str] = &[
        "molcajete_session",
        "molcajete_connection_permission",
        "molcajete_connection_group_permission",
        "molcajete_group_connection_permission",
        "molcajete_group_connection_group_permission",
        "molcajete_group_member",
        "molcajete_group",
        "molcajete_connection_parameter",
        "molcajete_connection",
        "molcajete_connection_group",
        "molcajete_user",
        "_sqlx_migrations",
    ];

    /// The schema before roles, groups and folders.
    const BEFORE_GROUPS: i64 = 20261125000000;

    /// Data written by the first schema survives the later migrations:
    /// administrators keep administering and grants keep granting.
    #[tokio::test]
    async fn migrates_existing_data() {
        sqlx::any::install_default_drivers();
        let dir = tempfile::tempdir().unwrap();
        let mut urls = vec![Database::sqlite_url(&dir.path().join("old.db"))];
        for var in ["MOLCAJETE_TEST_POSTGRES_URL", "MOLCAJETE_TEST_MYSQL_URL"] {
            urls.extend(std::env::var(var).ok());
        }
        for url in urls {
            let dialect = Dialect::from_url(&url).unwrap();
            let name = dialect.name();
            let pool = AnyPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            let old = Database {
                pool: pool.clone(),
                dialect,
            };
            if dialect == Dialect::Mysql {
                query!(old, "SET FOREIGN_KEY_CHECKS = 0")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            for table in TABLES {
                query!(old, &format!("DROP TABLE IF EXISTS {table}"))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            if dialect == Dialect::Mysql {
                query!(old, "SET FOREIGN_KEY_CHECKS = 1")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            migrator(dialect)
                .run_to(BEFORE_GROUPS, &pool)
                .await
                .unwrap();
            for (id, username, is_admin) in [("u1", "root", 1), ("u2", "bob", 0)] {
                query!(
                    old,
                    "INSERT INTO molcajete_user (user_id, username, password_hash, is_admin, \
                     disabled, created_at, updated_at) VALUES (?, ?, 'h', ?, 0, 1, 1)"
                )
                .bind(id)
                .bind(username)
                .bind(is_admin as i64)
                .execute(&pool)
                .await
                .unwrap();
            }
            for id in ["c1", "c2"] {
                query!(
                    old,
                    "INSERT INTO molcajete_connection (connection_id, name, protocol, \
                     created_at, updated_at) VALUES (?, ?, 'vnc', 1, 1)"
                )
                .bind(id)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            }
            query!(
                old,
                "INSERT INTO molcajete_connection_permission (user_id, connection_id) \
                 VALUES ('u2', 'c1')"
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;

            let db = Database::open(&url, 2).await.unwrap();
            let root = db.user("u1").await.unwrap();
            let bob = db.user("u2").await.unwrap();
            assert_eq!((root.role, root.is_admin), (Role::Admin, true), "{name}");
            assert_eq!((bob.role, bob.is_admin), (Role::User, false), "{name}");
            assert_eq!(
                db.user_connection_ids("u2").await.unwrap(),
                ["c1"],
                "{name}"
            );
            assert_eq!(
                db.reachable_connection_ids("u2").await.unwrap(),
                ["c1"],
                "{name}"
            );
            assert_eq!(db.connection("c2").await.unwrap().parent_id, None, "{name}");
            assert!(db.list_groups().await.unwrap().is_empty(), "{name}");
            assert!(db.list_folders().await.unwrap().is_empty(), "{name}");
        }
    }
}
