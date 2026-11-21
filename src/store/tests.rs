use std::path::PathBuf;

use super::*;

fn tc(suite: &str, name: &str, outcome: Outcome, ms: Option<i64>) -> TestCase {
    TestCase::new(Tainted::raw(suite), Tainted::raw(name), outcome, ms)
}

const RUN: NewRun<'static> = NewRun {
    commit: None,
    source: "nextest",
    files: &[],
    partial: false,
    branch: None,
    tag: None,
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
    assert_eq!(SCHEMA_VERSION, 5);
}

#[test]
fn schema_has_runs_tests_and_results() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name"
        ),
        [
            "report_memberships",
            "results",
            "run_files",
            "runs",
            "tests"
        ]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT name FROM pragma_table_info('runs') ORDER BY cid"
        ),
        [
            "id",
            "commit_sha",
            "source",
            "ingested_at",
            "partial",
            "branch",
            "tag",
            "history_id"
        ]
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
            "SELECT name FROM pragma_table_info('report_memberships') ORDER BY cid"
        ),
        ["run_id", "namespace", "fingerprint_version", "digest"]
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
        matches!(err, StoreError::CreateDir { ref path, .. } if *path == Tainted::from(blocker.join("sub").as_path())),
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
    let commit: CommitId = "0123abcd".parse().unwrap();
    let run = NewRun {
        commit: Some(&commit),
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
fn merge_duplicates_preserves_first_order_saturation_and_unknown_durations() {
    let cases = [
        tc("z", "first", Outcome::Skipped, Some(i64::MAX)),
        tc("a", "second", Outcome::Error, None),
        tc("z", "first", Outcome::Passed, Some(1)),
        tc("a", "second", Outcome::Passed, None),
        tc("z", "third", Outcome::Passed, Some(8)),
        tc("z", "third", Outcome::Failed, None),
    ];
    assert_eq!(
        merge_duplicates(&cases),
        [
            tc("z", "first", Outcome::Passed, Some(i64::MAX)),
            tc("a", "second", Outcome::Error, None),
            tc("z", "third", Outcome::Failed, Some(8)),
        ]
    );
    assert_eq!(cases[0].outcome(), Outcome::Skipped);
    assert_eq!(cases[2].duration_ms(), Some(1));
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
        partial: false,
        branch: None,
        tag: None,
    }
}

/// The history as (run id, source, [(suite, name, outcome, ms)]), so tests
/// need not know test ids.
fn named(history: &History) -> Vec<(i64, String, Vec<Row>)> {
    history
        .runs()
        .iter()
        .map(|run| {
            let results = run
                .results()
                .iter()
                .map(|r| {
                    let (suite, name) = &history.tests()[&r.test_id()];
                    (
                        suite.as_raw().to_owned(),
                        name.as_raw().to_owned(),
                        r.outcome().as_str().to_owned(),
                        r.duration_ms(),
                    )
                })
                .collect();
            (run.id(), run.source().to_owned(), results)
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
    let ids: Vec<i64> = history.runs()[1]
        .results()
        .iter()
        .map(|r| r.test_id())
        .collect();
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
            .runs()
            .iter()
            .map(|r| (r.id(), r.source().to_owned()))
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
    assert!(only_ci.runs().iter().all(|r| r.results().len() == 1));
}

#[test]
fn history_of_an_empty_store_is_empty() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.history(&[]).unwrap().runs(), []);
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

#[test]
fn a_commit_id_is_7_to_64_hex_digits() {
    let sha1 = "0123456789abcdef0123456789abcdef01234567";
    let sha256 = "0123456789abcdef".repeat(4);
    for ok in ["0123abc", "deadbeef", sha1, &sha256] {
        assert_eq!(ok.parse::<CommitId>().unwrap().as_str(), ok);
    }
}

#[test]
fn a_commit_id_refuses_too_few_or_too_many_digits() {
    for bad in ["", "0", "0123ab", &"a".repeat(65)] {
        assert_eq!(bad.parse::<CommitId>(), Err(CommitIdError), "{bad:?}");
    }
}

#[test]
fn a_commit_id_refuses_what_is_not_hex() {
    for bad in [
        "0123abg",
        "HEAD~1234",
        "v1.2.3-45",
        " 0123abcd",
        "0123abcd\n",
        "0x123abcd",
        "０１２３４５６",
    ] {
        assert_eq!(bad.parse::<CommitId>(), Err(CommitIdError), "{bad:?}");
    }
}

#[test]
fn a_commit_id_is_kept_in_lower_case() {
    let id: CommitId = "0123ABCDef".parse().unwrap();
    assert_eq!(id.as_str(), "0123abcdef");
    assert_eq!(id.to_string(), "0123abcdef");
    assert_eq!(id, "0123abcdef".parse().unwrap());
}

#[test]
fn a_sqlite_error_keeps_its_message_raw_and_displays_it_escaped_once() {
    // A store whose schema text holds a terminal escape and a backslash:
    // SQLite's "malformed database schema" message quotes it from the file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        r"CREATE TABLE t(x);
         PRAGMA writable_schema = ON;
         UPDATE sqlite_schema SET name = 't' || char(27) || '[31mRED\',
             sql = 'CREATE TABLE t(x) ' || char(27) || '[31mRED\';",
    )
    .unwrap();
    drop(conn);
    let err = Store::open(&path).unwrap_err();
    let StoreError::Sqlite { message, .. } = &err else {
        panic!("not a SQLite error: {err:?}");
    };
    assert!(message.as_raw().contains("\u{1b}[31mRED\\"), "{message:?}");
    let shown = err.to_string();
    assert!(shown.contains(r"\u{1b}[31mRED\\"), "{shown:?}");
    assert!(!shown.contains(r"\\u{1b}"), "escaped twice: {shown:?}");
    assert!(!shown.contains('\u{1b}'), "{shown:?}");
}

#[test]
fn every_store_error_escapes_its_path_and_message_exactly_once() {
    let path = || Tainted::from(std::path::Path::new("\u{1b}[31mevil.sqlite"));
    let io = || std::io::Error::other("io");
    for err in [
        StoreError::CreateDir {
            path: path(),
            source: io(),
        },
        StoreError::Sqlite {
            path: path(),
            message: Tainted::raw("\u{1b}[31mevil"),
            sqlite: rusqlite::Error::QueryReturnedNoRows,
        },
        StoreError::TooNew {
            path: path(),
            found: 9,
            supported: 1,
        },
        StoreError::NotAStore {
            path: path(),
            found: 1,
            expected: 2,
        },
        StoreError::BadVersion {
            path: path(),
            found: 9,
        },
        StoreError::MemoryPath { path: path() },
    ] {
        let text = err.to_string();
        assert!(text.contains(r"\u{1b}[31mevil"), "{text:?}");
        assert!(!text.contains(r"\\u{1b}"), "escaped twice: {text:?}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}

#[test]
fn an_unknown_outcome_in_the_store_is_shown_escaped_exactly_once() {
    // The outcome comes from the store file. It goes into the error raw
    // and is escaped once, when the error is displayed.
    let mut store = Store::open_in_memory().unwrap();
    store
        .record_run(&RUN, &[tc("s", "t", Outcome::Passed, Some(1))])
        .unwrap();
    store
        .conn
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE results SET outcome = char(27) || '[31mevil';",
        )
        .unwrap();
    let shown = store.history(&[]).unwrap_err().to_string();
    assert!(shown.contains(r#""\u{1b}[31mevil""#), "{shown:?}");
    assert!(!shown.contains(r"\\u{1b}"), "escaped twice: {shown:?}");
    assert!(!shown.contains('\u{1b}'), "{shown:?}");
}

#[test]
fn an_unknown_outcome_in_the_store_names_its_cause_once() {
    // rusqlite's message for a conversion failure already includes the
    // cause, and `source()` returns that cause again.
    let mut store = Store::open_in_memory().unwrap();
    store
        .record_run(&RUN, &[tc("s", "t", Outcome::Passed, Some(1))])
        .unwrap();
    store
        .conn
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE results SET outcome = 'X';",
        )
        .unwrap();
    let shown = store.history(&[]).unwrap_err().to_string();
    assert_eq!(
        shown.matches(r#"unknown outcome "X""#).count(),
        1,
        "{shown:?}"
    );
}

/// A store at schema version 1, as greenshift wrote it before runs could
/// be partial, holding one run of one passing test.
fn version_1_store(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (id, commit_sha, source) VALUES (1, 'abc1234', 'nextest');
         INSERT INTO tests (id, suite, name) VALUES (1, 's', 't');
         INSERT INTO results (run_id, test_id, outcome, duration_ms)
         VALUES (1, 1, 'passed', 5);",
    )
    .unwrap();
}

fn user_version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn a_version_1_store_is_migrated_and_its_runs_are_not_partial() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    version_1_store(&path);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let history = store.history(&[]).unwrap();
    assert_eq!(
        named(&history),
        [(
            1,
            "nextest".to_string(),
            vec![row("s", "t", "passed", Some(5))]
        )]
    );
    assert!(!history.runs()[0].partial());
    assert_eq!(run_rows(&store)[0].4, None, "no branch is known");
    assert_eq!(run_rows(&store)[0].5, None, "no tag is known");
    // New runs are written alongside the old one.
    let partial = NewRun {
        partial: true,
        ..RUN
    };
    store.record_run(&partial, &[]).unwrap();
    let flags: Vec<bool> = store
        .history(&[])
        .unwrap()
        .runs()
        .iter()
        .map(|r| r.partial())
        .collect();
    assert_eq!(flags, [false, true]);
}

#[test]
fn a_read_only_open_reads_a_version_1_store_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    version_1_store(&path);
    let store = Store::open_read_only(&path).unwrap().unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
    let history = store.history(&[]).unwrap();
    assert_eq!(
        named(&history),
        [(
            1,
            "nextest".to_string(),
            vec![row("s", "t", "passed", Some(5))]
        )]
    );
    assert!(!history.runs()[0].partial());
    drop(store);
    assert_eq!(user_version(&path), 1);
}

#[test]
fn history_reads_back_whether_each_run_is_partial() {
    let mut store = Store::open_in_memory().unwrap();
    for partial in [false, true, false] {
        store.record_run(&NewRun { partial, ..RUN }, &[]).unwrap();
    }
    let flags: Vec<bool> = store
        .history(&[])
        .unwrap()
        .runs()
        .iter()
        .map(|r| r.partial())
        .collect();
    assert_eq!(flags, [false, true, false]);
}

#[test]
fn runs_partial_holds_only_0_or_1() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .conn
        .execute("INSERT INTO runs (source, partial) VALUES ('x', 2)", [])
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
}

#[test]
fn a_run_has_a_branch_or_a_tag_never_both() {
    let store = Store::open_in_memory().unwrap();
    for (branch, tag) in [(None, None), (Some("main"), None), (None, Some("v1"))] {
        store
            .conn
            .execute(
                "INSERT INTO runs (source, branch, tag) VALUES ('x', ?1, ?2)",
                params![branch, tag],
            )
            .unwrap();
    }
    let err = store
        .conn
        .execute(
            "INSERT INTO runs (source, branch, tag) VALUES ('x', 'main', 'v1')",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
}

/// The same constraint, reached the way real code reaches it: through
/// [`Store::record_run`], not a raw `INSERT`. `NewRun` can represent a run
/// with both a branch and a tag (the CHECK constraint is its only guard),
/// so this proves the guard holds at that boundary too: the run is
/// refused, no row is written, and the store still records the next run.
#[test]
fn record_run_rejects_a_run_with_both_a_branch_and_a_tag() {
    let mut store = Store::open_in_memory().unwrap();
    let both = NewRun {
        branch: Some("main"),
        tag: Some("v1"),
        ..RUN
    };
    let err = store.record_run(&both, &[]).unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    assert_eq!(count(&store, "runs"), 0, "the rejected run left no row");

    let recorded = store.record_run(&RUN, &[]).unwrap();
    assert_eq!(recorded.run_id, 1, "the next run is recorded as the first");
}

/// A store at schema version 2, as greenshift wrote it before runs had a
/// branch, holding a whole run and a partial one.
fn version_2_store(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(MIGRATIONS[0]).unwrap();
    conn.execute_batch(MIGRATIONS[1]).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (id, commit_sha, source, partial)
         VALUES (1, 'abc1234', 'nextest', 0), (2, NULL, 'ci', 1);
         INSERT INTO tests (id, suite, name) VALUES (1, 's', 't');
         INSERT INTO results (run_id, test_id, outcome, duration_ms)
         VALUES (1, 1, 'passed', 5), (2, 1, 'failed', 7);",
    )
    .unwrap();
}

/// (id, commit_sha, source, partial, branch, tag) of every run, oldest first.
type RunRow = (
    i64,
    Option<String>,
    String,
    bool,
    Option<String>,
    Option<String>,
);

fn run_rows(store: &Store) -> Vec<RunRow> {
    let mut stmt = store
        .conn
        .prepare("SELECT id, commit_sha, source, partial, branch, tag FROM runs ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn a_version_2_store_is_migrated_and_its_runs_have_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    version_2_store(&path);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(
        run_rows(&store),
        [
            (
                1,
                Some("abc1234".into()),
                "nextest".into(),
                false,
                None,
                None
            ),
            (2, None, "ci".into(), true, None, None),
        ]
    );
    assert_eq!(
        rows(&store, 2),
        [row("s", "t", "failed", Some(7))],
        "results are kept"
    );
}

#[test]
fn a_read_only_open_reads_a_version_2_store_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.sqlite");
    version_2_store(&path);
    let store = Store::open_read_only(&path).unwrap().unwrap();
    assert_eq!(store.schema_version().unwrap(), 2);
    let flags: Vec<bool> = store
        .history(&[])
        .unwrap()
        .runs()
        .iter()
        .map(|r| r.partial())
        .collect();
    assert_eq!(flags, [false, true]);
    drop(store);
    assert_eq!(user_version(&path), 2);
}

#[test]
fn a_run_is_recorded_with_its_branch_or_none() {
    let mut store = Store::open_in_memory().unwrap();
    for branch in [Some("main"), None, Some("feature/x")] {
        store.record_run(&NewRun { branch, ..RUN }, &[]).unwrap();
    }
    let branches: Vec<Option<String>> = run_rows(&store).into_iter().map(|r| r.4).collect();
    assert_eq!(
        branches,
        [Some("main".into()), None, Some("feature/x".into())]
    );
}

#[test]
fn a_run_is_recorded_with_its_tag_or_none() {
    let mut store = Store::open_in_memory().unwrap();
    for tag in [Some("v1.2.0"), None, Some("v2.0.0-rc1")] {
        store.record_run(&NewRun { tag, ..RUN }, &[]).unwrap();
    }
    let tags: Vec<Option<String>> = run_rows(&store).into_iter().map(|r| r.5).collect();
    assert_eq!(
        tags,
        [Some("v1.2.0".into()), None, Some("v2.0.0-rc1".into())]
    );
}
