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
    file_name: "target/nextest/default/junit.xml",
};

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
        ["results", "runs", "tests"]
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
    let recorded = store.record_run(&RUN, &cases).unwrap();
    assert_eq!(recorded.results, 4);

    let (commit, source, file, at): (Option<String>, String, String, String) = store
        .conn
        .query_row(
            "SELECT commit_sha, source, file_name, ingested_at FROM runs WHERE id = ?1",
            [recorded.run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(commit.as_deref(), Some("0123abcd"));
    assert_eq!(source, "nextest");
    assert_eq!(file, "target/nextest/default/junit.xml");
    assert!(at.ends_with('Z') && at.contains('T'), "{at}");

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
            ],
        )
        .unwrap();
    assert_eq!(recorded.results, 3);
    assert_eq!(
        rows(&store, recorded.run_id),
        [
            row("w", "flaky", "failed", Some(12)),
            row("w", "half_timed", "error", Some(9)),
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
    let err = store.record_run(&RUN, &cases).unwrap_err();
    let cause = std::error::Error::source(&err).expect("sqlite error is the source");
    assert!(cause.to_string().contains("poisoned"), "{cause}");
    assert!(!err.to_string().contains("poisoned"), "{err}");
    assert_eq!(count(&store, "runs"), 0);
    assert_eq!(count(&store, "tests"), 0);
    assert_eq!(count(&store, "results"), 0);
}

#[test]
fn schema_rejects_an_unknown_outcome_and_dangling_ids() {
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute_batch(
            "INSERT INTO runs (id, source, file_name) VALUES (1, 'x', 'y');
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
