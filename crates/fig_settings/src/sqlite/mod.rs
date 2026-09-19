use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fig_util::directories::fig_data_dir;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::FromSql;
use rusqlite::{Connection, Error, ToSql, params};
use serde_json::Map;
use tracing::info;

use crate::Result;
use crate::error::DbOpenError;

const STATE_TABLE_NAME: &str = "state";
const AUTH_TABLE_NAME: &str = "auth_kv";
const POOL_MAX_SIZE: u32 = 4;

/// Keys that belong to the previous product's TCC / IME / login-item identity.
/// A renamed Easy Complete sqlite still contains them; they are not a Fastab
/// grant, launch hash, or SMAppService migration.
const PREVIOUS_PRODUCT_IDENTITY_KEYS: &[&str] = &[
    "desktop.accessibilityGranted",
    "input-method.launched-binary-sha256",
    "desktop.loginItemMigratedToSMAppService",
];

const CLEARED_PREVIOUS_PRODUCT_IDENTITY_KEY: &str = "desktop.clearedPreviousProductIdentity";

pub static DATABASE: LazyLock<Result<Db, DbOpenError>> = LazyLock::new(|| {
    let db = Db::new().map_err(|e| DbOpenError(e.to_string()))?;
    db.migrate().map_err(|e| DbOpenError(e.to_string()))?;
    forget_previous_product_identity_once(&db).map_err(|e| DbOpenError(e.to_string()))?;
    Ok(db)
});

pub fn database() -> Result<&'static Db, DbOpenError> {
    match DATABASE.as_ref() {
        Ok(db) => Ok(db),
        Err(err) => Err(err.clone()),
    }
}

#[derive(Debug)]
struct Migration {
    name: &'static str,
    sql: &'static str,
}

macro_rules! migrations {
    ($($name:expr),*) => {{
        &[
            $(
                Migration {
                    name: $name,
                    sql: include_str!(concat!("migrations/", $name, ".sql")),
                }
            ),*
        ]
    }};
}

const MIGRATIONS: &[Migration] = migrations![
    "000_migration_table",
    "001_history_table",
    "002_drop_history_in_ssh_docker",
    "003_improved_history_timing",
    "004_state_table",
    "005_auth_table"
];

#[derive(Debug, Clone)]
pub struct Db {
    pub(crate) pool: Pool<SqliteConnectionManager>,
}

impl Db {
    fn path() -> Result<PathBuf> {
        Ok(fig_data_dir()?.join("data.sqlite3"))
    }

    pub fn new() -> Result<Self> {
        // File-level only. Import uses `database()` after open so this cannot
        // re-enter the LazyLock.
        fig_util::directories::migrate_previous_product_data_dirs();
        Self::open(&Self::path()?)
    }

    fn open(path: &Path) -> Result<Self> {
        // make the parent dir if it doesnt exist
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let conn = SqliteConnectionManager::file(path).with_init(init_connection);
        // The default r2d2 checkout timeout is 30s. The completion engine's
        // supervisor thread reads this database between requests; if wedged
        // generator threads are sitting on connections, a 30s wait there
        // freezes every completion in the queue. Fail fast instead — callers
        // already treat database errors as best-effort.
        //
        // Default max_size is 15. This process is one writer plus a couple of
        // readers; fifteen idle SQLite connections are just page-cache.
        let pool = Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .connection_timeout(std::time::Duration::from_secs(3))
            .build(conn)?;

        // Check the unix permissions of the database file, set them to 0600 if they are not
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(path)?;
            let mut permissions = metadata.permissions();
            if permissions.mode() & 0o777 != 0o600 {
                permissions.set_mode(0o600);
                std::fs::set_permissions(path, permissions)?;
            }
        }

        Ok(Self { pool })
    }

    pub(crate) fn mock() -> Self {
        let conn = SqliteConnectionManager::memory();
        let pool = Pool::builder().build(conn).unwrap();
        Self { pool }
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.pool.get()?;
        let transaction = conn.transaction()?;

        let max_version = max_migration_version(&transaction);

        for (version, migration) in MIGRATIONS.iter().enumerate() {
            if has_migration(&transaction, version, max_version)? {
                continue;
            }

            // execute the migration
            transaction.execute_batch(migration.sql)?;

            info!(%version, name =% migration.name, "Applying migration");

            // insert the migration entry
            transaction.execute(
                "INSERT INTO migrations (version, migration_time) VALUES (?1, strftime('%s', 'now'));",
                params![version],
            )?;
        }

        // commit the transaction
        transaction.commit()?;

        Ok(())
    }

    fn get_value<T: FromSql>(&self, table: &'static str, key: impl AsRef<str>) -> Result<Option<T>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT value FROM {table} WHERE key = ?1"))?;
        match stmt.query_row([key.as_ref()], |row| row.get(0)) {
            Ok(data) => Ok(Some(data)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn get_state_value(&self, key: impl AsRef<str>) -> Result<Option<serde_json::Value>> {
        self.get_value(STATE_TABLE_NAME, key)
    }

    pub fn get_auth_value(&self, key: impl AsRef<str>) -> Result<Option<String>> {
        self.get_value(AUTH_TABLE_NAME, key)
    }

    fn set_value<T: ToSql>(&self, table: &'static str, key: impl AsRef<str>, value: T) -> Result<()> {
        self.pool.get()?.execute(
            &format!("INSERT OR REPLACE INTO {table} (key, value) VALUES (?1, ?2)"),
            params![key.as_ref(), value],
        )?;
        Ok(())
    }

    pub fn set_state_value(&self, key: impl AsRef<str>, value: impl Into<serde_json::Value>) -> Result<()> {
        self.set_value(STATE_TABLE_NAME, key, value.into())
    }

    pub fn set_auth_value(&self, key: impl AsRef<str>, value: impl Into<String>) -> Result<()> {
        self.set_value(AUTH_TABLE_NAME, key, value.into())
    }

    fn unset_value(&self, table: &'static str, key: impl AsRef<str>) -> Result<()> {
        self.pool
            .get()?
            .execute(&format!("DELETE FROM {table} WHERE key = ?1"), [key.as_ref()])?;
        Ok(())
    }

    pub fn unset_state_value(&self, key: impl AsRef<str>) -> Result<()> {
        self.unset_value(STATE_TABLE_NAME, key)
    }

    pub fn unset_auth_value(&self, key: impl AsRef<str>) -> Result<()> {
        self.unset_value(AUTH_TABLE_NAME, key)
    }

    fn is_value_set(&self, table: &'static str, key: impl AsRef<str>) -> Result<bool> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT value FROM {table} WHERE key = ?1"))?;
        match stmt.query_row([key.as_ref()], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(Error::QueryReturnedNoRows) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    pub fn is_state_value_set(&self, key: impl AsRef<str>) -> Result<bool> {
        self.is_value_set(STATE_TABLE_NAME, key)
    }

    pub fn is_auth_value_set(&self, key: impl AsRef<str>) -> Result<bool> {
        self.is_value_set(AUTH_TABLE_NAME, key)
    }

    fn all_values(&self, table: &'static str) -> Result<Map<String, serde_json::Value>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {table}"))?;
        let rows = stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let value: serde_json::Value = row.get(1)?;
            Ok((key, value))
        })?;

        let mut map = Map::new();
        for (key, value) in rows.flatten() {
            map.insert(key, value);
        }

        Ok(map)
    }

    pub fn all_state_values(&self) -> Result<Map<String, serde_json::Value>> {
        self.all_values(STATE_TABLE_NAME)
    }

    /// Copy state keys from `from` that `into` does not already have.
    /// Used when Fastab already created `data.sqlite3` (IME hash) before
    /// the Easy Complete database was merged. Identity keys are skipped —
    /// Easy Complete's Accessibility grant is not a Fastab grant.
    pub fn import_missing_state_values(from: &Self, into: &Self) -> Result<usize> {
        let mut imported = 0;
        for (key, value) in from.all_state_values()? {
            if is_previous_product_identity_key(&key) {
                continue;
            }
            if into.get_state_value(&key)?.is_none() {
                into.set_state_value(&key, value)?;
                imported += 1;
            }
        }
        Ok(imported)
    }

    pub fn import_missing_state_from_path(path: &Path) -> Result<usize> {
        import_missing_from_leftover_path(path).map(|(state, _history)| state)
    }

    /// Copy history rows when dest history is empty. File-level migrate
    /// skips dest `data.sqlite3`, so Easy Complete history would otherwise
    /// stay behind after IME created an empty Fastab database.
    pub fn import_history_if_dest_empty(from: &Self, into: &Self) -> Result<usize> {
        match history_row_count(into)? {
            Some(count) if count > 0 => return Ok(0),
            None => return Ok(0),
            Some(_) => {},
        }
        if !matches!(history_row_count(from)?, Some(count) if count > 0) {
            return Ok(0);
        }

        let from_conn = from.pool.get()?;
        let into_conn = into.pool.get()?;
        let mut stmt = from_conn.prepare(
            "SELECT command, shell, pid, session_id, cwd, start_time, end_time, duration, hostname, exit_code FROM history",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i32>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i32>>(9)?,
            ))
        })?;

        let mut imported = 0;
        for row in rows {
            let (command, shell, pid, session_id, cwd, start_time, end_time, duration, hostname, exit_code) = row?;
            into_conn.execute(
                "INSERT INTO history
                    (command, shell, pid, session_id, cwd, start_time, end_time, duration, hostname, exit_code)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    command, shell, pid, session_id, cwd, start_time, end_time, duration, hostname, exit_code
                ],
            )?;
            imported += 1;
        }
        Ok(imported)
    }

    // atomic style operations

    fn atomic_op<T: FromSql + ToSql>(
        &self,
        key: impl AsRef<str>,
        op: impl FnOnce(&Option<T>) -> Option<T>,
    ) -> Result<Option<T>> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let value = tx.query_row::<Option<T>, _, _>(
            &format!("SELECT value FROM {STATE_TABLE_NAME} WHERE key = ?1"),
            [key.as_ref()],
            |row| row.get(0),
        );

        let value_0: Option<T> = match value {
            Ok(value) => value,
            Err(Error::QueryReturnedNoRows) => None,
            Err(err) => return Err(err.into()),
        };

        let value_1 = op(&value_0);

        if let Some(value) = value_1 {
            tx.execute(
                &format!("INSERT OR REPLACE INTO {STATE_TABLE_NAME} (key, value) VALUES (?1, ?2)"),
                params![key.as_ref(), value],
            )?;
        } else {
            tx.execute(
                &format!("DELETE FROM {STATE_TABLE_NAME} WHERE key = ?1"),
                [key.as_ref()],
            )?;
        }

        tx.commit()?;

        Ok(value_0)
    }

    /// Atomically get the value of a key, then perform an or operation on it
    /// and set the new value. If the key does not exist, set it to the or value.
    pub fn atomic_bool_or(&self, key: impl AsRef<str>, or: bool) -> Result<bool> {
        self.atomic_op::<serde_json::Value>(key, |val| match val {
            // Some(val) => Some(serde_json::Value::Bool( || or)),
            Some(serde_json::Value::Bool(b)) => Some(serde_json::Value::Bool(*b || or)),
            Some(_) | None => Some(serde_json::Value::Bool(or)),
        })
        .map(|val| val.and_then(|val| val.as_bool()).unwrap_or(false))
    }
}

fn is_previous_product_identity_key(key: &str) -> bool {
    PREVIOUS_PRODUCT_IDENTITY_KEYS.contains(&key)
}

/// Copy leftover sqlite to a scratch file before open/migrate so we do not
/// mutate the Easy Complete database (and so WAL init does not drop
/// `-wal`/`-shm` next to it).
pub(crate) fn import_missing_from_leftover_path(path: &Path) -> Result<(usize, usize)> {
    if !path.is_file() {
        return Ok((0, 0));
    }
    let scratch = std::env::temp_dir().join(format!("fastab-import-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch)?;
    let copy = scratch.join("data.sqlite3");
    let result = (|| {
        copy_sqlite_bundle(path, &copy)?;
        let from = Db::open(&copy)?;
        from.migrate()?;
        let into = database()?;
        let state = Db::import_missing_state_values(&from, into)?;
        let history = Db::import_history_if_dest_empty(&from, into)?;
        Ok((state, history))
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn copy_sqlite_bundle(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dest)?;
    for suffix in ["-wal", "-shm"] {
        let mut src_extra = src.as_os_str().to_os_string();
        src_extra.push(suffix);
        let src_extra = PathBuf::from(src_extra);
        if src_extra.is_file() {
            let mut dest_extra = dest.as_os_str().to_os_string();
            dest_extra.push(suffix);
            std::fs::copy(&src_extra, dest_extra)?;
        }
    }
    Ok(())
}

fn forget_previous_product_identity_once(db: &Db) -> Result<()> {
    if db
        .get_state_value(CLEARED_PREVIOUS_PRODUCT_IDENTITY_KEY)?
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        return Ok(());
    }
    for key in PREVIOUS_PRODUCT_IDENTITY_KEYS {
        db.unset_state_value(key)?;
    }
    db.set_state_value(CLEARED_PREVIOUS_PRODUCT_IDENTITY_KEY, true)?;
    Ok(())
}

fn history_row_count(db: &Db) -> Result<Option<i64>> {
    let conn = db.pool.get()?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'history'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let count = conn.query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
    Ok(Some(count))
}

/// Applied to every pooled connection of the on-disk database.
///
/// figterm inserts history rows while the desktop reads them; without WAL a
/// writer blocks readers for the whole transaction, and without a busy
/// timeout a contended statement fails immediately with `SQLITE_BUSY`. WAL
/// lets the reader and writer proceed concurrently, and the busy timeout
/// bounds the residual contention instead of surfacing it as flaky errors.
fn init_connection(conn: &mut Connection) -> std::result::Result<(), Error> {
    conn.busy_timeout(std::time::Duration::from_secs(1))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn max_migration_version<C: Deref<Target = Connection>>(conn: &C) -> Option<i64> {
    let mut stmt = conn.prepare("SELECT MAX(version) FROM migrations").ok()?;
    stmt.query_row([], |row| row.get(0)).ok()
}

fn has_migration<C: Deref<Target = Connection>>(conn: &C, version: usize, max_version: Option<i64>) -> Result<bool> {
    // IMPORTANT: Due to a bug with the first 7 migrations, we have to check manually
    //
    // Background: the migrations table stores two identifying keys: the sqlite auto-generated
    // auto-incrementing key `id`, and the `version` which is the index of the `MIGRATIONS`
    // constant.
    //
    // Checking whether a migration exists would compare id with version, but since id is 1-indexed
    // and version is 0-indexed, we would actually skip the last migration! Therefore, it's
    // possible users are missing a critical migration (namely, auth_kv table creation) when
    // upgrading to the qchat build (which includes two new migrations). Hence, we have to check
    // all migrations until version 7 to make sure that nothing is missed.
    if version <= 7 {
        let mut stmt = match conn.prepare("SELECT COUNT(*) FROM migrations WHERE version = ?1") {
            Ok(stmt) => stmt,
            // If the migrations table does not exist, then we can reasonably say no migrations
            // will exist.
            Err(Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
                return Ok(false);
            },
            Err(err) => return Err(err.into()),
        };
        let count: i32 = stmt.query_row([version], |row| row.get(0))?;
        return Ok(count >= 1);
    }

    // Continuing from the previously implemented logic - any migrations after the 7th can have a simple
    // maximum version check, since we can reasonably assume if any version >=7 will have all
    // migrations prior to it.
    #[allow(clippy::match_like_matches_macro)]
    Ok(match max_version {
        Some(max_version) if max_version >= version as i64 => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock() -> Db {
        let db = Db::mock();
        db.migrate().unwrap();
        db
    }

    #[test]
    fn the_pool_does_not_keep_fifteen_idle_connections() {
        let tempdir = tempfile::tempdir().unwrap();
        let db = Db::open(&tempdir.path().join("data.sqlite3")).unwrap();
        assert_eq!(db.pool.max_size(), POOL_MAX_SIZE);
    }

    #[test]
    fn test_migrate() {
        let db = mock();

        // assert migration count is correct
        let max_migration = max_migration_version(&&*db.pool.get().unwrap());
        assert_eq!(max_migration, Some(MIGRATIONS.len() as i64 - 1));
    }

    #[test]
    fn list_migrations() {
        // Assert the migrations are in order
        assert!(MIGRATIONS.windows(2).all(|w| w[0].name <= w[1].name));

        // Assert the migrations start with their index
        assert!(
            MIGRATIONS
                .iter()
                .enumerate()
                .all(|(i, m)| m.name.starts_with(&format!("{:03}_", i)))
        );

        // Assert all the files in migrations/ are in the list
        let migration_folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sqlite/migrations");
        let migration_count = std::fs::read_dir(migration_folder).unwrap().count();
        assert_eq!(MIGRATIONS.len(), migration_count);
    }

    #[test]
    fn import_missing_state_copies_only_absent_keys() {
        let from = mock();
        let into = mock();
        from.set_state_value("keep-me", true).unwrap();
        from.set_state_value("already", "old").unwrap();
        into.set_state_value("already", "new").unwrap();

        let imported = Db::import_missing_state_values(&from, &into).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(into.get_state_value("keep-me").unwrap().unwrap(), true);
        assert_eq!(into.get_state_value("already").unwrap().unwrap(), "new");
    }

    #[test]
    fn import_missing_state_skips_previous_product_identity_keys() {
        let from = mock();
        let into = mock();
        from.set_state_value("desktop.accessibilityGranted", true).unwrap();
        from.set_state_value("input-method.launched-binary-sha256", "old-hash")
            .unwrap();
        from.set_state_value("desktop.loginItemMigratedToSMAppService", true)
            .unwrap();
        from.set_state_value("theme", "dark").unwrap();

        let imported = Db::import_missing_state_values(&from, &into).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(into.get_state_value("theme").unwrap().unwrap(), "dark");
        assert!(into.get_state_value("desktop.accessibilityGranted").unwrap().is_none());
        assert!(
            into.get_state_value("input-method.launched-binary-sha256")
                .unwrap()
                .is_none()
        );
        assert!(
            into.get_state_value("desktop.loginItemMigratedToSMAppService")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forget_previous_product_identity_runs_once() {
        let db = mock();
        db.set_state_value("desktop.accessibilityGranted", true).unwrap();
        db.set_state_value("input-method.launched-binary-sha256", "old-hash")
            .unwrap();
        db.set_state_value("keep", 1).unwrap();

        forget_previous_product_identity_once(&db).unwrap();
        assert!(db.get_state_value("desktop.accessibilityGranted").unwrap().is_none());
        assert!(
            db.get_state_value("input-method.launched-binary-sha256")
                .unwrap()
                .is_none()
        );
        assert_eq!(db.get_state_value("keep").unwrap().unwrap(), 1);
        assert_eq!(
            db.get_state_value("desktop.clearedPreviousProductIdentity")
                .unwrap()
                .unwrap(),
            true
        );

        db.set_state_value("input-method.launched-binary-sha256", "fastab-hash")
            .unwrap();
        forget_previous_product_identity_once(&db).unwrap();
        assert_eq!(
            db.get_state_value("input-method.launched-binary-sha256")
                .unwrap()
                .unwrap(),
            "fastab-hash"
        );
    }

    #[test]
    fn import_history_copies_only_when_dest_is_empty() {
        let from = mock();
        let into = mock();
        from.pool
            .get()
            .unwrap()
            .execute("INSERT INTO history (command, shell) VALUES ('ls', 'zsh')", [])
            .unwrap();

        let imported = Db::import_history_if_dest_empty(&from, &into).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(history_row_count(&into).unwrap(), Some(1));

        from.pool
            .get()
            .unwrap()
            .execute("INSERT INTO history (command) VALUES ('pwd')", [])
            .unwrap();
        assert_eq!(Db::import_history_if_dest_empty(&from, &into).unwrap(), 0);
        assert_eq!(history_row_count(&into).unwrap(), Some(1));
    }

    #[test]
    fn leftover_sqlite_copy_does_not_mutate_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data.sqlite3");
        {
            let conn = rusqlite::Connection::open(&src).unwrap();
            conn.execute_batch("CREATE TABLE t (k TEXT); INSERT INTO t VALUES ('a');")
                .unwrap();
        }
        let before = std::fs::read(&src).unwrap();

        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        copy_sqlite_bundle(&src, &scratch.join("data.sqlite3")).unwrap();
        let _copy = Db::open(&scratch.join("data.sqlite3")).unwrap();

        assert_eq!(std::fs::read(&src).unwrap(), before);
        let extras: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains("data.sqlite3"))
            .collect();
        assert_eq!(extras.len(), 1);
    }

    #[test]
    fn state_table_tests() {
        let db = mock();

        // set
        db.set_state_value("test", "test").unwrap();
        db.set_state_value("int", 1).unwrap();
        db.set_state_value("float", 1.0).unwrap();
        db.set_state_value("bool", true).unwrap();
        db.set_state_value("null", ()).unwrap();
        db.set_state_value("array", vec![1, 2, 3]).unwrap();
        db.set_state_value("object", serde_json::json!({ "test": "test" }))
            .unwrap();
        db.set_state_value("binary", b"test".to_vec()).unwrap();

        // get
        assert_eq!(db.get_state_value("test").unwrap().unwrap(), "test");
        assert_eq!(db.get_state_value("int").unwrap().unwrap(), 1);
        assert_eq!(db.get_state_value("float").unwrap().unwrap(), 1.0);
        assert_eq!(db.get_state_value("bool").unwrap().unwrap(), true);
        assert_eq!(db.get_state_value("null").unwrap().unwrap(), serde_json::Value::Null);
        assert_eq!(
            db.get_state_value("array").unwrap().unwrap(),
            serde_json::json!([1, 2, 3])
        );
        assert_eq!(
            db.get_state_value("object").unwrap().unwrap(),
            serde_json::json!({ "test": "test" })
        );
        assert_eq!(
            db.get_state_value("binary").unwrap().unwrap(),
            serde_json::json!(b"test".to_vec())
        );

        // unset
        db.unset_state_value("test").unwrap();
        db.unset_state_value("int").unwrap();

        // is_set
        assert!(!db.is_state_value_set("test").unwrap());
        assert!(!db.is_state_value_set("int").unwrap());
        assert!(db.is_state_value_set("float").unwrap());
        assert!(db.is_state_value_set("bool").unwrap());
    }

    #[test]
    fn auth_table_tests() {
        let db = mock();

        db.set_auth_value("test", "test").unwrap();
        assert_eq!(db.get_auth_value("test").unwrap().unwrap(), "test");
        assert!(db.is_auth_value_set("test").unwrap());
        db.unset_auth_value("test").unwrap();
        assert!(!db.is_auth_value_set("test").unwrap());

        assert_eq!(db.get_auth_value("test2").unwrap(), None);
        assert!(!db.is_auth_value_set("test2").unwrap());
    }

    #[test]
    fn db_open_time() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("data.sqlite3");

        // init the db
        let db = Db::open(&path).unwrap();
        db.migrate().unwrap();
        drop(db);

        let test_count = 100;

        let instant = std::time::Instant::now();
        let db = Db::open(&path).unwrap();
        for _ in 0..test_count {
            db.set_state_value("test", "test").unwrap();
            db.get_state_value("test").unwrap().unwrap();
        }
        let elapsed = instant.elapsed() / test_count;
        println!("time: {:?}", elapsed);
    }

    #[test]
    fn test_atomic_bool() {
        let key = "test";
        let db = mock();

        let cases = [
            (None, false, false, false),
            (None, true, false, true),
            (Some(false), false, false, false),
            (Some(false), true, false, true),
            (Some(true), false, true, true),
            (Some(true), true, true, true),
        ];

        for (a, b, c, d) in cases {
            db.set_state_value(key, a).unwrap();
            assert_eq!(db.atomic_bool_or(key, b).unwrap(), c);
            assert_eq!(db.get_state_value(key).unwrap().unwrap(), d);
            db.unset_state_value(key).unwrap();
        }
    }
}
