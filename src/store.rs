//! The per-repo history store: one SQLite file, by default
//! `.greenshift/history.sqlite`.
//!
//! The schema is versioned with `PRAGMA user_version` and migrated forward
//! in code when the store is opened. Each entry of `MIGRATIONS` takes the
//! schema from version `i` to `i + 1`; existing entries are never edited, only
//! appended to. Later inputs (diffs, coverage) arrive as new migrations that
//! add tables keyed by `runs.id` and `tests.id`.
//!
//! A greenshift store is marked with `PRAGMA application_id` =
//! [`APPLICATION_ID`], set by migration 1. Opening a SQLite file whose
//! application_id is anything else (including 0, the SQLite default) is
//! refused if the file already holds any schema object, so greenshift never
//! writes into another program's database. A file with no schema at all
//! (new, or empty) is taken over and migrated from version 0, whatever its
//! `user_version`.
//!
//! Schema version 1:
//!
//! - `runs`: one row per `greenshift ingest` invocation.
//! - `run_files`: the JUnit files a run came from, one row each, numbered
//!   from 0 in the order given. `path` is the path exactly as given, as raw
//!   bytes: on Unix the `OsStr` bytes, so non-UTF-8 names and names with
//!   newlines survive unchanged; elsewhere `OsStr::as_encoded_bytes`
//!   (WTF-8 on Windows), which round-trips on the same platform.
//! - `tests`: one row per test identity (`suite`, `name`), shared by every
//!   run, so the strings are stored once.
//! - `results`: one row per test per run: outcome and duration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::junit::{Outcome, TestCase};

/// The default store location, relative to the working directory.
pub const DEFAULT_PATH: &str = ".greenshift/history.sqlite";

/// `PRAGMA application_id` of every greenshift store: "GRNS" in ASCII.
pub const APPLICATION_ID: i32 = 0x4752_4E53;

/// Schema migrations, in order. `MIGRATIONS[i]` upgrades version `i` to `i + 1`.
///
/// Migration 1 also stamps [`APPLICATION_ID`] via `pragma_update` in
/// [`Store::migrate`] rather than a literal in this SQL, so the value has
/// one source of truth instead of two copies that could drift apart.
const MIGRATIONS: &[&str] = &[
    // 1: black-box history.
    r#"
    CREATE TABLE runs (
        id          INTEGER PRIMARY KEY,
        commit_sha  TEXT,
        source      TEXT NOT NULL,
        ingested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
    ) STRICT;

    CREATE TABLE run_files (
        run_id   INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        position INTEGER NOT NULL CHECK (position >= 0),
        path     BLOB NOT NULL,
        PRIMARY KEY (run_id, position)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE tests (
        id    INTEGER PRIMARY KEY,
        suite TEXT NOT NULL,
        name  TEXT NOT NULL,
        UNIQUE (suite, name)
    ) STRICT;

    CREATE TABLE results (
        run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        test_id     INTEGER NOT NULL REFERENCES tests(id),
        outcome     TEXT NOT NULL
                    CHECK (outcome IN ('passed', 'failed', 'error', 'skipped')),
        duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
        PRIMARY KEY (run_id, test_id)
    ) STRICT, WITHOUT ROWID;

    CREATE INDEX results_by_test ON results (test_id);
    "#,
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Why the store could not be opened or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not create {path}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("history store {path}")]
    Sqlite {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "history store {path} has schema version {found}, newer than this greenshift supports ({supported}); upgrade greenshift"
    )]
    TooNew {
        path: String,
        found: i64,
        supported: i64,
    },
    #[error(
        "{path} is not a greenshift history store (SQLite application_id {found}, expected {expected}); refusing to write to it"
    )]
    NotAStore {
        path: String,
        found: i32,
        expected: i32,
    },
    #[error("history store {path} has schema version {found}, which no greenshift writes")]
    BadVersion { path: String, found: i64 },
    #[error(
        "{path} is SQLite's special name for a private in-memory database, not a file; pass a real path"
    )]
    MemoryPath { path: String },
}

/// A run about to be recorded.
#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    /// The commit the tests ran against, if known.
    pub commit: Option<&'a str>,
    /// Which producer or CI job the results came from.
    pub source: &'a str,
    /// The JUnit files the results came from, in the order given.
    pub files: &'a [PathBuf],
}

/// What [`Store::record_run`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub run_id: i64,
    /// Rows written to `results`: distinct tests in the run.
    pub results: usize,
}

/// An open, migrated history store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: String,
}

impl Store {
    /// Open (creating if needed) the store at `path`, creating its parent
    /// directory, and migrate it to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if path == Path::new(":memory:") {
            return Err(StoreError::MemoryPath {
                path: ":memory:".into(),
            });
        }
        let shown = path.display().to_string();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::CreateDir {
                path: dir.display().to_string(),
                source,
            })?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn =
            Connection::open_with_flags(path, flags).map_err(|source| StoreError::Sqlite {
                path: shown.clone(),
                source,
            })?;
        Store::init(conn, shown)
    }

    /// A migrated store that lives only in memory.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory().map_err(|source| StoreError::Sqlite {
            path: ":memory:".into(),
            source,
        })?;
        Store::init(conn, ":memory:".into())
    }

    fn init(conn: Connection, path: String) -> Result<Store, StoreError> {
        let mut store = Store { conn, path };
        store
            .conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .and_then(|()| store.conn.pragma_update(None, "foreign_keys", true))
            .map_err(|e| store.sqlite(e))?;
        store.migrate()?;
        Ok(store)
    }

    fn sqlite(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Sqlite {
            path: self.path.clone(),
            source,
        }
    }

    /// The schema version of the open store.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| self.sqlite(e))
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let path = self.path.clone();
        let sqlite = |source| StoreError::Sqlite {
            path: path.clone(),
            source,
        };
        // IMMEDIATE takes the write lock before reading the version, so two
        // processes opening a fresh store cannot both apply a migration.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sqlite)?;
        let app_id: i32 = tx
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(sqlite)?;
        let found: i64 = if app_id == APPLICATION_ID {
            tx.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(sqlite)?
        } else {
            let objects: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
                .map_err(sqlite)?;
            if objects > 0 {
                return Err(StoreError::NotAStore {
                    path: path.clone(),
                    found: app_id,
                    expected: APPLICATION_ID,
                });
            }
            // Taken over: whatever user_version it has is not a greenshift
            // schema version, so migrate from the start.
            0
        };
        if found < 0 {
            return Err(StoreError::BadVersion {
                path: path.clone(),
                found,
            });
        }
        if found > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                path: path.clone(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        for (from, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            tx.execute_batch(sql).map_err(sqlite)?;
            if from == 0 {
                // Stamp the store exactly once, from the one constant that
                // Store::open's own gate above compares against.
                tx.pragma_update(None, "application_id", APPLICATION_ID)
                    .map_err(sqlite)?;
            }
            tx.pragma_update(None, "user_version", from as i64 + 1)
                .map_err(sqlite)?;
        }
        tx.commit().map_err(sqlite)
    }

    /// Record one run and all of its results in a single transaction: either
    /// everything is written or nothing is.
    ///
    /// A run holds one result per test. If the same test (suite and name)
    /// appears more than once, its results are combined: the most severe
    /// outcome wins (failed, then error, then passed, then skipped) and the
    /// durations are summed (unknown if none is known).
    pub fn record_run<'c>(
        &mut self,
        run: &NewRun<'_>,
        cases: impl IntoIterator<Item = &'c TestCase>,
    ) -> Result<Recorded, StoreError> {
        let merged = merge_duplicates(cases);
        let path = self.path.clone();
        let sqlite = |source| StoreError::Sqlite {
            path: path.clone(),
            source,
        };
        let tx = self.conn.transaction().map_err(sqlite)?;
        tx.execute(
            "INSERT INTO runs (commit_sha, source) VALUES (?1, ?2)",
            params![run.commit, run.source],
        )
        .map_err(sqlite)?;
        let run_id = tx.last_insert_rowid();
        {
            let mut add_file = tx
                .prepare("INSERT INTO run_files (run_id, position, path) VALUES (?1, ?2, ?3)")
                .map_err(sqlite)?;
            for (position, file) in run.files.iter().enumerate() {
                add_file
                    .execute(params![run_id, position as i64, path_bytes(file)])
                    .map_err(sqlite)?;
            }
        }
        {
            let mut find_test = tx
                .prepare("SELECT id FROM tests WHERE suite = ?1 AND name = ?2")
                .map_err(sqlite)?;
            let mut add_test = tx
                .prepare("INSERT INTO tests (suite, name) VALUES (?1, ?2)")
                .map_err(sqlite)?;
            let mut add_result = tx
                .prepare(
                    "INSERT INTO results (run_id, test_id, outcome, duration_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(sqlite)?;
            for c in &merged {
                let existing: Option<i64> = find_test
                    .query_row(params![c.suite, c.name], |r| r.get(0))
                    .optional()
                    .map_err(sqlite)?;
                let test_id = match existing {
                    Some(id) => id,
                    None => add_test.insert(params![c.suite, c.name]).map_err(sqlite)?,
                };
                add_result
                    .execute(params![run_id, test_id, c.outcome.as_str(), c.duration_ms])
                    .map_err(sqlite)?;
            }
        }
        tx.commit().map_err(sqlite)?;
        Ok(Recorded {
            run_id,
            results: merged.len(),
        })
    }
}

/// The bytes stored in `run_files.path`; see the module docs.
#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

/// The bytes stored in `run_files.path`; see the module docs.
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// How much an outcome says about the test; the higher one wins when a test
/// appears twice in a run.
fn severity(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Skipped => 0,
        Outcome::Passed => 1,
        Outcome::Error => 2,
        Outcome::Failed => 3,
    }
}

/// Combine repeated test identities, keeping first-appearance order.
fn merge_duplicates<'c>(cases: impl IntoIterator<Item = &'c TestCase>) -> Vec<TestCase> {
    let mut merged: Vec<TestCase> = Vec::new();
    let mut index: HashMap<(&str, &str), usize> = HashMap::new();
    for c in cases {
        match index.get(&(c.suite.as_str(), c.name.as_str())) {
            Some(&i) => {
                let m = &mut merged[i];
                if severity(c.outcome) > severity(m.outcome) {
                    m.outcome = c.outcome;
                }
                m.duration_ms = match (m.duration_ms, c.duration_ms) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    (a, b) => a.or(b),
                };
            }
            None => {
                index.insert((c.suite.as_str(), c.name.as_str()), merged.len());
                merged.push(c.clone());
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests;
