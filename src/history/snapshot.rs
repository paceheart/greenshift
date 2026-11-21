//! Data-only SQLite snapshots. Input SQL is never executed as SQL.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rusqlite::{config::DbConfig, params, Connection, OpenFlags};

use super::{GroupOverlap, MergeReport};
#[cfg(test)]
use crate::junit::Outcome;
use crate::report_key::ReportKey;
use crate::store::{self, Store, APPLICATION_ID, SCHEMA_VERSION};
use crate::tainted::Tainted;

mod dictionary;
mod frozen;
mod text;

use dictionary::{DictionaryBuilder, TestDictionary, TestId, TestKey};
use frozen::{
    FrozenBytes, FrozenFiles, FrozenReports, FrozenResults, FrozenRuns, ResultRow, Run, RunMetadata,
};
use text::FrozenText;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("history operation cancelled")]
    Cancelled,
    #[error("snapshot I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot SQLite error: {0}")]
    Sqlite(Tainted),
    #[error("invalid history snapshot: {0}")]
    Invalid(&'static str),
    #[error("could not create canonical history snapshot: {0}")]
    Store(#[from] crate::store::StoreError),
}

impl From<rusqlite::Error> for SnapshotError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(Tainted::raw(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunMetadataBuilder {
    history_id: FrozenText,
    commit: Option<FrozenText>,
    source: FrozenText,
    at: FrozenText,
    partial: i64,
    branch: Option<FrozenText>,
    tag: Option<FrozenText>,
}

impl RunMetadataBuilder {
    fn from_frozen(metadata: &RunMetadata) -> Self {
        Self {
            history_id: metadata.history_id().clone(),
            commit: metadata.commit().cloned(),
            source: metadata.source().clone(),
            at: metadata.at().clone(),
            partial: metadata.partial(),
            branch: metadata.branch().cloned(),
            tag: metadata.tag().cloned(),
        }
    }

    fn freeze(self) -> RunMetadata {
        RunMetadata::new(
            self.history_id,
            self.commit,
            self.source,
            self.at,
            self.partial,
            self.branch,
            self.tag,
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RunBuilder {
    metadata: RunMetadataBuilder,
    files: Vec<(i64, FrozenBytes)>,
    results: Vec<ResultRow>,
    reports: Vec<ReportKey>,
}

impl RunBuilder {
    fn freeze(self) -> Run {
        Run::new(
            self.metadata.freeze(),
            FrozenFiles::from_vec(self.files),
            FrozenResults::from_vec(self.results),
            FrozenReports::from_vec(self.reports),
        )
    }
}

fn freeze_runs(builders: impl ExactSizeIterator<Item = RunBuilder>) -> FrozenRuns {
    // Reserve the completed run vector once; it never grows beside the live
    // builder map/vector. Box shrinking happens only after that input is gone.
    let mut runs = Vec::with_capacity(builders.len());
    runs.extend(builders.map(RunBuilder::freeze));
    FrozenRuns::from_vec(runs)
}

struct Data {
    tests: TestDictionary,
    runs: FrozenRuns,
}

fn invalid(why: &'static str) -> SnapshotError {
    SnapshotError::Invalid(why)
}

type SchemaObject = (String, String, String, Option<String>);

fn schema(conn: &Connection) -> Result<Vec<SchemaObject>, SnapshotError> {
    let mut stmt =
        conn.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn check_sidecars(path: &Path) -> Result<(), SnapshotError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        match std::fs::symlink_metadata(Path::new(&sidecar)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
            Ok(_) => {
                return Err(invalid(
                    "snapshot must be standalone: SQLite sidecar exists",
                ));
            }
        }
    }
    Ok(())
}

fn check_output(path: &Path) -> Result<(), SnapshotError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => {
            return Err(invalid(
                "snapshot destination must be a regular non-symlink file",
            ));
        }
    }
    // Never remove sidecars: they may belong to a live database. Replacing only
    // its main inode would leave another reader observing the previous WAL.
    check_sidecars(path)
}

fn open_checked(path: &Path) -> Result<(Connection, i64), SnapshotError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(invalid("expected a regular SQLite file"));
    }
    check_sidecars(path)?;
    let conn = Connection::open_with_flags(
        store::as_plain_file(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    // One read transaction covers schema validation and every subsequent query.
    conn.execute_batch("BEGIN")?;
    let version = checked_connection_version(&conn)?;
    Ok((conn, version))
}

fn checked_connection_version(conn: &Connection) -> Result<i64, SnapshotError> {
    let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app != APPLICATION_ID {
        return Err(invalid("application id mismatch"));
    }
    if !(1..=SCHEMA_VERSION).contains(&version) {
        return Err(invalid("unsupported snapshot schema version"));
    }
    let expected = store::canonical_schema(version)?;
    if schema(conn)? != schema(&expected)? {
        return Err(invalid(
            "schema does not exactly match this greenshift release",
        ));
    }
    let check: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(invalid("SQLite integrity check failed"));
    }
    if conn
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        return Err(invalid("broken foreign key"));
    }
    Ok(version)
}

fn borrowed_text<'row>(
    row: &'row rusqlite::Row<'_>,
    column: usize,
) -> Result<&'row str, SnapshotError> {
    row.get_ref(column)?
        .as_str()
        .map_err(|_| invalid("expected UTF-8 text field"))
}

fn frozen_text(row: &rusqlite::Row<'_>, column: usize) -> Result<FrozenText, SnapshotError> {
    let text = borrowed_text(row, column)?;
    Ok(FrozenText::copy_from(text))
}

fn optional_text(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> Result<Option<FrozenText>, SnapshotError> {
    if matches!(row.get_ref(column)?, rusqlite::types::ValueRef::Null) {
        Ok(None)
    } else {
        frozen_text(row, column).map(Some)
    }
}

fn load(path: &Path) -> Result<Data, SnapshotError> {
    let (conn, version) = open_checked(path)?;
    load_connection(&conn, version)
}

fn load_connection(conn: &Connection, version: i64) -> Result<Data, SnapshotError> {
    // Normalize repeated test keys rather than expanding their text per result.
    // There is intentionally no arbitrary history size/allocation limit. Inputs
    // remain capable of exhausting memory, disk or CPU; see the design notes.
    let mut test_ids = HashMap::new();
    let mut dictionary = DictionaryBuilder::default();
    let mut stmt = conn.prepare("SELECT id,suite,name FROM tests ORDER BY id")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let suite = borrowed_text(row, 1)?;
        let name = borrowed_text(row, 2)?;
        if id <= 0 {
            return Err(invalid("test id must be positive"));
        }
        let local = dictionary.insert(TestKey::new(
            FrozenText::copy_from(suite),
            FrozenText::copy_from(name),
        ));
        test_ids.insert(id, local);
    }
    let tests = dictionary.freeze();
    let mut runs = BTreeMap::new();
    let run_sql = match version {
        1 => "SELECT id,commit_sha,source,ingested_at,0,NULL,NULL,NULL FROM runs ORDER BY id",
        2 => "SELECT id,commit_sha,source,ingested_at,partial,NULL,NULL,NULL FROM runs ORDER BY id",
        3 => {
            "SELECT id,commit_sha,source,ingested_at,partial,branch,tag,NULL FROM runs ORDER BY id"
        }
        _ => {
            "SELECT id,commit_sha,source,ingested_at,partial,branch,tag,history_id FROM runs ORDER BY id"
        }
    };
    let mut stmt = conn.prepare(run_sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let history_id = match optional_text(row, 7)? {
            Some(identity) => identity,
            None => FrozenText::copy_from(""),
        };
        let metadata = RunMetadataBuilder {
            history_id,
            commit: optional_text(row, 1)?,
            source: frozen_text(row, 2)?,
            at: frozen_text(row, 3)?,
            partial: row.get(4)?,
            branch: optional_text(row, 5)?,
            tag: optional_text(row, 6)?,
        };
        if id <= 0
            || ![0, 1].contains(&metadata.partial)
            || (metadata.branch.is_some() && metadata.tag.is_some())
        {
            return Err(invalid("invalid run id, partial flag, or branch/tag"));
        }
        if version >= 4
            && (metadata.history_id.len() != 32
                || !metadata
                    .history_id
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        {
            return Err(invalid("run history id must be 32 lowercase hex digits"));
        }
        runs.insert(
            id,
            RunBuilder {
                metadata,
                files: Vec::new(),
                results: Vec::new(),
                reports: Vec::new(),
            },
        );
    }
    let mut stmt =
        conn.prepare("SELECT run_id,position,path FROM run_files ORDER BY run_id,position")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let position: i64 = row.get(1)?;
        let path = row
            .get_ref(2)?
            .as_blob()
            .map_err(|_| invalid("expected binary run file path"))?;
        if position < 0 {
            return Err(invalid("invalid run file position"));
        }
        let run = runs
            .get_mut(&id)
            .ok_or_else(|| invalid("unknown run for file"))?;
        if run.files.capacity() == 0 {
            run.files.reserve_exact(1);
        }
        run.files.push((position, FrozenBytes::copy_from(path)));
    }
    let mut stmt = conn.prepare(
        "SELECT run_id,test_id,outcome,duration_ms FROM results ORDER BY run_id,test_id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let run_id: i64 = row.get(0)?;
        let test_id: i64 = row.get(1)?;
        let outcome = borrowed_text(row, 2)?;
        let duration: Option<i64> = row.get(3)?;
        let parsed_outcome =
            store::stored_outcome(outcome).ok_or_else(|| invalid("invalid outcome or duration"))?;
        if duration.is_some_and(|d| d < 0) {
            return Err(invalid("invalid outcome or duration"));
        }
        let test = *test_ids
            .get(&test_id)
            .ok_or_else(|| invalid("unknown test"))?;
        let run = runs
            .get_mut(&run_id)
            .ok_or_else(|| invalid("unknown run for result"))?;
        if run.results.capacity() == 0 {
            run.results.reserve_exact(1);
        }
        run.results
            .push(ResultRow::new(test, parsed_outcome, duration));
    }
    if version >= 5 {
        let mut seen = BTreeSet::new();
        let mut stmt = conn.prepare("SELECT run_id,namespace,fingerprint_version,digest FROM report_memberships ORDER BY run_id,namespace,fingerprint_version,digest")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let namespace = borrowed_text(row, 1)?;
            let report = ReportKey::new(namespace, row.get(2)?, borrowed_text(row, 3)?)
                .map_err(|_| invalid("invalid report membership key"))?;
            let run = runs
                .get_mut(&id)
                .ok_or_else(|| invalid("unknown run for report membership"))?;
            let namespace_matches = match run.metadata.commit.as_ref() {
                None => namespace.is_empty(),
                Some(commit) => !namespace.is_empty() && commit.as_str() == namespace,
            };
            if !namespace_matches {
                return Err(invalid(
                    "report membership namespace disagrees with run commit",
                ));
            }
            if !seen.insert(report.clone()) {
                return Err(invalid("duplicate report membership"));
            }
            run.reports.push(report);
        }
    }
    if version < 4 {
        for (id, identity) in store::legacy_run_identities(conn)? {
            runs.get_mut(&id)
                .ok_or_else(|| invalid("legacy identity has no run"))?
                .metadata
                .history_id = FrozenText::new(identity);
        }
    }
    for run in runs.values_mut() {
        run.results
            .sort_by(|a, b| tests.get(a.test()).cmp(&tests.get(b.test())));
    }
    Ok(Data {
        tests,
        runs: freeze_runs(runs.into_values()),
    })
}

/// Reject any snapshot that is not canonical greenshift data.
pub fn validate(path: &Path) -> Result<(), SnapshotError> {
    load(path).map(|_| ())
}

fn write(data: &Data, output: &Path) -> Result<(), SnapshotError> {
    check_output(output)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    drop(Store::open(temporary.path())?);
    let mut conn = Connection::open(store::as_plain_file(temporary.path()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let tx = conn.transaction()?;
    insert_data(&tx, data)?;
    tx.commit()?;
    drop(conn);
    validate(temporary.path())?;
    temporary.as_file().sync_all()?;
    check_output(output)?;
    temporary
        .persist(output)
        .map_err(|error| SnapshotError::Io(error.error))?;
    Ok(())
}

fn insert_data(conn: &Connection, data: &Data) -> Result<(), SnapshotError> {
    for key in data.tests.iter() {
        conn.execute(
            "INSERT INTO tests(suite,name) VALUES (?1,?2)",
            params![key.suite().as_str(), key.name().as_str()],
        )?;
    }
    let mut chronological: Vec<_> = data.runs.iter().collect();
    chronological.sort_unstable_by(|a, b| {
        (a.metadata().at(), a.metadata().history_id())
            .cmp(&(b.metadata().at(), b.metadata().history_id()))
    });
    for run in chronological {
        let metadata = run.metadata();
        conn.execute("INSERT INTO runs(commit_sha,source,ingested_at,partial,branch,tag,history_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",params![metadata.commit().map(FrozenText::as_str),metadata.source().as_str(),metadata.at().as_str(),metadata.partial(),metadata.branch().map(FrozenText::as_str),metadata.tag().map(FrozenText::as_str),metadata.history_id().as_str()])?;
        let id = conn.last_insert_rowid();
        for report in run.reports().as_slice() {
            conn.execute("INSERT INTO report_memberships(run_id,namespace,fingerprint_version,digest) VALUES (?1,?2,?3,?4)", params![id,report.namespace(),report.version(),report.digest()])?;
        }
        for (position, path) in run.files().iter() {
            conn.execute(
                "INSERT INTO run_files(run_id,position,path) VALUES (?1,?2,?3)",
                params![id, position, path.as_bytes()],
            )?;
        }
        for result in run.results().iter() {
            data.tests
                .get(result.test())
                .ok_or_else(|| invalid("result test missing from snapshot"))?;
            let test = i64::try_from(result.test().index() + 1)
                .map_err(|_| invalid("test id exceeds SQLite range"))?;
            conn.execute(
                "INSERT INTO results(run_id,test_id,outcome,duration_ms) VALUES (?1,?2,?3,?4)",
                params![id, test, result.outcome().as_str(), result.duration()],
            )?;
        }
    }
    Ok(())
}

/// Export one consistent read transaction into a standalone canonical database.
/// Existing output is replaced atomically only after complete validation.
pub fn snapshot(db: &Path, output: &Path) -> Result<(), SnapshotError> {
    write(&load(db)?, output)
}

fn remap_runs(
    runs: &FrozenRuns,
    mapping: &[TestId],
    dictionary: &TestDictionary,
) -> Result<Vec<RunBuilder>, SnapshotError> {
    let mut remapped = Vec::with_capacity(runs.len());
    for run in runs.iter() {
        let mut results = Vec::with_capacity(run.results().len());
        for result in run.results().iter() {
            let test = *mapping
                .get(result.test().index())
                .ok_or_else(|| invalid("result test missing from snapshot"))?;
            results.push(ResultRow::new(test, result.outcome(), result.duration()));
        }
        results.sort_by(|a, b| dictionary.get(a.test()).cmp(&dictionary.get(b.test())));
        let mut files = Vec::with_capacity(run.files().len());
        files.extend(run.files().iter().cloned());
        remapped.push(RunBuilder {
            metadata: RunMetadataBuilder::from_frozen(run.metadata()),
            files,
            results,
            reports: run.reports().as_slice().to_vec(),
        });
    }
    Ok(remapped)
}

/// Merge destination-first without treating SQLite's local row ids as identity.
/// Genuine independent executions remain distinct. Keyed imports deduplicate by
/// membership; entire partially overlapping incoming groups are excluded and
/// reported. Conflicting contents for any shared retained identity are rejected.
pub fn merge(current: &Path, incoming: &Path, output: &Path) -> Result<MergeReport, SnapshotError> {
    let data = load(current)?;
    let other = load(incoming)?;
    let (data, report) = union(data, other)?;
    write(&data, output)?;
    Ok(report)
}

fn union(data: Data, other: Data) -> Result<(Data, MergeReport), SnapshotError> {
    let mut keys = BTreeSet::new();
    for key in data.tests.iter().chain(other.tests.iter()) {
        if !keys.contains(key) {
            keys.insert(key.clone());
        }
    }
    let mut builder = DictionaryBuilder::default();
    for key in keys {
        builder.insert(key);
    }
    let remap = |dictionary: &TestDictionary| {
        dictionary
            .iter()
            .map(|key| {
                builder
                    .id_for(key)
                    .ok_or_else(|| invalid("test missing from merged dictionary"))
            })
            .collect::<Result<Vec<_>, SnapshotError>>()
    };
    let first = remap(&data.tests)?;
    let second = remap(&other.tests)?;
    let mut merged = builder.freeze();
    let mut runs = remap_runs(&data.runs, &first, &merged)?;
    let incoming = remap_runs(&other.runs, &second, &merged)?;
    drop(data);
    drop(other);
    drop(second);
    let mut replay_only_tests = BTreeSet::new();
    {
        let known: HashMap<_, _> = runs
            .iter()
            .map(|run| (run.metadata.history_id.as_str(), run))
            .collect();
        // Full equality for every retained shared UUID precedes any key skip.
        for run in &incoming {
            match known.get(run.metadata.history_id.as_str()) {
                Some(existing) if **existing != *run => {
                    return Err(invalid("one run identity has conflicting contents"));
                }
                Some(_) => (),
                None => (),
            }
        }
    }
    let retained_ids: BTreeSet<_> = runs
        .iter()
        .map(|run| run.metadata.history_id.clone())
        .collect();
    // This is the reverse-direction limitation, not an attempt to split or
    // reconcile groups. In restore, these local groups cannot be fully merged
    // back into the fetched source under its existing membership ownership.
    let retained_groups = {
        let source_keys: BTreeSet<_> = incoming.iter().flat_map(|run| run.reports.iter()).collect();
        runs.iter()
            .filter_map(|run| {
                let seen = run
                    .reports
                    .iter()
                    .filter(|key| source_keys.contains(*key))
                    .count();
                (seen != 0 && seen != run.reports.len()).then(|| {
                    GroupOverlap::from_validated_id(
                        run.metadata.history_id.as_str(),
                        run.reports.len() - seen,
                    )
                })
            })
            .collect()
    };
    let mut excluded_groups = Vec::new();
    let mut nonexact_replays = 0;
    let mut metadata_replays = 0;
    let mut owners: BTreeMap<ReportKey, usize> = BTreeMap::new();
    for (index, run) in runs.iter().enumerate() {
        for key in &run.reports {
            owners.insert(key.clone(), index);
        }
    }
    for run in incoming {
        if retained_ids.contains(&run.metadata.history_id) {
            continue;
        }
        if !run.reports.is_empty() {
            let seen = run
                .reports
                .iter()
                .filter(|key| owners.contains_key(*key))
                .count();
            if seen != 0 && seen != run.reports.len() {
                excluded_groups.push(GroupOverlap::from_validated_id(
                    run.metadata.history_id.as_str(),
                    run.reports.len() - seen,
                ));
                replay_only_tests.extend(run.results.iter().map(|result| result.test().index()));
                continue;
            }
            if seen == run.reports.len() {
                let first = owners[&run.reports[0]];
                let exact = runs[first].reports == run.reports;
                if exact && runs[first].results != run.results {
                    return Err(invalid("same report group has conflicting results"));
                }
                let metadata_changed = run.reports.iter().any(|key| {
                    let metadata = &runs[owners[key]].metadata;
                    (
                        metadata.source.as_str(),
                        metadata.partial,
                        &metadata.branch,
                        &metadata.tag,
                    ) != (
                        run.metadata.source.as_str(),
                        run.metadata.partial,
                        &run.metadata.branch,
                        &run.metadata.tag,
                    )
                });
                if metadata_changed {
                    metadata_replays += 1;
                }
                if !exact {
                    nonexact_replays += 1;
                }
                // Non-exact all-seen groups prove membership only, not aggregate
                // equivalence. Destination grouping and all metadata stay intact.
                replay_only_tests.extend(run.results.iter().map(|result| result.test().index()));
                continue;
            }
        }
        let index = runs.len();
        for key in &run.reports {
            owners.insert(key.clone(), index);
        }
        runs.push(run);
    }
    // Preserve every destination key, including historical unused entries. Also
    // preserve keys needed by admitted observations and unrelated input metadata.
    // Only names contributed exclusively by ignored replay results are omitted.
    for test in &first {
        replay_only_tests.remove(&test.index());
    }
    drop(first);
    for run in &runs {
        for result in &run.results {
            replay_only_tests.remove(&result.test().index());
        }
    }
    if !replay_only_tests.is_empty() {
        let mut builder = DictionaryBuilder::default();
        let mapping: Vec<_> = merged
            .iter()
            .enumerate()
            .map(|(index, key)| {
                if replay_only_tests.contains(&index) {
                    None
                } else {
                    Some(builder.insert(key.clone()))
                }
            })
            .collect();
        for run in &mut runs {
            for result in &mut run.results {
                let test = mapping
                    .get(result.test().index())
                    .copied()
                    .flatten()
                    .ok_or_else(|| invalid("retained result missing from merged dictionary"))?;
                *result = ResultRow::new(test, result.outcome(), result.duration());
            }
        }
        merged = builder.freeze();
    }
    Ok((
        Data {
            tests: merged,
            runs: freeze_runs(runs.into_iter()),
        },
        MergeReport::new(
            excluded_groups,
            retained_groups,
            nonexact_replays,
            metadata_replays,
        ),
    ))
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::report_key::ReportKeySet;
    use crate::store::NewRun;

    fn record(path: &Path, reports: &[&str]) {
        let mut store = Store::open(path).unwrap();
        let mut cases = Vec::new();
        for report in reports {
            cases.extend(crate::junit::parse_str(report).unwrap().into_cases());
        }
        let keys = ReportKeySet::new(
            reports
                .iter()
                .map(|report| ReportKey::from_bytes(None, report.as_bytes())),
        )
        .unwrap();
        store
            .record_import(
                &NewRun {
                    commit: None,
                    source: "fixture",
                    files: &[],
                    partial: false,
                    branch: None,
                    tag: None,
                },
                &keys,
                &cases,
            )
            .unwrap();
    }

    const A: &str = r#"<testsuite name="gears"><testcase name="turns" time="0.01"/></testsuite>"#;
    const B: &str = r#"<testsuite name="gears"><testcase name="holds" time="0.02"/></testsuite>"#;

    fn identities(path: &Path) -> Vec<String> {
        Connection::open(path)
            .unwrap()
            .prepare("SELECT history_id FROM runs ORDER BY history_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn dictionary_rows(path: &Path) -> Vec<(String, String)> {
        Connection::open(path)
            .unwrap()
            .prepare("SELECT suite,name FROM tests ORDER BY suite,name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn test_import_snapshot_ignored_replay_does_not_retain_incoming_only_names() {
        for spanning in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let destination = dir.path().join("destination");
            let incoming = dir.path().join("incoming");
            if spanning {
                record(&destination, &[A]);
                record(&destination, &[B]);
                record(&incoming, &[A, B]);
            } else {
                record(&destination, &[A, B]);
                record(&incoming, &[A]);
            }
            Connection::open(&destination)
                .unwrap()
                .execute(
                    "INSERT INTO tests(suite,name) VALUES ('old-unused','keep-destination')",
                    [],
                )
                .unwrap();
            let mut expected = dictionary_rows(&destination);
            let conn = Connection::open(&incoming).unwrap();
            conn.execute(
                "UPDATE tests SET name='ignored-replay-label' WHERE name='turns'",
                [],
            )
            .unwrap();
            // Unrelated existing dictionary metadata is not a historical cleanup
            // target. Only payload contributed exclusively by ignored runs goes.
            conn.execute(
                "INSERT INTO tests(suite,name) VALUES ('orphan','keep-unrelated-input')",
                [],
            )
            .unwrap();
            drop(conn);
            expected.push(("orphan".to_owned(), "keep-unrelated-input".to_owned()));
            expected.sort();
            let before_source = std::fs::read(&incoming).unwrap();
            let before_ids = identities(&destination);
            let output = dir.path().join("merged");
            merge(&destination, &incoming, &output).unwrap();
            assert_eq!(dictionary_rows(&output), expected);
            assert_eq!(identities(&output), before_ids);
            let restored = dir.path().join("restored");
            std::fs::copy(&destination, &restored).unwrap();
            restore_existing(&restored, &incoming, || false).unwrap();
            assert_eq!(dictionary_rows(&restored), expected);
            assert_eq!(identities(&restored), before_ids);
            assert_eq!(std::fs::read(&incoming).unwrap(), before_source);
        }
    }

    #[test]
    fn test_import_snapshot_replay_name_shared_with_admitted_run_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        let incoming = dir.path().join("incoming");
        record(&destination, &[A, B]);
        record(&incoming, &[A]);
        Connection::open(&incoming)
            .unwrap()
            .execute("UPDATE tests SET name='admitted-label'", [])
            .unwrap();
        let report = crate::junit::parse_str(
            r#"<testsuite name="gears"><testcase name="admitted-label"/></testsuite>"#,
        )
        .unwrap();
        let mut store = Store::open(&incoming).unwrap();
        store
            .record_run(
                &NewRun {
                    commit: None,
                    source: "admitted",
                    files: &[],
                    partial: false,
                    branch: None,
                    tag: None,
                },
                report.cases(),
            )
            .unwrap();
        drop(store);
        let output = dir.path().join("output");
        merge(&destination, &incoming, &output).unwrap();
        assert!(
            dictionary_rows(&output).contains(&("gears".to_owned(), "admitted-label".to_owned()))
        );
        assert_eq!(identities(&output).len(), 2);
    }

    #[test]
    fn test_import_snapshot_independent_exact_groups_dedupe_and_keep_destination() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        record(&first, &[A, B]);
        record(&second, &[B, A]);
        let first_ids = identities(&first);
        let second_ids = identities(&second);
        assert_ne!(first_ids, second_ids);
        for (destination, incoming, expected, name) in [
            (&first, &second, &first_ids, "merged-first"),
            (&second, &first, &second_ids, "merged-second"),
        ] {
            let output = dir.path().join(name);
            merge(destination, incoming, &output).unwrap();
            assert_eq!(&identities(&output), expected);
            let data = load(&output).unwrap();
            assert_eq!(
                data.runs.iter().next().unwrap().reports().as_slice().len(),
                2
            );
        }
    }

    #[test]
    fn test_import_snapshot_partial_overlap_excludes_whole_group_and_reports_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let combined = dir.path().join("combined");
        let output = dir.path().join("output");
        record(&first, &[A]);
        record(&combined, &[A, B]);
        let before = std::fs::read(&first).unwrap();
        let incoming_before = std::fs::read(&combined).unwrap();
        let first_ids = identities(&first);
        let combined_ids = identities(&combined);
        let report = merge(&first, &combined, &output).unwrap();
        assert_eq!(report.excluded_groups().len(), 1);
        assert_eq!(report.excluded_groups()[0].history_id(), combined_ids[0]);
        assert_eq!(report.excluded_groups()[0].unseen_memberships(), 1);
        assert_eq!(identities(&output), first_ids);
        assert_eq!(std::fs::read(&first).unwrap(), before);
        assert_eq!(std::fs::read(&combined).unwrap(), incoming_before);
        let restored = restore_existing(&first, &combined, || false).unwrap();
        assert_eq!(restored, report);
        assert_eq!(identities(&first), first_ids);
        assert_eq!(std::fs::read(&combined).unwrap(), incoming_before);
        let reverse = merge(&combined, &first, &output).unwrap();
        assert!(reverse.excluded_groups().is_empty());
        assert_eq!(reverse.retained_groups().len(), 1);
        assert_eq!(reverse.retained_groups()[0].history_id(), combined_ids[0]);
        assert_eq!(reverse.retained_groups()[0].unseen_memberships(), 1);
        assert_eq!(reverse.nonexact_replays(), 1);
        assert_eq!(identities(&output), identities(&combined));
    }

    #[test]
    fn test_import_snapshot_spanning_membership_keeps_existing_grouping() {
        let dir = tempfile::tempdir().unwrap();
        let split = dir.path().join("split");
        let combined = dir.path().join("combined");
        let output = dir.path().join("output");
        record(&split, &[A]);
        record(&split, &[B]);
        record(&combined, &[A, B]);
        merge(&split, &combined, &output).unwrap();
        assert_eq!(identities(&output), identities(&split));
        let reverse = dir.path().join("reverse");
        merge(&combined, &split, &reverse).unwrap();
        assert_eq!(identities(&reverse), identities(&combined));
    }

    #[test]
    fn test_import_snapshot_exact_group_result_disagreement_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        record(&first, &[A]);
        record(&second, &[A]);
        Connection::open(&second)
            .unwrap()
            .execute("UPDATE results SET duration_ms=999", [])
            .unwrap();
        assert!(matches!(
            merge(&first, &second, &dir.path().join("out")),
            Err(SnapshotError::Invalid(
                "same report group has conflicting results"
            ))
        ));
    }

    #[test]
    fn test_import_snapshot_shared_retained_id_guard_precedes_report_skip() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        record(&first, &[A]);
        std::fs::copy(&first, &second).unwrap();
        Connection::open(&second)
            .unwrap()
            .execute("UPDATE runs SET source='different'", [])
            .unwrap();
        assert!(matches!(
            merge(&first, &second, &dir.path().join("out")),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
    }

    #[test]
    fn test_import_snapshot_later_shared_id_conflict_aborts_before_partial_group_skip() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        let incoming = dir.path().join("incoming");
        let output = dir.path().join("output");
        record(&destination, &[A]);
        Store::open(&destination)
            .unwrap()
            .record_run(
                &NewRun {
                    commit: None,
                    source: "shared",
                    files: &[],
                    partial: false,
                    branch: None,
                    tag: None,
                },
                std::iter::empty(),
            )
            .unwrap();
        record(&incoming, &[A, B]);
        let connection = Connection::open(&incoming).unwrap();
        connection
            .execute(
                "ATTACH DATABASE ?1 AS original",
                [destination.to_str().unwrap()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO runs(commit_sha,source,ingested_at,partial,branch,tag,history_id)
             SELECT commit_sha,'conflicting',ingested_at,partial,branch,tag,history_id
             FROM original.runs WHERE source='shared'",
                [],
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&destination).unwrap();
        assert!(matches!(
            merge(&destination, &incoming, &output),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
        assert!(!output.exists());
        assert!(matches!(
            restore_existing(&destination, &incoming, || false),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), before);
    }

    #[test]
    fn test_import_snapshot_restore_roundtrip_retains_membership_and_local_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let exported = dir.path().join("exported");
        let local = dir.path().join("local");
        record(&original, &[A, B]);
        record(&local, &[B, A]);
        let local_ids = identities(&local);
        snapshot(&original, &exported).unwrap();
        restore_existing(&local, &exported, || false).unwrap();
        assert_eq!(identities(&local), local_ids);
        let conn = Connection::open(&local).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM report_memberships", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(conn);
        // Membership is actually read by the importer after transport, not only
        // written into a table the intended consumer never reaches.
        record(&local, &[A, B]);
        assert_eq!(identities(&local), local_ids);
    }

    #[test]
    fn test_import_snapshot_membership_namespace_must_match_owner() {
        for commit in ["aaaaaaa", "", "legacy"] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("db");
            record(&db, &[A]);
            Connection::open(&db)
                .unwrap()
                .execute("UPDATE runs SET commit_sha=?1", [commit])
                .unwrap();
            assert!(matches!(
                validate(&db),
                Err(SnapshotError::Invalid(
                    "report membership namespace disagrees with run commit"
                ))
            ));
        }
    }
}

/// Import into the existing SQLite inode while excluding other SQLite writers.
/// Incoming schemas remain data: only the trusted schema factory supplies DDL.
pub(crate) fn restore_existing(
    db: &Path,
    incoming: &Path,
    mut cancelled: impl FnMut() -> bool,
) -> Result<MergeReport, SnapshotError> {
    restore_existing_transaction(db, incoming, &mut cancelled, |_, _| Ok(()))
}

#[derive(Clone, Copy)]
enum RestoreCheckpoint {
    Reserved,
    BeforeCommit,
}

// The private observer gives transaction tests an exact boundary without
// process control or relying on how often the cancellation predicate is polled.
fn restore_existing_transaction(
    db: &Path,
    incoming: &Path,
    cancelled: &mut impl FnMut() -> bool,
    mut observe: impl FnMut(RestoreCheckpoint, &Connection) -> Result<(), SnapshotError>,
) -> Result<MergeReport, SnapshotError> {
    if cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    let incoming = load(incoming)?;
    check_output(db)?;
    let mut conn = Connection::open_with_flags(
        store::as_plain_file(db),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    // Reserve the writer before the first local schema or row read. Never
    // replace this inode: already-open SQLite handles must follow the commit.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    observe(RestoreCheckpoint::Reserved, &tx)?;
    if cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    let local = if schema(&tx)?.is_empty() {
        Data {
            tests: DictionaryBuilder::default().freeze(),
            runs: freeze_runs(std::iter::empty()),
        }
    } else {
        let version = checked_connection_version(&tx)?;
        load_connection(&tx, version)?
    };
    let (data, report) = union(local, incoming)?;
    if cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    let canonical = store::canonical_schema(SCHEMA_VERSION)?;
    let definitions = schema(&canonical)?;
    tx.execute_batch("DROP TABLE IF EXISTS report_memberships; DROP TABLE IF EXISTS results; DROP TABLE IF EXISTS run_files; DROP TABLE IF EXISTS runs; DROP TABLE IF EXISTS tests;")?;
    for kind in ["table", "index"] {
        for (object_kind, _, _, sql) in &definitions {
            if object_kind == kind {
                if let Some(sql) = sql {
                    tx.execute_batch(sql)?;
                }
            }
        }
    }
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    insert_data(&tx, &data)?;
    let version = checked_connection_version(&tx)?;
    load_connection(&tx, version)?;
    observe(RestoreCheckpoint::BeforeCommit, &tx)?;
    if cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let sources = conn
            .prepare("SELECT source FROM runs ORDER BY source")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        sources
    }

    #[test]
    fn test_restore_existing_reserves_writer_before_local_read_and_preserves_inode() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let incoming = dir.path().join("incoming");
        fixture(&local, "local");
        fixture(&incoming, "remote");
        let writer = Connection::open(&local).unwrap();
        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&local).unwrap().ino()
        };
        let mut reserved = false;
        restore_existing_transaction(&local,&incoming,&mut||false,|checkpoint,_| {
            if matches!(checkpoint,RestoreCheckpoint::Reserved) {
                // No local schema/data query has run yet. A DEFERRED mutation
                // allows this ordinary writer, so this assertion must go red.
                let result=writer.execute("INSERT INTO runs(source,history_id) VALUES ('competing',lower(hex(randomblob(16))))",[]);
                assert!(matches!(result,Err(rusqlite::Error::SqliteFailure(error,_)) if error.code==rusqlite::ErrorCode::DatabaseBusy));
                reserved=true;
            }
            Ok(())
        }).unwrap();
        assert!(reserved);
        writer
            .execute(
                "INSERT INTO runs(source,history_id) VALUES ('after',lower(hex(randomblob(16))))",
                [],
            )
            .unwrap();
        assert_eq!(sources(&local), vec!["after", "local", "remote"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&local).unwrap().ino(), inode);
        }
    }

    #[test]
    fn test_restore_existing_cancel_before_commit_rolls_back_rewritten_tables() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let incoming = dir.path().join("incoming");
        fixture(&local, "local");
        fixture(&incoming, "remote");
        let before = std::fs::read(&local).unwrap();
        let cancel = std::cell::Cell::new(false);
        let result = restore_existing_transaction(
            &local,
            &incoming,
            &mut || cancel.get(),
            |checkpoint, tx| {
                if matches!(checkpoint, RestoreCheckpoint::BeforeCommit) {
                    assert_eq!(
                        tx.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
                            .unwrap(),
                        2
                    );
                    cancel.set(true);
                }
                Ok(())
            },
        );
        assert!(matches!(result, Err(SnapshotError::Cancelled)));
        assert!(cancel.get());
        assert_eq!(sources(&local), vec!["local"]);
        assert_eq!(std::fs::read(&local).unwrap(), before);
    }

    #[test]
    fn test_restore_existing_supported_schemas_upgrade_in_place_and_keep_identities() {
        for version in 1..=SCHEMA_VERSION {
            let dir = tempfile::tempdir().unwrap();
            let local = dir.path().join("local");
            let incoming = dir.path().join("incoming");
            if version < SCHEMA_VERSION {
                historical_fixture(&local, version);
            } else {
                fixture(&local, "legacy");
            }
            assert_eq!(
                Connection::open(&local)
                    .unwrap()
                    .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                version,
                "restore fixture must exercise the named source schema"
            );
            fixture(&incoming, "remote");
            let before = load(&local)
                .unwrap()
                .runs
                .iter()
                .map(|run| run.metadata().history_id().as_str().to_owned())
                .collect::<Vec<_>>();
            #[cfg(unix)]
            let inode = {
                use std::os::unix::fs::MetadataExt;
                std::fs::metadata(&local).unwrap().ino()
            };
            restore_existing(&local, &incoming, || false).unwrap();
            assert_eq!(sources(&local), vec!["legacy", "remote"]);
            let restored = load(&local).unwrap();
            assert!(before.iter().all(|id| {
                restored
                    .runs
                    .iter()
                    .any(|run| run.metadata().history_id().as_str() == id)
            }));
            let conn = Connection::open(&local).unwrap();
            assert_eq!(checked_connection_version(&conn).unwrap(), SCHEMA_VERSION);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(std::fs::metadata(&local).unwrap().ino(), inode);
            }
        }
    }

    #[test]
    fn test_restore_existing_identity_conflict_rolls_back_without_changes() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let incoming = dir.path().join("incoming");
        fixture(&local, "local");
        std::fs::copy(&local, &incoming).unwrap();
        Connection::open(&incoming)
            .unwrap()
            .execute("UPDATE runs SET source='conflict'", [])
            .unwrap();
        let before = std::fs::read(&local).unwrap();
        assert!(matches!(
            restore_existing(&local, &incoming, || false),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
        assert_eq!(std::fs::read(&local).unwrap(), before);
    }

    #[test]
    fn test_frozen_snapshot_layout_and_capacity_receipt() {
        eprintln!(
            "snapshot layout: TestKey={} TestId={} ResultRow={} RunMetadata={} RunBuilder={} Run={} file_row={}",
            std::mem::size_of::<TestKey>(),
            std::mem::size_of::<TestId>(),
            std::mem::size_of::<ResultRow>(),
            std::mem::size_of::<RunMetadata>(),
            std::mem::size_of::<RunBuilder>(),
            std::mem::size_of::<Run>(),
            std::mem::size_of::<(i64, FrozenBytes)>()
        );
        let mut dictionary = DictionaryBuilder::default();
        let test = dictionary.insert(TestKey::new(
            FrozenText::copy_from("suite"),
            FrozenText::copy_from("test"),
        ));
        for len in [0, 1, 2, 3, 5] {
            let mut results = Vec::new();
            let mut files = Vec::new();
            for index in 0..len {
                if results.capacity() == 0 {
                    results.reserve_exact(1);
                }
                if files.capacity() == 0 {
                    files.reserve_exact(1);
                }
                results.push(ResultRow::new(test, Outcome::Passed, Some(1)));
                files.push((index as i64, FrozenBytes::copy_from(&[0, 255])));
            }
            eprintln!(
                "snapshot capacity: len={len} results={} files={}",
                results.capacity(),
                files.capacity()
            );
            assert!(results.capacity() + len <= 3 * len);
            assert!(files.capacity() + len <= 3 * len);
            assert_eq!(FrozenResults::from_vec(results).len(), len);
            assert_eq!(FrozenFiles::from_vec(files).len(), len);
        }
    }

    #[test]
    fn test_frozen_binary_paths_share_storage_without_mutable_aliases() {
        let mut source = vec![0, 255, 128];
        let frozen = FrozenBytes::copy_from(&source);
        let shared = frozen.clone();
        source[0] = 42;
        assert_eq!(frozen.as_bytes(), &[0, 255, 128]);
        assert!(std::ptr::eq(
            frozen.as_bytes().as_ptr(),
            shared.as_bytes().as_ptr()
        ));
    }

    #[test]
    fn test_merge_remaps_different_local_test_ids_before_comparing_run_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "same");
        Connection::open(&first)
            .unwrap()
            .execute_batch("UPDATE tests SET name='a'; INSERT INTO tests VALUES (2,'suite','z')")
            .unwrap();
        std::fs::copy(&first, &second).unwrap();
        Connection::open(&second).unwrap().execute_batch("PRAGMA foreign_keys=OFF; UPDATE tests SET id=20 WHERE id=1; UPDATE tests SET id=10 WHERE id=2; UPDATE results SET test_id=20").unwrap();
        merge(&first, &second, &output).unwrap();
        let conn = Connection::open(&output).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT t.name FROM results r JOIN tests t ON t.id=r.test_id",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "a"
        );
        let keys = conn
            .prepare("SELECT name FROM tests ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(keys, vec!["a", "z"]);
    }

    #[test]
    fn test_merge_same_local_test_id_with_different_semantic_key_is_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "same");
        std::fs::copy(&first, &second).unwrap();
        Connection::open(&second)
            .unwrap()
            .execute("UPDATE tests SET name='different'", [])
            .unwrap();
        std::fs::write(&output, b"preserved").unwrap();
        assert!(matches!(
            merge(&first, &second, &output),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
        assert_eq!(std::fs::read(&output).unwrap(), b"preserved");
    }

    #[test]
    fn test_dictionary_repeated_names_share_one_frozen_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        fixture(&path, "source");
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE tests SET name=?1", ["x".repeat(1024)])
            .unwrap();
        for id in 2..=10 {
            conn.execute("INSERT INTO runs(id,source,history_id) VALUES (?1,'source',lower(hex(randomblob(16))))", [id]).unwrap();
            conn.execute("INSERT INTO results VALUES (?1,1,'passed',1)", [id])
                .unwrap();
        }
        drop(conn);
        let data = load(&path).unwrap();
        assert_eq!(data.tests.iter().count(), 1);
        let name = data.tests.iter().next().unwrap().name().as_str();
        assert_eq!(name.len(), 1024);
        for run in data.runs.iter() {
            let result = run.results().iter().next().unwrap();
            let resolved = data.tests.get(result.test()).unwrap().name().as_str();
            assert!(std::ptr::eq(name.as_ptr(), resolved.as_ptr()));
        }
        assert!(std::mem::size_of::<ResultRow>() <= 32);
    }

    #[test]
    fn test_unique_dictionary_fields_beyond_former_length_limit_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        fixture(&path, "source");
        let conn = Connection::open(&path).unwrap();
        for index in 0..10 {
            conn.execute(
                "INSERT INTO tests(suite,name) VALUES ('suite',?1)",
                [format!("{}-{index}", "x".repeat(65_537))],
            )
            .unwrap();
        }
        drop(conn);
        let output = dir.path().join("output");
        snapshot(&path, &output).unwrap();
        let data = load(&output).unwrap();
        assert_eq!(data.tests.iter().count(), 11);
        assert_eq!(
            data.tests
                .iter()
                .filter(|key| key.name().len() > 65_536)
                .count(),
            10
        );
    }

    fn fixture(path: &Path, source: &str) {
        drop(Store::open(path).unwrap());
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO runs(source,ingested_at,history_id) VALUES (?1,'2026-01-01T00:00:00.000Z',lower(hex(randomblob(16))))",
            [source],
        )
        .unwrap();
        conn.execute("INSERT INTO tests(suite,name) VALUES ('suite','test')", [])
            .unwrap();
        conn.execute("INSERT INTO results VALUES (1,1,'passed',12)", [])
            .unwrap();
        conn.execute("INSERT INTO run_files VALUES (1,0,x'00ff')", [])
            .unwrap();
    }

    #[test]
    fn test_validate_extra_schema_and_bad_values_rejected() {
        for (change, expected) in [
            (
                "CREATE TABLE extra(value)",
                "schema does not exactly match this greenshift release",
            ),
            (
                "CREATE VIEW extra AS SELECT * FROM tests",
                "schema does not exactly match this greenshift release",
            ),
            (
                "CREATE TRIGGER extra AFTER INSERT ON tests BEGIN DELETE FROM results; END",
                "schema does not exactly match this greenshift release",
            ),
            ("PRAGMA application_id=0", "application id mismatch"),
            (
                "PRAGMA user_version=999",
                "unsupported snapshot schema version",
            ),
            (
                "PRAGMA ignore_check_constraints=ON; UPDATE results SET duration_ms=-1",
                "invalid outcome or duration",
            ),
            (
                "PRAGMA foreign_keys=OFF; UPDATE results SET test_id=999",
                "broken foreign key",
            ),
            (
                "PRAGMA foreign_keys=OFF; UPDATE runs SET id=-1; UPDATE results SET run_id=-1; UPDATE run_files SET run_id=-1",
                "invalid run id, partial flag, or branch/tag",
            ),
            (
                "UPDATE runs SET history_id=NULL",
                "run history id must be 32 lowercase hex digits",
            ),
            (
                "UPDATE runs SET history_id='ABCDEF'",
                "run history id must be 32 lowercase hex digits",
            ),
            (
                "UPDATE runs SET history_id='0000000000000000000000000000000A'",
                "run history id must be 32 lowercase hex digits",
            ),
            (
                "UPDATE runs SET history_id='0000000000000000000000000000000g'",
                "run history id must be 32 lowercase hex digits",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("db");
            fixture(&path, "test");
            Connection::open(&path)
                .unwrap()
                .execute_batch(change)
                .unwrap();
            let result = validate(&path);
            assert!(
                matches!(&result, Err(SnapshotError::Invalid(message)) if *message == expected),
                "wrong rejection for {change}: {result:?}"
            );
        }
    }

    fn historical_fixture(path: &Path, version: i64) {
        let conn = store::canonical_schema(version).unwrap();
        conn.execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        let output = Connection::open(path).unwrap();
        output.execute("INSERT INTO runs(id,source,ingested_at) VALUES (9,'legacy','2020-01-01T00:00:00.000Z')",[]).unwrap();
        if version >= 4 {
            output
                .execute(
                    "UPDATE runs SET history_id='00000000000000000000000000000009'",
                    [],
                )
                .unwrap();
        }
        output
            .execute("INSERT INTO tests VALUES (4,'suite','test')", [])
            .unwrap();
        output
            .execute("INSERT INTO results VALUES (9,4,'passed',12)", [])
            .unwrap();
        output
            .execute("INSERT INTO run_files VALUES (9,0,x'00ff')", [])
            .unwrap();
    }

    #[test]
    fn test_snapshot_supported_historical_schemas_upgrade_without_input_mutation() {
        for version in 1..SCHEMA_VERSION {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("input");
            let output = dir.path().join("output");
            historical_fixture(&input, version);
            let before = std::fs::read(&input).unwrap();
            validate(&input).unwrap();
            snapshot(&input, &output).unwrap();
            assert_eq!(std::fs::read(&input).unwrap(), before);
            let upgraded = Store::open_read_only(&output).unwrap().unwrap();
            assert_eq!(upgraded.schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(upgraded.history(&[]).unwrap().runs().len(), 1);
            let conn = Connection::open(&output).unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM report_memberships", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn test_snapshot_copy_contract_covers_every_canonical_table_and_column() {
        let conn = store::canonical_schema(SCHEMA_VERSION).unwrap();
        let mut tables: Vec<_> = schema(&conn)
            .unwrap()
            .into_iter()
            .filter(|object| object.0 == "table")
            .map(|object| object.1)
            .collect();
        tables.sort();
        assert_eq!(
            tables,
            vec![
                "report_memberships",
                "results",
                "run_files",
                "runs",
                "tests"
            ]
        );
        for (table, expected) in [
            (
                "runs",
                vec![
                    "id",
                    "commit_sha",
                    "source",
                    "ingested_at",
                    "partial",
                    "branch",
                    "tag",
                    "history_id",
                ],
            ),
            ("tests", vec!["id", "suite", "name"]),
            (
                "results",
                vec!["run_id", "test_id", "outcome", "duration_ms"],
            ),
            ("run_files", vec!["run_id", "position", "path"]),
            (
                "report_memberships",
                vec!["run_id", "namespace", "fingerprint_version", "digest"],
            ),
        ] {
            let columns = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                columns, expected,
                "snapshot copy contract must cover new fields in {table}"
            );
        }
    }

    #[test]
    fn test_snapshot_legacy_trigger_and_future_version_reject_without_writes() {
        for (change, expected) in [
            (
                "CREATE TRIGGER attack AFTER UPDATE ON runs BEGIN DELETE FROM tests; END",
                "schema does not exactly match this greenshift release",
            ),
            (
                "PRAGMA user_version=999",
                "unsupported snapshot schema version",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("input");
            let output = dir.path().join("output");
            historical_fixture(&input, 3);
            Connection::open(&input)
                .unwrap()
                .execute_batch(change)
                .unwrap();
            let before = std::fs::read(&input).unwrap();
            std::fs::write(&output, b"untouched").unwrap();
            assert!(
                matches!(snapshot(&input,&output),Err(SnapshotError::Invalid(message)) if message==expected)
            );
            assert_eq!(std::fs::read(&input).unwrap(), before);
            assert_eq!(std::fs::read(&output).unwrap(), b"untouched");
        }
    }

    #[test]
    fn test_snapshot_legacy_identity_v1_golden_bytes_and_store_migration_agree() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let snapshot = dir.path().join("snapshot");
        historical_fixture(&input, 3);
        super::snapshot(&input, &snapshot).unwrap();
        let identity = |path: &Path| {
            Connection::open(path)
                .unwrap()
                .query_row("SELECT history_id FROM runs", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        };
        // Independent SHA-256 oracle over the fixed v1 typed encoding of the
        // fixture (original id9, NULL commit, legacy source/time/files/results).
        assert_eq!(identity(&snapshot), "29823b29db8f0d9396909bc519396cd2");
        drop(Store::open(&input).unwrap());
        assert_eq!(identity(&input), identity(&snapshot));
    }

    #[test]
    fn test_store_open_invalid_history_identity_refused_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        fixture(&input, "source");
        Connection::open(&input)
            .unwrap()
            .execute("UPDATE runs SET history_id=NULL", [])
            .unwrap();
        let before = std::fs::read(&input).unwrap();
        assert!(matches!(
            Store::open(&input),
            Err(store::StoreError::InvalidHistoryId { .. })
        ));
        assert_eq!(std::fs::read(&input).unwrap(), before);
    }

    #[test]
    fn test_snapshot_legacy_arbitrary_commit_is_data_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        historical_fixture(&input, 3);
        let legacy = "legacy ref; $(not-a-command)\nHEAD";
        Connection::open(&input)
            .unwrap()
            .execute("UPDATE runs SET commit_sha=?1", [legacy])
            .unwrap();
        snapshot(&input, &output).unwrap();
        let conn = Connection::open(&output).unwrap();
        assert_eq!(
            conn.query_row("SELECT commit_sha FROM runs", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            legacy
        );
        assert_eq!(
            conn.query_row("SELECT length(history_id) FROM runs", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            32
        );
        drop(conn);
        Connection::open(&input)
            .unwrap()
            .execute("UPDATE runs SET commit_sha=?1", ["x".repeat(65_537)])
            .unwrap();
        snapshot(&input, &output).unwrap();
        assert_eq!(
            Connection::open(&output)
                .unwrap()
                .query_row("SELECT length(commit_sha) FROM runs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            65_537
        );
    }

    #[test]
    fn test_merge_orders_runs_chronologically_then_by_identity_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let early = dir.path().join("early");
        let late = dir.path().join("late");
        let forward = dir.path().join("forward");
        let reverse = dir.path().join("reverse");
        fixture(&early, "early");
        fixture(&late, "late");
        Connection::open(&early).unwrap().execute("UPDATE runs SET ingested_at='2020-01-01T00:00:00.000Z',history_id='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'",[]).unwrap();
        Connection::open(&late).unwrap().execute_batch("UPDATE runs SET ingested_at='2021-01-01T00:00:00.000Z',history_id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; INSERT INTO runs(source,ingested_at,history_id) VALUES ('tie','2021-01-01T00:00:00.000Z','00000000000000000000000000000000')").unwrap();
        Connection::open(&late).unwrap().execute_batch("INSERT INTO tests VALUES (2,'suite','new-case'); INSERT INTO results VALUES (1,2,'failed',4)").unwrap();
        merge(&early, &late, &forward).unwrap();
        merge(&late, &early, &reverse).unwrap();
        let order = |path: &Path| {
            let conn = Connection::open(path).unwrap();
            let order = conn
                .prepare("SELECT source FROM runs ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            order
        };
        assert_eq!(order(&forward), vec!["early", "tie", "late"]);
        assert_eq!(order(&reverse), order(&forward));
        let analyse = |path: &Path| {
            crate::select::analyse(
                &Store::open_read_only(path)
                    .unwrap()
                    .unwrap()
                    .history(&[])
                    .unwrap(),
            )
            .unwrap()
        };
        let forward_selection = analyse(&forward);
        assert!(forward_selection
            .tests()
            .iter()
            .any(|test| test.name().as_raw() == "new-case"));
        assert_eq!(analyse(&reverse), forward_selection);
    }

    #[test]
    fn test_snapshot_readback_preserves_metadata_and_binary_paths() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        fixture(&input, "producer");
        snapshot(&input, &output).unwrap();
        let db = Connection::open(&output).unwrap();
        assert_eq!(
            db.query_row("SELECT source FROM runs", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "producer"
        );
        assert_eq!(
            db.query_row("SELECT path FROM run_files", [], |r| r.get::<_, Vec<u8>>(0))
                .unwrap(),
            vec![0, 255]
        );
        assert_eq!(
            db.query_row("SELECT duration_ms FROM results", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            12
        );
    }

    #[test]
    fn test_merge_colliding_ids_keeps_disjoint_runs_and_retry_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "first");
        fixture(&second, "second");
        merge(&first, &second, &output).unwrap();
        merge(&output, &second, &first).unwrap();
        let conn = Connection::open(&first).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM results", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM tests", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_merge_invalid_incoming_leaves_output_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "first");
        std::fs::write(&second, b"not SQLite").unwrap();
        std::fs::write(&output, b"sentinel").unwrap();
        assert!(merge(&first, &second, &output).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"sentinel");
    }

    #[test]
    fn test_large_text_and_binary_paths_preserved_without_arbitrary_length_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        fixture(&path, "test");
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE tests SET name=?1", ["x".repeat(65_537)])
            .unwrap();
        let bytes = vec![255u8; 65_537];
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE run_files SET path=?1", [&bytes])
            .unwrap();
        let output = dir.path().join("output");
        snapshot(&path, &output).unwrap();
        let conn = Connection::open(&output).unwrap();
        assert_eq!(
            conn.query_row("SELECT length(name) FROM tests", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            65_537
        );
        assert_eq!(
            conn.query_row("SELECT path FROM run_files", [], |r| r.get::<_, Vec<u8>>(0))
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn test_merge_identical_independent_runs_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "same");
        snapshot(&first, &second).unwrap();
        let conn = Connection::open(&second).unwrap();
        conn.execute(
            "INSERT INTO runs(source,ingested_at,history_id) SELECT source,ingested_at,lower(hex(randomblob(16))) FROM runs",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO results SELECT 2,test_id,outcome,duration_ms FROM results",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO run_files SELECT 2,position,path FROM run_files",
            [],
        )
        .unwrap();
        drop(conn);
        merge(&first, &second, &output).unwrap();
        merge(&output, &second, &first).unwrap();
        let conn = Connection::open(&first).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn test_merge_same_identity_conflicting_contents_rejected_without_output_change() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "source");
        snapshot(&first, &second).unwrap();
        Connection::open(&second)
            .unwrap()
            .execute("UPDATE results SET outcome='failed'", [])
            .unwrap();
        std::fs::write(&output, b"unchanged").unwrap();
        assert!(matches!(
            merge(&first, &second, &output),
            Err(SnapshotError::Invalid(
                "one run identity has conflicting contents"
            ))
        ));
        assert_eq!(std::fs::read(output).unwrap(), b"unchanged");
    }

    #[test]
    fn test_merge_two_baseline_clones_keep_baseline_once_and_both_new_runs() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let output = dir.path().join("output");
        fixture(&first, "same");
        snapshot(&first, &second).unwrap();
        for path in [&first, &second] {
            let conn = Connection::open(path).unwrap();
            conn.execute("INSERT INTO runs(source,ingested_at,history_id) SELECT source,ingested_at,lower(hex(randomblob(16))) FROM runs", []).unwrap();
            conn.execute(
                "INSERT INTO results SELECT 2,test_id,outcome,duration_ms FROM results",
                [],
            )
            .unwrap();
        }
        merge(&first, &second, &output).unwrap();
        let conn = Connection::open(&output).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM results", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn test_valid_sparse_file_beyond_former_size_cap_and_corruption_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        fixture(&path, "synthetic");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(256 * 1024 * 1024 + 4096).unwrap();
        validate(&path).unwrap();
        drop(file);
        std::fs::write(&path, b"SQLite format 3\0truncated").unwrap();
        assert!(validate(&path).is_err());
    }

    #[test]
    fn test_snapshot_empty_history_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        drop(Store::open(&input).unwrap());
        snapshot(&input, &output).unwrap();
        let store = Store::open_read_only(&output).unwrap().unwrap();
        assert!(store.history(&[]).unwrap().runs().is_empty());
    }

    #[test]
    fn test_validate_valid_live_wal_input_rejected_without_touching_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        fixture(&path, "source");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; UPDATE runs SET source='live-wal';")
            .unwrap();
        let wal = dir.path().join("input-wal");
        let before = std::fs::read(&wal).unwrap();
        assert!(!before.is_empty());
        assert!(matches!(
            validate(&path),
            Err(SnapshotError::Invalid(
                "snapshot must be standalone: SQLite sidecar exists"
            ))
        ));
        assert_eq!(std::fs::read(&wal).unwrap(), before);
        assert_eq!(
            conn.query_row("SELECT source FROM runs", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "live-wal"
        );
    }

    #[test]
    fn test_snapshot_live_wal_output_rejected_preserving_visible_database() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        fixture(&input, "replacement");
        fixture(&output, "original");
        let conn = Connection::open(&output).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; UPDATE runs SET source='visible-original';")
            .unwrap();
        let before_db = std::fs::read(&output).unwrap();
        let before_wal = std::fs::read(dir.path().join("output-wal")).unwrap();
        assert!(snapshot(&input, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), before_db);
        assert_eq!(
            std::fs::read(dir.path().join("output-wal")).unwrap(),
            before_wal
        );
        let reader = Connection::open(&output).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT source FROM runs", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "visible-original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_snapshot_nonregular_inputs_and_outputs_rejected_before_sqlite_open() {
        use std::os::unix::fs::{symlink, FileTypeExt};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let link = dir.path().join("link");
        let socket = dir.path().join("socket");
        fixture(&input, "source");
        symlink(&input, &link).unwrap();
        let _listener = UnixListener::bind(&socket).unwrap();
        for path in [&link, &socket, dir.path()] {
            assert!(matches!(validate(path), Err(SnapshotError::Invalid(_))));
            assert!(snapshot(&input, path).is_err());
        }
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket());
    }

    #[test]
    fn test_snapshot_orphan_sidecars_are_never_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let output = dir.path().join("output");
        fixture(&input, "source");
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = dir.path().join(format!("output{suffix}"));
            std::fs::write(&sidecar, b"owned-by-another-writer").unwrap();
            assert!(snapshot(&input, &output).is_err());
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"owned-by-another-writer");
            assert!(!output.exists());
            std::fs::remove_file(&sidecar).unwrap();
        }
    }
}
