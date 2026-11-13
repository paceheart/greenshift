use std::path::PathBuf;

use super::*;

fn tc(suite: &str, name: &str, outcome: Outcome, ms: Option<i64>) -> TestCase {
    TestCase {
        suite: suite.into(),
        name: name.into(),
        outcome,
        duration_ms: ms,
    }
}

const RUN: NewRun<'static> = NewRun {
    commit: Some("0123abcd"),
    source: "nextest",
    files: &[],
};

/// The paths recorded for a run, in order, as stored.
fn run_files(store: &Store, run_id: i64) -> Vec<(i64, Vec<u8>)> {
    let mut stmt = store
        .conn
        .prepare("SELECT position, path FROM run_files WHERE run_id = ?1 ORDER BY position")
        .unwrap();
    stmt.query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

type Row = (String, String, String, Option<i64>);

fn strings(store: &Store, sql: &str) -> Vec<String> {
    let mut stmt = store.conn.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .conn
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// (suite, name, outcome, duration_ms) for a run, by suite and name.
fn rows(store: &Store, run_id: i64) -> Vec<Row> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT t.suite, t.name, r.outcome, r.duration_ms
             FROM results r JOIN tests t ON t.id = r.test_id
             WHERE r.run_id = ?1 ORDER BY t.suite, t.name",
        )
        .unwrap();
    stmt.query_map([run_id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn row(suite: &str, name: &str, outcome: &str, ms: Option<i64>) -> Row {
    (suite.into(), name.into(), outcome.into(), ms)
}

#[test]
fn fresh_store_is_at_the_current_schema_version() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn schema_has_runs_tests_and_results() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name"
        ),
        ["results", "run_files", "runs", "tests"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('runs') ORDER BY cid"
        ),
        ["id", "commit_sha", "source", "ingested_at"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('run_files') ORDER BY cid"
        ),
        ["run_id", "position", "path"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('run_files') WHERE pk > 0 ORDER BY pk"
        ),
        ["run_id", "position"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('results') ORDER BY cid"
        ),
        ["run_id", "test_id", "outcome", "duration_ms"]
    );
    // The test identity is unique and indexed on (suite, name).
    assert_eq!(
        strings(
            &store,
            "SELECT ii.name FROM pragma_index_list('tests') il,
                    pragma_index_info(il.name) ii
             WHERE il.\"unique\" ORDER BY ii.seqno"
        ),
        ["suite", "name"]
    );
}

#[test]
fn results_is_without_rowid_keyed_by_run_and_test() {
    let store = Store::open_in_memory().unwrap();
    let sql: String = store
        .conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name = 'results'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("WITHOUT ROWID"), "{sql}");
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('results') WHERE pk > 0 ORDER BY pk"
        ),
        ["run_id", "test_id"]
    );
}

#[test]
fn open_creates_the_parent_directory_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/.greenshift/history.sqlite");
    {
        let mut store = Store::open(&path).unwrap();
        store
            .record_run(&RUN, &[tc("s", "t", Outcome::Passed, Some(1))])
            .unwrap();
    }
    assert!(path.is_file());
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(strings(&store, "SELECT name FROM tests"), ["t"]);
}

#[test]
fn a_parent_that_is_a_file_is_a_create_dir_error() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let err = Store::open(&blocker.join("sub/history.sqlite")).unwrap_err();
    assert!(
        matches!(err, StoreError::CreateDir { ref path, .. } if *path == blocker.join("sub").display().to_string()),
        "{err:?}"
    );
    assert!(blocker.is_file());
}

fn application_id(store: &Store) -> i64 {
    store
        .conn
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .unwrap()
}

#[test]
fn a_fresh_store_is_marked_as_greenshift() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(application_id(&store), i64::from(APPLICATION_ID));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    drop(Store::open(&path).unwrap());
    let store = Store::open(&path).unwrap();
    assert_eq!(application_id(&store), i64::from(APPLICATION_ID));
}

#[test]
fn an_empty_sqlite_file_becomes_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "page_size", 8192).unwrap();
    conn.execute_batch("VACUUM").unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(application_id(&store), i64::from(APPLICATION_ID));
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn an_empty_sqlite_file_with_a_stray_user_version_becomes_a_store() {
    // No schema, so it is taken over; its user_version says nothing about
    // greenshift's schema and must not skip migration 1.
    for (app_id, user_version) in [(0, 1), (0, SCHEMA_VERSION + 6), (42, 1)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", app_id).unwrap();
        conn.pragma_update(None, "user_version", user_version)
            .unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(application_id(&store), i64::from(APPLICATION_ID));
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        store
            .record_run(&RUN, &[tc("s", "t", Outcome::Passed, None)])
            .unwrap();
    }
}

/// Make a SQLite file at `path` that some other program owns, and return
/// its bytes.
fn foreign_db(path: &Path, application_id: i32) -> Vec<u8> {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('mine');")
        .unwrap();
    conn.pragma_update(None, "application_id", application_id)
        .unwrap();
    drop(conn);
    std::fs::read(path).unwrap()
}

#[test]
fn a_database_of_another_program_is_refused_and_left_alone() {
    for foreign in [0, 0x1234_5678] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let before = foreign_db(&path, foreign);
        let err = Store::open(&path).unwrap_err();
        assert!(
            matches!(err, StoreError::NotAStore { found, .. } if found == foreign),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("not a greenshift history store"),
            "{err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "file was modified");
    }
}

#[test]
fn store_from_a_newer_greenshift_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    let err = Store::open(&path).unwrap_err();
    assert!(matches!(err, StoreError::TooNew { found, .. } if found == SCHEMA_VERSION + 1));
    assert!(err.to_string().contains("upgrade greenshift"), "{err}");
}

#[test]
fn the_special_memory_name_is_refused_as_a_db_path() {
    // SQLite treats the bare filename ":memory:" as its private in-memory
    // database even without URI mode. Opening it as if it were a real path
    // would report success while recording nothing to disk.
    let err = Store::open(Path::new(":memory:")).unwrap_err();
    assert!(matches!(err, StoreError::MemoryPath { .. }), "{err:?}");
}

#[test]
fn the_empty_name_is_refused_as_a_db_path() {
    // SQLite opens "" as a private temporary database, gone on close.
    let err = Store::open(Path::new("")).unwrap_err();
    assert!(matches!(err, StoreError::MemoryPath { .. }), "{err:?}");
    assert!(err.to_string().starts_with(r#""""#), "{err}");
}

#[test]
fn store_with_a_negative_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", -1).unwrap();
    drop(conn);
    let err = Store::open(&path).unwrap_err();
    assert!(
        matches!(err, StoreError::BadVersion { found: -1, .. }),
        "{err:?}"
    );
}

#[test]
fn record_run_writes_one_run_and_every_result() {
    let mut store = Store::open_in_memory().unwrap();
    let cases = [
        tc("widgets", "parses_a_sprocket", Outcome::Passed, Some(4)),
        tc("widgets", "rejects_a_bent_cog", Outcome::Failed, Some(12)),
        tc("widgets", "spins", Outcome::Error, None),
        tc("gears", "needs_a_lathe", Outcome::Skipped, Some(0)),
    ];
    let files = [PathBuf::from("target/nextest/default/junit.xml")];
    let run = NewRun {
        files: &files,
        ..RUN
    };
    let recorded = store.record_run(&run, &cases).unwrap();
    assert_eq!(recorded.results, 4);

    let (commit, source, at): (Option<String>, String, String) = store
        .conn
        .query_row(
            "SELECT commit_sha, source, ingested_at FROM runs WHERE id = ?1",
            [recorded.run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(commit.as_deref(), Some("0123abcd"));
    assert_eq!(source, "nextest");
    assert!(at.ends_with('Z') && at.contains('T'), "{at}");
    assert_eq!(
        run_files(&store, recorded.run_id),
        [(0, b"target/nextest/default/junit.xml".to_vec())]
    );

    assert_eq!(
        rows(&store, recorded.run_id),
        [
            row("gears", "needs_a_lathe", "skipped", Some(0)),
            row("widgets", "parses_a_sprocket", "passed", Some(4)),
            row("widgets", "rejects_a_bent_cog", "failed", Some(12)),
            row("widgets", "spins", "error", None),
        ]
    );
}

#[test]
fn every_file_of_a_run_is_recorded_in_order() {
    let mut store = Store::open_in_memory().unwrap();
    // A newline is legal in a file name; it no longer collides with a
    // separator.
    let files = [
        PathBuf::from("b.xml"),
        PathBuf::from("dir/a\nb.xml"),
        PathBuf::from("a.xml"),
    ];
    let run = NewRun {
        files: &files,
        ..RUN
    };
    let id = store.record_run(&run, &[]).unwrap().run_id;
    assert_eq!(
        run_files(&store, id),
        [
            (0, b"b.xml".to_vec()),
            (1, b"dir/a\nb.xml".to_vec()),
            (2, b"a.xml".to_vec()),
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_non_utf8_path_is_recorded_byte_for_byte() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut store = Store::open_in_memory().unwrap();
    let raw = b"results/\xff\xfe-junit.xml";
    let files = [PathBuf::from(OsStr::from_bytes(raw))];
    let run = NewRun {
        files: &files,
        ..RUN
    };
    let id = store.record_run(&run, &[]).unwrap().run_id;
    assert_eq!(run_files(&store, id), [(0, raw.to_vec())]);
}

#[test]
fn deleting_a_run_deletes_its_files_and_results() {
    let mut store = Store::open_in_memory().unwrap();
    let files = [PathBuf::from("a.xml"), PathBuf::from("b.xml")];
    let run = NewRun {
        files: &files,
        ..RUN
    };
    let id = store
        .record_run(&run, &[tc("s", "t", Outcome::Passed, None)])
        .unwrap()
        .run_id;
    store
        .conn
        .execute("DELETE FROM runs WHERE id = ?1", [id])
        .unwrap();
    assert_eq!(count(&store, "run_files"), 0);
    assert_eq!(count(&store, "results"), 0);
}

#[test]
fn tests_are_shared_across_runs() {
    let mut store = Store::open_in_memory().unwrap();
    let first = store
        .record_run(
            &RUN,
            &[
                tc("widgets", "a", Outcome::Passed, Some(1)),
                tc("widgets", "b", Outcome::Passed, Some(2)),
            ],
        )
        .unwrap();
    let second = store
        .record_run(
            &RUN,
            &[
                tc("widgets", "b", Outcome::Failed, Some(3)),
                tc("widgets", "c", Outcome::Passed, Some(4)),
            ],
        )
        .unwrap();
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(count(&store, "runs"), 2);
    assert_eq!(count(&store, "tests"), 3);
    assert_eq!(count(&store, "results"), 4);
    assert_eq!(
        rows(&store, second.run_id),
        [
            row("widgets", "b", "failed", Some(3)),
            row("widgets", "c", "passed", Some(4)),
        ]
    );
}

#[test]
fn same_name_in_different_suites_is_a_different_test() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .record_run(
            &RUN,
            &[
                tc("gears", "turns", Outcome::Passed, None),
                tc("widgets", "turns", Outcome::Passed, None),
            ],
        )
        .unwrap();
    assert_eq!(count(&store, "tests"), 2);
}

#[test]
fn control_bytes_in_suite_and_name_are_stored_raw() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .record_run(&RUN, &[tc("s\u{1b}[31m", "t\0u", Outcome::Passed, None)])
        .unwrap()
        .run_id;
    assert_eq!(
        rows(&store, id),
        [row("s\u{1b}[31m", "t\0u", "passed", None)]
    );
}

#[test]
fn a_repeated_test_in_one_run_is_merged() {
    let mut store = Store::open_in_memory().unwrap();
    let recorded = store
        .record_run(
            &RUN,
            &[
                tc("w", "flaky", Outcome::Passed, Some(5)),
                tc("w", "steady", Outcome::Passed, None),
                tc("w", "flaky", Outcome::Failed, Some(7)),
                tc("w", "steady", Outcome::Skipped, None),
                tc("w", "half_timed", Outcome::Skipped, None),
                tc("w", "half_timed", Outcome::Error, Some(9)),
                // The top two ranks of the severity ladder, the only pairing
                // an inverted failed/error order would slip through.
                tc("w", "crashed", Outcome::Error, Some(1)),
                tc("w", "crashed", Outcome::Failed, Some(2)),
                // Appears once, alongside the duplicated identities above:
                // merging them must not touch this row.
                tc("w", "solo", Outcome::Passed, Some(3)),
            ],
        )
        .unwrap();
    assert_eq!(recorded.results, 5);
    assert_eq!(
        rows(&store, recorded.run_id),
        [
            row("w", "crashed", "failed", Some(3)),
            row("w", "flaky", "failed", Some(12)),
            row("w", "half_timed", "error", Some(9)),
            row("w", "solo", "passed", Some(3)),
            row("w", "steady", "passed", None),
        ]
    );
}

#[test]
fn commit_is_optional() {
    let mut store = Store::open_in_memory().unwrap();
    let run = NewRun {
        commit: None,
        ..RUN
    };
    let id = store.record_run(&run, &[]).unwrap().run_id;
    let commit: Option<String> = store
        .conn
        .query_row("SELECT commit_sha FROM runs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(commit, None);
}

#[test]
fn a_failed_insert_writes_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute_batch(
            "CREATE TEMP TRIGGER poison BEFORE INSERT ON tests
             WHEN NEW.name = 'poison' BEGIN SELECT RAISE(ABORT, 'poisoned'); END;",
        )
        .unwrap();
    let cases = [
        tc("s", "fine", Outcome::Passed, None),
        tc("s", "poison", Outcome::Passed, None),
    ];
    let files = [PathBuf::from("a.xml")];
    let run = NewRun {
        files: &files,
        ..RUN
    };
    let err = store.record_run(&run, &cases).unwrap_err();
    // The cause is escaped into the message, not chained, so it prints once.
    assert!(std::error::Error::source(&err).is_none(), "{err:?}");
    assert!(
        matches!(&err, StoreError::Sqlite { sqlite, .. } if sqlite.to_string().contains("poisoned")),
        "{err:?}"
    );
    assert_eq!(err.to_string().matches("poisoned").count(), 1, "{err}");
    assert_eq!(count(&store, "runs"), 0);
    assert_eq!(count(&store, "run_files"), 0);
    assert_eq!(count(&store, "tests"), 0);
    assert_eq!(count(&store, "results"), 0);
}

#[test]
fn schema_rejects_an_unknown_outcome_and_dangling_ids() {
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO runs (id, source) VALUES (1, 'x');
             INSERT INTO tests (id, suite, name) VALUES (1, 's', 't');",
        )
        .unwrap();
    let insert = |run: i64, test: i64, outcome: &str| {
        store.conn.execute(
            "INSERT INTO results (run_id, test_id, outcome) VALUES (?1, ?2, ?3)",
            params![run, test, outcome],
        )
    };
    let err = insert(1, 1, "flaky").unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    let err = insert(1, 99, "passed").unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    let err = insert(99, 1, "passed").unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    insert(1, 1, "passed").unwrap();
    let err = insert(1, 1, "failed").unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "{err}");
}

fn run_from(source: &'static str) -> NewRun<'static> {
    NewRun {
        commit: None,
        source,
        files: &[],
    }
}

/// The history as (run id, source, [(suite, name, outcome, ms)]), so tests
/// need not know test ids.
fn named(history: &History) -> Vec<(i64, String, Vec<Row>)> {
    history
        .runs
        .iter()
        .map(|run| {
            let results = run
                .results
                .iter()
                .map(|r| {
                    let (suite, name) = &history.tests[&r.test_id];
                    (
                        suite.as_raw().to_owned(),
                        name.as_raw().to_owned(),
                        r.outcome.as_str().to_owned(),
                        r.duration_ms,
                    )
                })
                .collect();
            (run.id, run.source.clone(), results)
        })
        .collect()
}

#[test]
fn history_reads_every_run_oldest_first_with_its_results() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .record_run(
            &run_from("ci"),
            &[
                tc("s", "b", Outcome::Failed, Some(3)),
                tc("s", "a", Outcome::Passed, None),
            ],
        )
        .unwrap();
    store
        .record_run(
            &run_from("local"),
            &[
                tc("s", "a", Outcome::Error, Some(7)),
                tc("t", "c", Outcome::Skipped, Some(0)),
            ],
        )
        .unwrap();
    let history = store.history(&[]).unwrap();
    assert_eq!(
        named(&history),
        [
            (
                1,
                "ci".into(),
                vec![
                    row("s", "b", "failed", Some(3)),
                    row("s", "a", "passed", None)
                ]
            ),
            (
                2,
                "local".into(),
                vec![
                    row("s", "a", "error", Some(7)),
                    row("t", "c", "skipped", Some(0))
                ]
            ),
        ]
    );
    // Results within a run are in test id order.
    let ids: Vec<i64> = history.runs[1].results.iter().map(|r| r.test_id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
}

#[test]
fn history_keeps_only_the_given_sources() {
    let mut store = Store::open_in_memory().unwrap();
    for source in ["ci", "local", "nightly", "ci"] {
        store
            .record_run(
                &run_from(source),
                &[tc("s", source, Outcome::Passed, Some(1))],
            )
            .unwrap();
    }
    let runs = |sources: &[&str]| -> Vec<(i64, String)> {
        let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
        store
            .history(&sources)
            .unwrap()
            .runs
            .iter()
            .map(|r| (r.id, r.source.clone()))
            .collect()
    };
    assert_eq!(runs(&["ci"]), [(1, "ci".into()), (4, "ci".into())]);
    assert_eq!(
        runs(&["nightly", "ci"]),
        [(1, "ci".into()), (3, "nightly".into()), (4, "ci".into())]
    );
    assert_eq!(runs(&["absent"]), []);
    assert_eq!(runs(&[]).len(), 4);
    let only_ci = store.history(&["ci".into()]).unwrap();
    assert!(only_ci.runs.iter().all(|r| r.results.len() == 1));
}

#[test]
fn history_of_an_empty_store_is_empty() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.history(&[]).unwrap().runs, []);
}

#[test]
fn a_read_only_open_of_a_missing_file_is_none_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/history.sqlite");
    assert!(Store::open_read_only(&path).unwrap().is_none());
    assert!(!dir.path().join("sub").exists());
}

#[cfg(unix)]
#[test]
fn a_read_only_open_of_a_file_that_cannot_be_checked_is_an_error_not_none() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    let path = locked.join("history.sqlite");
    {
        let mut store = Store::open(&path).unwrap();
        store
            .record_run(&RUN, &[tc("s", "t", Outcome::Passed, Some(1))])
            .unwrap();
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let checkable = path.try_exists().is_ok();
    let opened = Store::open_read_only(&path);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if checkable {
        // Running as root: permissions are not enforced, nothing to test.
        return;
    }
    let err = opened.unwrap_err();
    assert!(matches!(err, StoreError::Sqlite { .. }), "{err:?}");
}

#[test]
fn a_read_only_open_of_an_empty_sqlite_file_is_none_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    std::fs::write(&path, "").unwrap();
    assert!(Store::open_read_only(&path).unwrap().is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"");
}

#[test]
fn a_read_only_open_reads_a_store_without_writing_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    {
        let mut store = Store::open(&path).unwrap();
        store
            .record_run(&RUN, &[tc("s", "t", Outcome::Passed, Some(1))])
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let store = Store::open_read_only(&path).unwrap().unwrap();
    let history = store.history(&[]).unwrap();
    assert_eq!(named(&history)[0].2, [row("s", "t", "passed", Some(1))]);
    // The connection itself refuses writes.
    assert!(store
        .conn
        .execute("INSERT INTO runs (source) VALUES ('x')", [])
        .is_err());
    drop(store);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file was modified");
}

#[test]
fn a_read_only_open_refuses_what_open_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let foreign = dir.path().join("foreign.sqlite");
    let before = foreign_db(&foreign, 0x1234_5678);
    let err = Store::open_read_only(&foreign).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::NotAStore {
                found: 0x1234_5678,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), before);

    for (version, newer) in [(SCHEMA_VERSION + 1, true), (-1, false)] {
        let path = dir.path().join(format!("v{version}.sqlite"));
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        drop(conn);
        let err = Store::open_read_only(&path).unwrap_err();
        if newer {
            assert!(matches!(err, StoreError::TooNew { .. }), "{err:?}");
        } else {
            assert!(
                matches!(err, StoreError::BadVersion { found: -1, .. }),
                "{err:?}"
            );
        }
    }

    for special in [":memory:", ""] {
        let err = Store::open_read_only(Path::new(special)).unwrap_err();
        assert!(matches!(err, StoreError::MemoryPath { .. }), "{err:?}");
    }
}
