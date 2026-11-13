//! The per-repo history store: one SQLite file, by default
//! `.greenshift/history.sqlite`.
//!
//! The schema is versioned with `PRAGMA user_version` and migrated forward
//! in code when the store is opened. Each entry of `MIGRATIONS` takes the
//! schema from version `i` to `i + 1`; existing entries are never edited, only
//! appended to. Later inputs (diffs, coverage) arrive as new migrations that
//! add tables keyed by `runs.id` and `tests.id`.
//!
//! Schema version 1:
//!
//! - `runs`: one row per `greenshift ingest` invocation.
//! - `tests`: one row per test identity (`suite`, `name`), shared by every
//!   run, so the strings are stored once.
//! - `results`: one row per test per run: outcome and duration.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::junit::{Outcome, TestCase};

/// The default store location, relative to the working directory.
pub const DEFAULT_PATH: &str = ".greenshift/history.sqlite";

/// Schema migrations, in order. `MIGRATIONS[i]` upgrades version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    // 1: black-box history.
    r#"
    CREATE TABLE runs (
        id          INTEGER PRIMARY KEY,
        commit_sha  TEXT,
        source      TEXT NOT NULL,
        ingested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
        file_name   TEXT NOT NULL
    ) STRICT;

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
    #[error("could not create {path}: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("history store {path}: {source}")]
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
}

/// A run about to be recorded.
#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    /// The commit the tests ran against, if known.
    pub commit: Option<&'a str>,
    /// Which producer or CI job the results came from.
    pub source: &'a str,
    /// The JUnit file(s) the results came from, newline-separated.
    pub file_name: &'a str,
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
        let found: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sqlite)?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                path: path.clone(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        for (from, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            tx.execute_batch(sql).map_err(sqlite)?;
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
            "INSERT INTO runs (commit_sha, source, file_name) VALUES (?1, ?2, ?3)",
            params![run.commit, run.source, run.file_name],
        )
        .map_err(sqlite)?;
        let run_id = tx.last_insert_rowid();
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
