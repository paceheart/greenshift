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
//!
//! Schema version 2 adds `runs.partial`: 1 for a run known to hold only
//! part of the suite (`ingest --partial`), 0 otherwise. Runs written at
//! version 1 take the default, 0, as every run then was recorded as whole.
//! A version 1 store opened read-only is read as if migrated: its runs
//! are not partial.
//!
//! Schema version 3 adds `runs.branch` and `runs.tag`: the branch or the
//! tag the tests ran on, from `--branch`, `--tag` or the CI environment,
//! or NULL when none names one. A run has at most one of them. Runs
//! written before version 3 keep NULL in both: what they ran on is not
//! known.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};

use crate::junit::{Outcome, TestCase};
use crate::report_key::ReportKeySet;
use crate::tainted::Tainted;

pub use crate::report_key::{CommitId, CommitIdError};

mod history_builder;
mod history_data;

pub use history_builder::StoredRunBuilder;
pub use history_data::{History, StoredResult, StoredRun};

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
    // 2: runs marked as holding only part of the suite.
    r#"
    ALTER TABLE runs ADD COLUMN partial INTEGER NOT NULL DEFAULT 0
        CHECK (partial IN (0, 1));
    "#,
    // 3: the branch or the tag a run's tests ran on, when known.
    r#"
    ALTER TABLE runs ADD COLUMN branch TEXT;
    ALTER TABLE runs ADD COLUMN tag TEXT CHECK (branch IS NULL OR tag IS NULL);
    "#,
    // 4: identity independent of a SQLite file's local row ids, for snapshot merge.
    r#"
    ALTER TABLE runs ADD COLUMN history_id TEXT;
    CREATE UNIQUE INDEX runs_by_history_id ON runs(history_id);
    "#,
    // 5: globally unique report fingerprints, grouped by the first import.
    r#"
    CREATE TABLE report_memberships (
        run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        namespace TEXT NOT NULL CHECK (namespace = '' OR
            (length(namespace) BETWEEN 7 AND 64 AND namespace NOT GLOB '*[^0-9a-f]*')),
        fingerprint_version INTEGER NOT NULL CHECK (fingerprint_version = 1),
        digest TEXT NOT NULL CHECK (length(digest) = 64 AND digest NOT GLOB '*[^0-9a-f]*'),
        PRIMARY KEY (namespace, fingerprint_version, digest)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX report_memberships_by_run ON report_memberships(run_id);
    "#,
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Construct only trusted schema SQL, for exact-version snapshot admission.
/// This connection contains no caller data and never opens an input database.
pub(crate) fn canonical_schema(version: i64) -> Result<Connection, StoreError> {
    let path = Tainted::raw(":memory:");
    if !(1..=SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::BadVersion {
            path,
            found: version,
        });
    }
    let conn = Connection::open_in_memory().map_err(sqlite_err(&path))?;
    for sql in MIGRATIONS.iter().take(version as usize) {
        conn.execute_batch(sql).map_err(sqlite_err(&path))?;
    }
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .map_err(sqlite_err(&path))?;
    conn.pragma_update(None, "user_version", version)
        .map_err(sqlite_err(&path))?;
    Ok(conn)
}

// Two separately migrated copies of the same legacy DB must retain the same
// run identities. Include local id to preserve identical repeated legacy runs;
// content distinguishes unrelated runs whose local ids happen to coincide.
fn identify_legacy_runs(conn: &Connection) -> rusqlite::Result<()> {
    for (id, identity) in legacy_run_identities(conn)? {
        conn.execute(
            "UPDATE runs SET history_id=?1 WHERE id=?2",
            params![identity, id],
        )?;
    }
    Ok(())
}

// The v1 hash encoding, including SQL bytes and original local ids, is a durable
// format. Snapshot callers first admit the exact known schema and typed data.
pub(crate) fn legacy_run_identities(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    use sha2::{Digest, Sha256};
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let run_sql = match version {
        1 => "SELECT id,commit_sha,source,ingested_at,0,NULL,NULL FROM runs ORDER BY id",
        2 => "SELECT id,commit_sha,source,ingested_at,partial,NULL,NULL FROM runs ORDER BY id",
        _ => "SELECT id,commit_sha,source,ingested_at,partial,branch,tag FROM runs ORDER BY id",
    };
    let mut runs = conn.prepare(run_sql)?;
    let mut rows = runs.query([])?;
    let mut identities = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let mut digest = Sha256::new();
        digest.update(b"greenshift-legacy-run-v1\0");
        hash_row(&mut digest, row, 7)?;
        for (sql, columns) in [
            ("SELECT position,path FROM run_files WHERE run_id=?1 ORDER BY position", 2),
            ("SELECT t.suite,t.name,r.outcome,r.duration_ms FROM results r JOIN tests t ON t.id=r.test_id WHERE r.run_id=?1 ORDER BY t.suite,t.name", 4),
        ] {
            digest.update(sql.as_bytes());
            let mut statement = conn.prepare(sql)?;
            let mut child_rows = statement.query([id])?;
            while let Some(child_row) = child_rows.next()? {
                hash_row(&mut digest, child_row, columns)?;
            }
        }
        identities.push((id, format!("{:x}", digest.finalize())[..32].to_owned()));
    }
    Ok(identities)
}

fn hash_row(
    digest: &mut sha2::Sha256,
    row: &rusqlite::Row<'_>,
    columns: usize,
) -> rusqlite::Result<()> {
    use rusqlite::types::ValueRef;
    use sha2::Digest;
    digest.update(b"row\0");
    for column in 0..columns {
        match row.get_ref(column)? {
            ValueRef::Null => digest.update([0]),
            ValueRef::Integer(value) => {
                digest.update([1]);
                digest.update(value.to_le_bytes());
            }
            ValueRef::Real(value) => {
                digest.update([2]);
                digest.update(value.to_bits().to_le_bytes());
            }
            ValueRef::Text(value) | ValueRef::Blob(value) => {
                digest.update([if matches!(row.get_ref(column)?, ValueRef::Text(_)) {
                    3
                } else {
                    4
                }]);
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value);
            }
        }
    }
    Ok(())
}

/// Why the store could not be opened or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not create {path}")]
    CreateDir {
        path: Tainted,
        #[source]
        source: std::io::Error,
    },
    // SQLite's messages can quote text from the store file itself (a
    // malformed schema, say), so the whole cause chain is kept in
    // `message` as `Tainted`, escaped when displayed, rather than chained
    // as a source and printed raw.
    #[error("history store {path}: {message}")]
    Sqlite {
        path: Tainted,
        message: Tainted,
        sqlite: rusqlite::Error,
    },
    #[error(
        "history store {path} has schema version {found}, newer than this greenshift supports ({supported}); upgrade greenshift"
    )]
    TooNew {
        path: Tainted,
        found: i64,
        supported: i64,
    },
    #[error(
        "{path} is not a greenshift history store (SQLite application_id {found}, expected {expected}); refusing to use it"
    )]
    NotAStore {
        path: Tainted,
        found: i32,
        expected: i32,
    },
    #[error("history store {path} has schema version {found}, which no greenshift writes")]
    BadVersion { path: Tainted, found: i64 },
    #[error("history store {path} has an invalid durable run identity")]
    InvalidHistoryId { path: Tainted },
    #[error("history store {path}: report key namespace does not match the import commit")]
    ReportKeyNamespaceMismatch { path: Tainted },
    #[error("history store {path}: existing report membership has a different owner commit")]
    InvalidReportMembership { path: Tainted },
    #[error("history store {path}: report import partially overlaps existing reports; no history was recorded")]
    PartialReportOverlap { path: Tainted },
    #[error("history store {path}: repeated report group has different aggregate test results; no history was recorded")]
    ReportPayloadMismatch { path: Tainted },
    #[error(
        "\"{path}\" is SQLite's special name for a private in-memory database, not a file; pass a real path"
    )]
    MemoryPath { path: Tainted },
}

/// A run about to be recorded.
#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    /// The commit the tests ran against, if known.
    pub commit: Option<&'a CommitId>,
    /// Which producer or CI job the results came from.
    pub source: &'a str,
    /// The JUnit files the results came from, in the order given.
    pub files: &'a [PathBuf],
    /// The run holds only part of the suite (one of several passes, say),
    /// so it never counts as a full run, whatever its size.
    pub partial: bool,
    /// The branch the tests ran on, if known.
    pub branch: Option<&'a str>,
    /// The tag the tests ran on, if known; never with a branch.
    pub tag: Option<&'a str>,
}

/// What [`Store::record_run`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub run_id: i64,
    /// Rows written to `results`: distinct tests in the run.
    pub results: usize,
}

/// What an all-seen replay establishes about the submitted aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGrouping {
    /// Same complete retained group, with semantically equal test results.
    ExactGroup,
    /// All reports exist, but their individual contributions cannot be verified
    /// from aggregate-only storage. Existing grouping is retained.
    MembershipOnly,
}

/// An import records a new aggregate or acknowledges existing memberships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportAcknowledgement {
    Recorded(Recorded),
    AlreadyPresent {
        grouping: ReplayGrouping,
        /// Submitted source/partial/ref differs; first-observed metadata remains.
        metadata_retained: bool,
    },
}

/// An open, migrated history store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: Tainted,
    /// The schema version the store is at: [`SCHEMA_VERSION`], unless it
    /// was opened read-only at an older one.
    version: i64,
}

impl Store {
    /// Open (creating if needed) the store at `path`, creating its parent
    /// directory, and migrate it to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        refuse_memory_path(path)?;
        let shown = Tainted::from(path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::CreateDir {
                path: Tainted::from(dir),
                source,
            })?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn =
            Connection::open_with_flags(as_plain_file(path), flags).map_err(sqlite_err(&shown))?;
        Store::init(conn, shown)
    }

    /// Open the store at `path` for reading only: nothing is created,
    /// migrated or written. `None` when there is no history to read: the
    /// file does not exist, or is a SQLite file with no schema at all.
    /// Anything that is not a greenshift store is refused as by
    /// [`Store::open`].
    pub fn open_read_only(path: &Path) -> Result<Option<Store>, StoreError> {
        refuse_memory_path(path)?;
        // Only a file known to be missing is no history. When its existence
        // cannot be checked (a directory on the way is unreadable, say), the
        // open below fails and says why.
        if matches!(path.try_exists(), Ok(false)) {
            return Ok(None);
        }
        let shown = Tainted::from(path);
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn =
            Connection::open_with_flags(as_plain_file(path), flags).map_err(sqlite_err(&shown))?;
        let sqlite = sqlite_err(&shown);
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(&sqlite)?;
        let app_id: i32 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(&sqlite)?;
        if app_id != APPLICATION_ID {
            let objects: i64 = conn
                .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
                .map_err(&sqlite)?;
            if objects > 0 {
                return Err(StoreError::NotAStore {
                    path: shown.clone(),
                    found: app_id,
                    expected: APPLICATION_ID,
                });
            }
            return Ok(None);
        }
        let found: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(&sqlite)?;
        if found < 0 {
            return Err(StoreError::BadVersion {
                path: shown.clone(),
                found,
            });
        }
        if found > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                path: shown.clone(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        // Migration 1 stamps the application_id and the version in one
        // transaction, so a marked store at version 0 holds nothing. An
        // older version is read as it is; see [`Store::history`].
        if found == 0 {
            return Ok(None);
        }
        drop(sqlite);
        Ok(Some(Store {
            conn,
            path: shown,
            version: found,
        }))
    }

    /// A migrated store that lives only in memory.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory().map_err(sqlite_err(&Tainted::raw(":memory:")))?;
        Store::init(conn, Tainted::raw(":memory:"))
    }

    fn init(conn: Connection, path: Tainted) -> Result<Store, StoreError> {
        let mut store = Store {
            conn,
            path,
            version: SCHEMA_VERSION,
        };
        store
            .conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .and_then(|()| store.conn.pragma_update(None, "foreign_keys", true))
            .map_err(|e| store.sqlite(e))?;
        store.migrate()?;
        Ok(store)
    }

    fn sqlite(&self, source: rusqlite::Error) -> StoreError {
        sqlite_err(&self.path)(source)
    }

    /// The schema version of the open store.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| self.sqlite(e))
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let path = &self.path;
        let sqlite = sqlite_err(path);
        // IMMEDIATE takes the write lock before reading the version, so two
        // processes opening a fresh store cannot both apply a migration.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(&sqlite)?;
        let app_id: i32 = tx
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(&sqlite)?;
        let found: i64 = if app_id == APPLICATION_ID {
            tx.pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(&sqlite)?
        } else {
            let objects: i64 = tx
                .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
                .map_err(&sqlite)?;
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
            tx.execute_batch(sql).map_err(&sqlite)?;
            if from == 3 {
                identify_legacy_runs(&tx).map_err(&sqlite)?;
            }
            if from == 0 {
                // Stamp the store exactly once, from the one constant that
                // Store::open's own gate above compares against.
                tx.pragma_update(None, "application_id", APPLICATION_ID)
                    .map_err(&sqlite)?;
            }
            tx.pragma_update(None, "user_version", from as i64 + 1)
                .map_err(&sqlite)?;
        }
        let invalid_ids: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE history_id IS NULL OR typeof(history_id) != 'text' OR length(history_id) != 32 OR history_id GLOB '*[^0-9a-f]*')",
            [], |row| row.get(0),
        ).map_err(&sqlite)?;
        if invalid_ids {
            return Err(StoreError::InvalidHistoryId { path: path.clone() });
        }
        tx.commit().map_err(&sqlite)
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
        let path = &self.path;
        let sqlite = sqlite_err(path);
        let tx = self.conn.transaction().map_err(&sqlite)?;
        let recorded = insert_run(&tx, run, &merged).map_err(&sqlite)?;
        tx.commit().map_err(&sqlite)?;
        Ok(recorded)
    }

    /// Atomically classify report memberships and record a fresh aggregate.
    /// Partial overlap and exact-group result disagreement roll back without
    /// changing any history. All-seen subsets/spanning groups acknowledge only
    /// membership, never per-report result equivalence or coverage upgrades.
    pub fn record_import<'c>(
        &mut self,
        run: &NewRun<'_>,
        keys: &ReportKeySet,
        cases: impl IntoIterator<Item = &'c TestCase>,
    ) -> Result<ImportAcknowledgement, StoreError> {
        if keys.namespace() != run.commit.map_or("", CommitId::as_str) {
            return Err(StoreError::ReportKeyNamespaceMismatch {
                path: self.path.clone(),
            });
        }
        let merged = merge_duplicates(cases);
        let path = &self.path;
        let sqlite = sqlite_err(path);
        // This writer lock covers classification as well as insertion across
        // independent connections; the unique key is the durable final guard.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(&sqlite)?;
        let mut owners: BTreeMap<i64, usize> = BTreeMap::new();
        {
            let mut find = tx.prepare("SELECT run_id FROM report_memberships WHERE namespace=?1 AND fingerprint_version=?2 AND digest=?3")
                .map_err(&sqlite)?;
            for key in keys.keys() {
                let owner: Option<i64> = find
                    .query_row(
                        params![key.namespace(), key.version(), key.digest()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(&sqlite)?;
                if let Some(owner) = owner {
                    *owners.entry(owner).or_default() += 1;
                }
            }
        }
        let seen: usize = owners.values().sum();
        if seen != 0 && seen != keys.keys().len() {
            return Err(StoreError::PartialReportOverlap { path: path.clone() });
        }
        if seen == 0 {
            let recorded = insert_run(&tx, run, &merged).map_err(&sqlite)?;
            {
                let mut add = tx.prepare("INSERT INTO report_memberships (run_id,namespace,fingerprint_version,digest) VALUES (?1,?2,?3,?4)")
                    .map_err(&sqlite)?;
                for key in keys.keys() {
                    add.execute(params![
                        recorded.run_id,
                        key.namespace(),
                        key.version(),
                        key.digest()
                    ])
                    .map_err(&sqlite)?;
                }
            }
            tx.commit().map_err(&sqlite)?;
            return Ok(ImportAcknowledgement::Recorded(recorded));
        }
        let mut grouping = ReplayGrouping::MembershipOnly;
        let mut metadata_retained = false;
        for (&owner, &selected) in &owners {
            let owner_commit: Option<String> = tx
                .query_row("SELECT commit_sha FROM runs WHERE id=?1", [owner], |row| {
                    row.get(0)
                })
                .map_err(&sqlite)?;
            if owner_commit.as_deref() != run.commit.map(CommitId::as_str) {
                return Err(StoreError::InvalidReportMembership { path: path.clone() });
            }
            let total: i64 = tx
                .query_row(
                    "SELECT count(*) FROM report_memberships WHERE run_id=?1",
                    [owner],
                    |row| row.get(0),
                )
                .map_err(&sqlite)?;
            if owners.len() == 1 && usize::try_from(total).ok() == Some(selected) {
                if !same_aggregate(&tx, owner, &merged).map_err(&sqlite)? {
                    return Err(StoreError::ReportPayloadMismatch { path: path.clone() });
                }
                grouping = ReplayGrouping::ExactGroup;
            }
            let metadata_equal: bool = tx.query_row(
                "SELECT source=?1 AND partial=?2 AND branch IS ?3 AND tag IS ?4 FROM runs WHERE id=?5",
                params![run.source, run.partial, run.branch, run.tag, owner], |row| row.get(0),
            ).map_err(&sqlite)?;
            metadata_retained |= !metadata_equal;
        }
        tx.commit().map_err(&sqlite)?;
        Ok(ImportAcknowledgement::AlreadyPresent {
            grouping,
            metadata_retained,
        })
    }

    /// Every run from `sources` (from every source when `sources` is
    /// empty) with its results, read in one transaction so the runs and
    /// results agree.
    pub fn history(&self, sources: &[String]) -> Result<History, StoreError> {
        let sqlite = sqlite_err(&self.path);
        let tx = self.conn.unchecked_transaction().map_err(&sqlite)?;
        let filter = if sources.is_empty() {
            String::new()
        } else {
            format!(
                "WHERE u.source IN ({})",
                vec!["?"; sources.len()].join(", ")
            )
        };
        // A version 1 store, opened read-only, has no `partial` column:
        // its runs were all recorded as whole.
        let partial = if self.version >= 2 { "u.partial" } else { "0" };
        let mut runs: Vec<StoredRunBuilder> = tx
            .prepare(&format!(
                "SELECT u.id, u.source, {partial} FROM runs u {filter} ORDER BY u.id"
            ))
            .and_then(|mut stmt| {
                stmt.query_map(params_from_iter(sources), |r| {
                    Ok(StoredRunBuilder {
                        id: r.get(0)?,
                        source: r.get(1)?,
                        partial: r.get(2)?,
                        results: Vec::new(),
                    })
                })?
                .collect()
            })
            .map_err(&sqlite)?;
        let results: Vec<(i64, StoredResult)> = tx
            .prepare(&format!(
                "SELECT r.run_id, r.test_id, r.outcome, r.duration_ms
                 FROM results r JOIN runs u ON u.id = r.run_id {filter}
                 ORDER BY r.run_id, r.test_id"
            ))
            .and_then(|mut stmt| {
                stmt.query_map(params_from_iter(sources), |r| {
                    let outcome: String = r.get(2)?;
                    // Raw, not `{:?}`: the message is kept as `Tainted`,
                    // which escapes it when shown.
                    let outcome = stored_outcome(&outcome).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            format!("unknown outcome \"{outcome}\"").into(),
                        )
                    })?;
                    Ok((r.get(0)?, StoredResult::new(r.get(1)?, outcome, r.get(3)?)))
                })?
                .collect()
            })
            .map_err(&sqlite)?;
        let tests = tx
            .prepare("SELECT id, suite, name FROM tests")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| {
                    let suite: String = r.get(1)?;
                    let name: String = r.get(2)?;
                    Ok((r.get(0)?, (Tainted::raw(suite), Tainted::raw(name))))
                })?
                .collect()
            })
            .map_err(&sqlite)?;
        tx.finish().map_err(&sqlite)?;
        // Both lists are in run id order, so one pass files each result
        // under its run.
        let mut next = runs.iter_mut().peekable();
        for (run_id, result) in results {
            while next.peek().is_some_and(|run| run.id < run_id) {
                next.next();
            }
            if let Some(run) = next.peek_mut().filter(|run| run.id == run_id) {
                run.results.push(result);
            }
        }
        Ok(History::new(
            runs.into_iter().map(StoredRunBuilder::build).collect(),
            tests,
        ))
    }
}

/// Insert only within the caller's transaction, preserving random execution
/// identities and the same aggregate semantics for both recording paths.
fn insert_run(
    tx: &rusqlite::Transaction<'_>,
    run: &NewRun<'_>,
    merged: &[TestCase],
) -> rusqlite::Result<Recorded> {
    tx.execute(
        "INSERT INTO runs (commit_sha, source, partial, branch, tag, history_id)
         VALUES (?1, ?2, ?3, ?4, ?5, lower(hex(randomblob(16))))",
        params![
            run.commit.map(CommitId::as_str),
            run.source,
            run.partial,
            run.branch,
            run.tag
        ],
    )?;
    let run_id = tx.last_insert_rowid();
    {
        let mut add_file =
            tx.prepare("INSERT INTO run_files (run_id, position, path) VALUES (?1, ?2, ?3)")?;
        for (position, file) in run.files.iter().enumerate() {
            add_file.execute(params![run_id, position as i64, path_bytes(file)])?;
        }
    }
    {
        let mut find_test = tx.prepare("SELECT id FROM tests WHERE suite = ?1 AND name = ?2")?;
        let mut add_test = tx.prepare("INSERT INTO tests (suite, name) VALUES (?1, ?2)")?;
        let mut add_result = tx.prepare(
            "INSERT INTO results (run_id, test_id, outcome, duration_ms) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for c in merged {
            let existing: Option<i64> = find_test
                .query_row(params![c.suite().as_raw(), c.name().as_raw()], |row| {
                    row.get(0)
                })
                .optional()?;
            let test_id = match existing {
                Some(id) => id,
                None => add_test.insert(params![c.suite().as_raw(), c.name().as_raw()])?,
            };
            add_result.execute(params![
                run_id,
                test_id,
                c.outcome().as_str(),
                c.duration_ms()
            ])?;
        }
    }
    Ok(Recorded {
        run_id,
        results: merged.len(),
    })
}

fn same_aggregate(
    tx: &rusqlite::Transaction<'_>,
    run_id: i64,
    merged: &[TestCase],
) -> rusqlite::Result<bool> {
    let mut expected: Vec<_> = merged
        .iter()
        .map(|c| {
            (
                c.suite().as_raw(),
                c.name().as_raw(),
                c.outcome().as_str(),
                c.duration_ms(),
            )
        })
        .collect();
    expected.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut statement = tx.prepare("SELECT t.suite,t.name,r.outcome,r.duration_ms FROM results r JOIN tests t ON t.id=r.test_id WHERE r.run_id=?1 ORDER BY t.suite,t.name")?;
    let mut rows = statement.query([run_id])?;
    for (suite, name, outcome, duration) in expected {
        let Some(row) = rows.next()? else {
            return Ok(false);
        };
        if row.get::<_, String>(0)? != suite
            || row.get::<_, String>(1)? != name
            || row.get::<_, String>(2)? != outcome
            || row.get::<_, Option<i64>>(3)? != duration
        {
            return Ok(false);
        }
    }
    Ok(rows.next()?.is_none())
}

/// SQLite's in-memory names would open a database that vanishes on close.
fn refuse_memory_path(path: &Path) -> Result<(), StoreError> {
    // The empty name is SQLite's private temporary database, deleted
    // when the connection closes, so nothing would persist.
    if path == Path::new(":memory:") || path.as_os_str().is_empty() {
        return Err(StoreError::MemoryPath {
            path: Tainted::from(path),
        });
    }
    Ok(())
}

/// The name to hand SQLite so that it opens `path` as a plain file.
///
/// The bundled SQLite is built with SQLITE_USE_URI, so a name starting
/// with `file:` is parsed as a URI whatever the open flags say, and
/// `file::memory:` or `?mode=memory` would silently open an in-memory
/// database. Anchoring such a path at `./` makes SQLite read it as the
/// plain file name it is. Other paths are passed as given, so SQLite's
/// own error text names the path the user typed.
pub(crate) fn as_plain_file(path: &Path) -> std::borrow::Cow<'_, Path> {
    if path.as_os_str().as_encoded_bytes().starts_with(b"file:") {
        std::borrow::Cow::Owned(Path::new(".").join(path))
    } else {
        std::borrow::Cow::Borrowed(path)
    }
}

/// The outcome stored as `text`, if it is one.
pub(crate) fn stored_outcome(text: &str) -> Option<Outcome> {
    [
        Outcome::Passed,
        Outcome::Failed,
        Outcome::Error,
        Outcome::Skipped,
    ]
    .into_iter()
    .find(|o| o.as_str() == text)
}

/// Wraps a SQLite error with the store path it happened on.
///
/// The message is the error and its chain of sources, joined by ": ". A
/// source whose text already ends the message is left out: some errors,
/// such as rusqlite's conversion failures, show their source themselves.
fn sqlite_err(path: &Tainted) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |sqlite| {
        let mut chain = sqlite.to_string();
        let mut cause = std::error::Error::source(&sqlite);
        while let Some(c) = cause {
            let text = c.to_string();
            if !chain.ends_with(&text) {
                chain.push_str(": ");
                chain.push_str(&text);
            }
            cause = c.source();
        }
        StoreError::Sqlite {
            path: path.clone(),
            message: Tainted::raw(chain),
            sqlite,
        }
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
    struct Accumulator<'a> {
        first: &'a TestCase,
        outcome: Outcome,
        duration_ms: Option<i64>,
    }
    let mut merged: Vec<Accumulator<'c>> = Vec::new();
    let mut index: HashMap<(&Tainted, &Tainted), usize> = HashMap::new();
    for c in cases {
        match index.get(&(c.suite(), c.name())) {
            Some(&i) => {
                let m = &mut merged[i];
                if severity(c.outcome()) > severity(m.outcome) {
                    m.outcome = c.outcome();
                }
                m.duration_ms = match (m.duration_ms, c.duration_ms()) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    (a, b) => a.or(b),
                };
            }
            None => {
                index.insert((c.suite(), c.name()), merged.len());
                merged.push(Accumulator {
                    first: c,
                    outcome: c.outcome(),
                    duration_ms: c.duration_ms(),
                });
            }
        }
    }
    merged
        .into_iter()
        .map(|m| {
            TestCase::new(
                m.first.suite().clone(),
                m.first.name().clone(),
                m.outcome,
                m.duration_ms,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::report_key::ReportKey;

    const RUN: NewRun<'static> = NewRun {
        commit: None,
        source: "producer",
        files: &[],
        partial: false,
        branch: None,
        tag: None,
    };

    fn keys(bytes: &[&[u8]]) -> ReportKeySet {
        ReportKeySet::new(bytes.iter().map(|bytes| ReportKey::from_bytes(None, bytes))).unwrap()
    }

    fn case(name: &str, outcome: Outcome, duration: Option<i64>) -> TestCase {
        TestCase::new(Tainted::raw("suite"), Tainted::raw(name), outcome, duration)
    }

    fn counts(store: &Store) -> Vec<i64> {
        [
            "runs",
            "run_files",
            "tests",
            "results",
            "report_memberships",
        ]
        .into_iter()
        .map(|table| {
            store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        })
        .collect()
    }

    #[test]
    fn test_record_import_exact_retry_preserves_first_provenance_and_semantic_aggregate() {
        let mut store = Store::open_in_memory().unwrap();
        let files = [PathBuf::from("first.xml")];
        let run = NewRun {
            partial: true,
            branch: Some("topic"),
            files: &files,
            ..RUN
        };
        let original = [
            case("alpha", Outcome::Passed, Some(3)),
            case("alpha", Outcome::Failed, Some(4)),
            case("beta", Outcome::Skipped, None),
        ];
        let group = keys(&[b"a", b"b"]);
        let ImportAcknowledgement::Recorded(recorded) =
            store.record_import(&run, &group, &original).unwrap()
        else {
            panic!("fresh import must record")
        };
        let identity: (String, String) = store
            .conn
            .query_row("SELECT history_id,ingested_at FROM runs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        let before = counts(&store);
        let changed_files = [PathBuf::from("renamed.xml")];
        let replay = NewRun {
            source: "other",
            partial: false,
            tag: Some("v1"),
            files: &changed_files,
            ..RUN
        };
        let equivalent = [
            case("beta", Outcome::Skipped, None),
            case("alpha", Outcome::Failed, Some(7)),
        ];
        assert_eq!(
            store.record_import(&replay, &group, &equivalent).unwrap(),
            ImportAcknowledgement::AlreadyPresent {
                grouping: ReplayGrouping::ExactGroup,
                metadata_retained: true
            }
        );
        assert_eq!(counts(&store), before);
        assert_eq!(
            store
                .conn
                .query_row("SELECT history_id,ingested_at FROM runs", [], |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?
                )))
                .unwrap(),
            identity
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT source,partial,branch,tag FROM runs", [], |row| Ok(
                    (
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?
                    )
                ))
                .unwrap(),
            ("producer".into(), true, Some("topic".into()), None)
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT path FROM run_files WHERE run_id=?1",
                    [recorded.run_id],
                    |row| row.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
            b"first.xml"
        );
        assert_eq!(
            store.record_import(&run, &group, &original).unwrap(),
            ImportAcknowledgement::AlreadyPresent {
                grouping: ReplayGrouping::ExactGroup,
                metadata_retained: false
            }
        );
    }

    #[test]
    fn test_record_import_exact_group_checks_each_semantic_component_without_mutation() {
        let mut store = Store::open_in_memory().unwrap();
        let group = keys(&[b"a"]);
        let original = [case("alpha", Outcome::Passed, Some(3))];
        store.record_import(&RUN, &group, &original).unwrap();
        let before = counts(&store);
        let changes = [
            vec![case("alpha", Outcome::Failed, Some(3))],
            vec![case("alpha", Outcome::Passed, Some(4))],
            vec![case("renamed", Outcome::Passed, Some(3))],
            vec![TestCase::new(
                Tainted::raw("other"),
                Tainted::raw("alpha"),
                Outcome::Passed,
                Some(3),
            )],
            vec![case("alpha", Outcome::Passed, None)],
            vec![],
            vec![
                case("alpha", Outcome::Passed, Some(3)),
                case("extra", Outcome::Passed, None),
            ],
        ];
        for changed in changes {
            assert!(matches!(
                store.record_import(&RUN, &group, &changed),
                Err(StoreError::ReportPayloadMismatch { .. })
            ));
            assert_eq!(counts(&store), before);
        }
    }

    #[test]
    fn test_record_import_partial_overlap_is_atomic_and_subsets_spanning_are_membership_only() {
        let mut store = Store::open_in_memory().unwrap();
        let a = keys(&[b"a"]);
        let ab = keys(&[b"a", b"b"]);
        store.record_import(&RUN, &a, &[]).unwrap();
        let before = counts(&store);
        assert!(matches!(
            store.record_import(&RUN, &ab, &[case("new", Outcome::Failed, None)]),
            Err(StoreError::PartialReportOverlap { .. })
        ));
        assert_eq!(counts(&store), before);
        store.record_import(&RUN, &keys(&[b"b"]), &[]).unwrap();
        assert_eq!(
            store
                .record_import(&RUN, &ab, &[case("unverifiable", Outcome::Failed, None)])
                .unwrap(),
            ImportAcknowledgement::AlreadyPresent {
                grouping: ReplayGrouping::MembershipOnly,
                metadata_retained: false
            }
        );
        assert_eq!(counts(&store), vec![2, 0, 0, 0, 2]);

        let mut fresh = Store::open_in_memory().unwrap();
        fresh.record_import(&RUN, &ab, &[]).unwrap();
        assert_eq!(
            fresh
                .record_import(&RUN, &a, &[case("unverifiable", Outcome::Failed, None)])
                .unwrap(),
            ImportAcknowledgement::AlreadyPresent {
                grouping: ReplayGrouping::MembershipOnly,
                metadata_retained: false
            }
        );
        assert_eq!(counts(&fresh), vec![1, 0, 0, 0, 2]);
    }

    #[test]
    fn test_record_import_namespace_and_failed_insert_leave_all_tables_unchanged() {
        let mut store = Store::open_in_memory().unwrap();
        let commit: CommitId = "0123ABC".parse().unwrap();
        let run = NewRun {
            commit: Some(&commit),
            ..RUN
        };
        assert!(matches!(
            store.record_import(&run, &keys(&[b"a"]), &[]),
            Err(StoreError::ReportKeyNamespaceMismatch { .. })
        ));
        assert_eq!(counts(&store), vec![0; 5]);
        let group = ReportKeySet::new([ReportKey::from_bytes(Some(&commit), b"a")]).unwrap();
        assert!(matches!(
            store.record_import(&run, &group, &[case("invalid", Outcome::Passed, Some(-1))]),
            Err(StoreError::Sqlite { .. })
        ));
        assert_eq!(counts(&store), vec![0; 5]);
        store.record_import(&run, &group, &[]).unwrap();
        store.record_import(&RUN, &keys(&[b"a"]), &[]).unwrap();
        assert_eq!(counts(&store), vec![2, 0, 0, 0, 2]);
    }

    #[test]
    fn test_record_run_remains_unkeyed_with_distinct_random_identities() {
        let mut store = Store::open_in_memory().unwrap();
        let cases = [case("same", Outcome::Passed, Some(1))];
        store.record_run(&RUN, &cases).unwrap();
        store.record_run(&RUN, &cases).unwrap();
        assert_eq!(counts(&store), vec![2, 0, 1, 2, 0]);
        let identities: i64 = store
            .conn
            .query_row("SELECT count(DISTINCT history_id) FROM runs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(identities, 2);
        store
            .record_import(&RUN, &keys(&[b"same"]), &cases)
            .unwrap();
        assert_eq!(counts(&store), vec![3, 0, 1, 3, 1]);
    }

    #[test]
    fn test_record_import_rejects_existing_membership_with_mismatched_owner_commit() {
        for stored_commit in [Some("0123456"), Some(""), Some("legacy arbitrary text")] {
            let mut store = Store::open_in_memory().unwrap();
            let group = keys(&[b"a"]);
            store.record_import(&RUN, &group, &[]).unwrap();
            store
                .conn
                .execute("UPDATE runs SET commit_sha=?1", [stored_commit])
                .unwrap();
            let before = counts(&store);
            assert!(matches!(
                store.record_import(&RUN, &group, &[]),
                Err(StoreError::InvalidReportMembership { .. })
            ));
            assert_eq!(counts(&store), before);
        }
        // Arbitrary commits on genuinely unkeyed legacy/execution data stay valid.
        let mut store = Store::open_in_memory().unwrap();
        store.record_run(&RUN, &[]).unwrap();
        store
            .conn
            .execute("UPDATE runs SET commit_sha='legacy arbitrary text'", [])
            .unwrap();
        store.record_import(&RUN, &keys(&[b"a"]), &[]).unwrap();
        assert_eq!(counts(&store), vec![2, 0, 0, 0, 1]);
    }

    #[test]
    fn test_record_import_independent_connections_serialize_same_disjoint_and_partial_groups() {
        use std::sync::{Arc, Barrier};
        for (left, right, expected_runs, expected_keys, conflict) in [
            (vec![b"a".as_slice()], vec![b"a".as_slice()], 1, 1, false),
            (vec![b"a".as_slice()], vec![b"b".as_slice()], 2, 2, false),
            (
                vec![b"a".as_slice(), b"b".as_slice()],
                vec![b"b".as_slice(), b"c".as_slice()],
                1,
                2,
                true,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("history.sqlite");
            let first = Store::open(&path).unwrap();
            let second = Store::open(&path).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = [(first, keys(&left)), (second, keys(&right))]
                .into_iter()
                .map(|(mut store, keys)| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        store.record_import(&RUN, &keys, &[])
                    })
                })
                .collect();
            let outcomes: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, Ok(ImportAcknowledgement::Recorded(_))))
                    .count(),
                expected_runs
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(
                        outcome,
                        Err(StoreError::PartialReportOverlap { .. })
                    ))
                    .count(),
                usize::from(conflict)
            );
            assert!(outcomes.iter().all(|outcome| outcome.is_ok()
                || matches!(outcome, Err(StoreError::PartialReportOverlap { .. }))));
            let store = Store::open(&path).unwrap();
            assert_eq!(
                counts(&store),
                vec![expected_runs as i64, 0, 0, 0, expected_keys]
            );
        }
    }

    #[test]
    fn test_report_membership_sql_guards_uniqueness_foreign_keys_and_canonical_values() {
        let mut store = Store::open_in_memory().unwrap();
        store.record_run(&RUN, &[]).unwrap();
        store.record_run(&RUN, &[]).unwrap();
        let digest = "a".repeat(64);
        store
            .conn
            .execute(
                "INSERT INTO report_memberships VALUES (1,'',1,?1)",
                [&digest],
            )
            .unwrap();
        for (owner, namespace, version, digest) in [
            (1, "", 1, digest.clone()),
            (2, "", 1, digest.clone()),
            (999, "", 1, "b".repeat(64)),
            (1, "ABCDEF0", 1, "b".repeat(64)),
            (1, "", 2, "b".repeat(64)),
            (1, "", 1, "A".repeat(64)),
            (1, "", 1, "b".repeat(63)),
        ] {
            assert!(store
                .conn
                .execute(
                    "INSERT INTO report_memberships VALUES (?1,?2,?3,?4)",
                    params![owner, namespace, version, digest]
                )
                .is_err());
        }
        store.conn.execute("DELETE FROM runs", []).unwrap();
        assert_eq!(counts(&store), vec![0; 5]);
    }
}
