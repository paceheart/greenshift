//! Runs the `greenshift ingest` binary against a temporary store.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

/// The binary, without the CI branch and tag variables of the environment
/// this test runs in, so a run records neither unless a test sets one.
fn greenshift() -> Command {
    let mut cmd = Command::cargo_bin("greenshift").expect("binary builds");
    for var in greenshift::cli::CI_REF_VARS {
        cmd.env_remove(var);
    }
    cmd
}

fn ingest(db: &Path, files: &[PathBuf], extra: &[&str]) -> assert_cmd::assert::Assert {
    greenshift()
        .arg("ingest")
        .arg("--db")
        .arg(db)
        .args(extra)
        .args(files)
        .assert()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn result(conn: &Connection, suite: &str, name: &str) -> (String, Option<i64>) {
    conn.query_row(
        "SELECT r.outcome, r.duration_ms FROM results r JOIN tests t ON t.id = r.test_id
         WHERE t.suite = ?1 AND t.name = ?2",
        [suite, name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn both_producers_in_one_invocation_make_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let files = [fixture("nextest.xml"), fixture("vaelii-run-vars.xml")];
    ingest(&db, &files, &["--commit", "0123abcd", "--source", "ci"])
        .success()
        .stdout(predicate::str::contains(
            "recorded run 1 with 13 tests from 2 files",
        ));

    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "runs"), 1);
    assert_eq!(count(&conn, "tests"), 13);
    assert_eq!(count(&conn, "results"), 13);

    let (commit, source, partial, version): (Option<String>, String, bool, i64) = conn
        .query_row(
            "SELECT commit_sha, source, partial,
                    (SELECT user_version FROM pragma_user_version)
             FROM runs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(commit.as_deref(), Some("0123abcd"));
    assert_eq!(source, "ci");
    assert!(!partial);
    assert_eq!(version, greenshift::store::SCHEMA_VERSION);
    let recorded: Vec<Vec<u8>> = conn
        .prepare("SELECT path FROM run_files ORDER BY position")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let given: Vec<Vec<u8>> = files
        .iter()
        .map(|f| f.as_os_str().as_encoded_bytes().to_vec())
        .collect();
    assert_eq!(recorded, given);

    let outcomes: Vec<(String, i64)> = conn
        .prepare("SELECT outcome, count(*) FROM results GROUP BY outcome ORDER BY outcome")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        outcomes,
        [
            ("error".into(), 3),
            ("failed".into(), 2),
            ("passed".into(), 5),
            ("skipped".into(), 3),
        ]
    );

    assert_eq!(
        result(&conn, "sprocket-works", "widgets::rejects_a_bent_cog"),
        ("failed".into(), Some(12))
    );
    assert_eq!(
        result(&conn, "sprocket-works::assembly", "assembles_a_gearbox"),
        ("error".into(), Some(1457))
    );
    assert_eq!(
        result(&conn, "widgets.flywheel-test", "stores-momentum"),
        ("skipped".into(), Some(0))
    );
    assert_eq!(
        result(&conn, "widgets.sprocket-test", "spins-under-load"),
        ("error".into(), Some(1250))
    );
}

#[test]
fn commit_is_optional_and_source_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("nextest.xml")], &[])
        .success()
        .stdout(predicate::str::contains("from 1 file into"));
    let conn = Connection::open(&db).unwrap();
    let (commit, source): (Option<String>, String) = conn
        .query_row("SELECT commit_sha, source FROM runs", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(commit, None);
    assert_eq!(source, "junit");
}

#[test]
fn a_single_test_is_reported_in_the_singular() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let one = dir.path().join("one.xml");
    std::fs::write(
        &one,
        r#"<testsuite name="s"><testcase name="t"/></testsuite>"#,
    )
    .unwrap();
    ingest(&db, &[one], &[])
        .success()
        .stdout(predicate::str::contains("with 1 test from 1 file into"));
}

#[test]
fn default_db_is_created_under_dot_greenshift() {
    let dir = tempfile::tempdir().unwrap();
    greenshift()
        .current_dir(dir.path())
        .arg("ingest")
        .arg(fixture("vaelii-run-vars.xml"))
        .assert()
        .success();
    let db = dir.path().join(".greenshift/history.sqlite");
    let conn = Connection::open(db).unwrap();
    assert_eq!(count(&conn, "results"), 7);
}

#[test]
fn different_commits_are_new_runs_sharing_test_identities() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "aaaaaaa"]).success();
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "bbbbbbb"])
        .success()
        .stdout(predicate::str::contains("recorded run 2"));
    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "runs"), 2);
    assert_eq!(count(&conn, "tests"), 6);
    assert_eq!(count(&conn, "results"), 12);
}

#[test]
fn malformed_xml_fails_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sub/history.sqlite");
    ingest(
        &db,
        &[fixture("nextest.xml"), fixture("malformed.xml")],
        &[],
    )
    .failure()
    .code(1)
    .stdout(predicate::str::is_empty())
    .stderr(
        predicate::str::contains("malformed.xml").and(predicate::str::contains("malformed XML")),
    );
    assert!(!db.exists());
    assert!(!dir.path().join("sub").exists());
}

#[test]
fn malformed_xml_leaves_an_existing_store_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("vaelii-run-vars.xml")], &[]).success();
    ingest(
        &db,
        &[fixture("nextest.xml"), fixture("malformed.xml")],
        &[],
    )
    .failure();
    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "runs"), 1);
    assert_eq!(count(&conn, "tests"), 7);
    assert_eq!(count(&conn, "results"), 7);
}

#[test]
fn a_database_that_is_not_a_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('mine');")
        .unwrap();
    drop(conn);
    let before = std::fs::read(&db).unwrap();
    let out = ingest(&db, &[fixture("nextest.xml")], &[])
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains(&format!(
            "{} is not a greenshift history store",
            db.display()
        )),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&db).unwrap(), before, "file was modified");
}

#[test]
fn missing_file_fails_and_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let missing = dir.path().join("nope.xml");
    let out = ingest(&db, std::slice::from_ref(&missing), &[])
        .failure()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    // One line: the file, what failed, and the OS cause, each said once.
    assert_eq!(
        stderr,
        format!(
            "greenshift: error: {}: could not read file: No such file or directory (os error 2)\n",
            missing.display()
        )
    );
    assert!(!db.exists());
}

#[test]
fn non_junit_and_empty_files_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let html = dir.path().join("page.xml");
    std::fs::write(&html, "<html><body/></html>").unwrap();
    let empty = dir.path().join("empty.xml");
    std::fs::write(&empty, "").unwrap();
    ingest(&db, &[html], &[])
        .failure()
        .stderr(predicate::str::contains("not JUnit XML"));
    ingest(&db, &[empty], &[])
        .failure()
        .stderr(predicate::str::contains("no root element"));
    assert!(!db.exists());
}

#[test]
fn ingest_without_files_is_a_usage_error() {
    greenshift()
        .arg("ingest")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("JUNIT_XML"));
}

#[test]
fn ingest_help_lists_its_options() {
    greenshift()
        .args(["ingest", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--db <PATH>")
                .and(predicate::str::contains("--commit <SHA>"))
                .and(predicate::str::contains("--source <NAME>"))
                .and(predicate::str::contains("--branch <NAME>"))
                .and(predicate::str::contains("--tag <NAME>"))
                .and(predicate::str::contains(".greenshift/history.sqlite")),
        );
}

// `?` is not allowed in Windows file names.
#[cfg(unix)]
#[test]
fn a_db_path_that_looks_like_a_sqlite_uri_is_an_ordinary_file() {
    // The bundled SQLite parses `file:` names as URIs; these would otherwise
    // open a private in-memory database and record nothing to disk.
    for name in ["file::memory:", "file:history.sqlite?mode=memory"] {
        let dir = tempfile::tempdir().unwrap();
        greenshift()
            .current_dir(dir.path())
            .args(["ingest", "--db", name])
            .arg(fixture("nextest.xml"))
            .assert()
            .success();
        let conn = Connection::open(dir.path().join(name)).unwrap();
        assert_eq!(count(&conn, "runs"), 1, "{name}");
    }
}

#[test]
fn a_db_that_cannot_be_opened_is_named_as_given() {
    // SQLite's own error text repeats the path it was handed; only `file:`
    // names are rewritten, so an ordinary path is not shown as `./...`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("store")).unwrap();
    greenshift()
        .current_dir(dir.path())
        .args(["ingest", "--db", "store"])
        .arg(fixture("nextest.xml"))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("history store store:")
                .and(predicate::str::contains("./store").not()),
        );
}

#[test]
fn sqlite_errors_quoting_the_store_file_escape_control_bytes() {
    // A store whose schema text holds a terminal escape: SQLite's
    // "malformed database schema" message quotes it from the file.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE t(x);
         PRAGMA writable_schema = ON;
         UPDATE sqlite_schema SET name = 't' || char(27) || '[31mRED',
             sql = 'CREATE TABLE t(x) ' || char(27) || '[31mRED';",
    )
    .unwrap();
    drop(conn);
    ingest(&db, &[fixture("nextest.xml")], &[])
        .failure()
        .stderr(
            predicate::str::contains("malformed database schema")
                .and(predicate::str::contains(r"\u{1b}[31mRED"))
                .and(predicate::str::contains("\u{1b}").not()),
        );
}

#[test]
fn a_bad_commit_is_a_usage_error_that_says_what_a_commit_is() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    for bad in ["abc12", "not-a-sha", "0123abcdg", "HEAD"] {
        ingest(&db, &[fixture("nextest.xml")], &["--commit", bad])
            .failure()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains("--commit")
                    .and(predicate::str::contains("7 to 64 hex digits")),
            );
    }
    assert!(!db.exists());
}

#[test]
fn a_bad_commit_prints_exactly_one_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "HEAD"])
        .failure()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::eq(
            "error: invalid value 'HEAD' for '--commit <SHA>': a commit is 7 to 64 hex digits\n\
             \n\
             For more information, try '--help'.\n",
        ));
}

#[test]
fn a_bad_commit_is_shown_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(
        &db,
        &[fixture("nextest.xml")],
        &["--commit", "\u{1b}[31mRED\nx"],
    )
    .failure()
    .code(2)
    .stderr(
        predicate::str::contains(r"\u{1b}[31mRED\nx").and(predicate::str::contains("\u{1b}").not()),
    );
}

#[test]
fn a_commit_is_stored_in_lowercase() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "0123ABCDef"]).success();
    let conn = Connection::open(&db).unwrap();
    let commit: String = conn
        .query_row("SELECT commit_sha FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(commit, "0123abcdef");
}

fn partial_flags(conn: &Connection) -> Vec<bool> {
    conn.prepare("SELECT partial FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn partial_replay_retains_original_metadata_without_another_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let files = [fixture("nextest.xml")];
    ingest(&db, &files, &[])
        .success()
        .stdout(predicate::str::contains("recorded run 1 with"));
    ingest(&db, &files, &["--partial"])
        .success()
        .stdout(predicate::str::contains("already recorded"))
        .stderr(predicate::str::contains("metadata"));
    let conn = Connection::open(&db).unwrap();
    assert_eq!(partial_flags(&conn), [false]);
}

/// `ingest` of one fixture with `extra` arguments and, of the branch and tag
/// variables, only those in `env`, whatever the environment of this test.
fn ingest_in_env(db: &Path, extra: &[&str], env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
    greenshift()
        .envs(env.iter().copied())
        .arg("ingest")
        .arg("--db")
        .arg(db)
        .args(extra)
        .arg(fixture("nextest.xml"))
        .assert()
}

fn branches(db: &Path) -> Vec<Option<String>> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT branch FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn the_branch_flag_beats_the_ci_env() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let env = [("GITHUB_HEAD_REF", "feature"), ("GITHUB_REF_NAME", "main")];
    ingest_in_env(&db, &["--branch", "mine"], &env)
        .success()
        .stdout(predicate::str::contains("on branch mine"));
    assert_eq!(branches(&db), [Some("mine".to_owned())]);
}

#[test]
fn a_pull_request_head_ref_beats_the_ref_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let env = [
        ("GITHUB_HEAD_REF", "feature"),
        ("GITHUB_REF_NAME", "7/merge"),
    ];
    ingest_in_env(&db, &[], &env)
        .success()
        .stdout(predicate::str::contains("on branch feature"));
    assert_eq!(branches(&db), [Some("feature".to_owned())]);
}

#[test]
fn an_empty_head_ref_falls_back_to_the_ref_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let env = [("GITHUB_HEAD_REF", ""), ("GITHUB_REF_NAME", "main")];
    ingest_in_env(&db, &[], &env).success();
    ingest_in_env(&db, &[], &[("GITHUB_REF_NAME", "main")]).success();
    assert_eq!(branches(&db), [Some("main".to_owned())]);
}

#[test]
fn no_flag_and_no_ci_env_is_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest_in_env(&db, &[], &[])
        .success()
        .stdout(predicate::str::contains(" on branch").not());
    // An empty ref name is no branch either.
    ingest_in_env(
        &db,
        &[],
        &[("GITHUB_HEAD_REF", ""), ("GITHUB_REF_NAME", "")],
    )
    .success();
    assert_eq!(branches(&db), [None]);
}

#[test]
fn an_empty_branch_flag_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest_in_env(&db, &["--branch", ""], &[])
        .code(2)
        .stderr(predicate::str::contains(
            "a value is required for '--branch",
        ));
    assert!(!db.exists());
}

/// (branch, tag) of every run, oldest first.
fn refs(db: &Path) -> Vec<(Option<String>, Option<String>)> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT branch, tag FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn branch(name: &str) -> (Option<String>, Option<String>) {
    (Some(name.to_owned()), None)
}

fn tag(name: &str) -> (Option<String>, Option<String>) {
    (None, Some(name.to_owned()))
}

#[test]
fn a_tag_push_records_the_tag_and_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    // As GitHub Actions sets them on a push of the tag v1.2.0.
    let env = [
        ("GITHUB_HEAD_REF", ""),
        ("GITHUB_REF_NAME", "v1.2.0"),
        ("GITHUB_REF_TYPE", "tag"),
    ];
    ingest_in_env(&db, &[], &env)
        .success()
        .stdout(predicate::str::contains(" on tag v1.2.0\n"))
        .stdout(predicate::str::contains(" on branch").not());
    assert_eq!(refs(&db), [tag("v1.2.0")]);
}

#[test]
fn a_branch_push_records_the_branch_and_no_tag() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let env = [
        ("GITHUB_HEAD_REF", ""),
        ("GITHUB_REF_NAME", "main"),
        ("GITHUB_REF_TYPE", "branch"),
    ];
    ingest_in_env(&db, &[], &env)
        .success()
        .stdout(predicate::str::contains(" on branch main\n"));
    assert_eq!(refs(&db), [branch("main")]);
}

#[test]
fn either_flag_replaces_the_ci_env() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let tag_push = [
        ("GITHUB_HEAD_REF", ""),
        ("GITHUB_REF_NAME", "v1.2.0"),
        ("GITHUB_REF_TYPE", "tag"),
    ];
    ingest_in_env(&db, &["--branch", "mine"], &tag_push)
        .success()
        .stdout(predicate::str::contains(" on branch mine\n"));
    assert_eq!(refs(&db), [branch("mine")]);
    let tag_db = dir.path().join("tag.sqlite");
    let pull_request = [
        ("GITHUB_HEAD_REF", "feature"),
        ("GITHUB_REF_NAME", "7/merge"),
        ("GITHUB_REF_TYPE", "branch"),
    ];
    ingest_in_env(&tag_db, &["--tag", "v9"], &pull_request)
        .success()
        .stdout(predicate::str::contains(" on tag v9\n"));
    assert_eq!(refs(&tag_db), [tag("v9")]);
}

#[test]
fn both_a_branch_and_a_tag_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest_in_env(&db, &["--branch", "main", "--tag", "v1"], &[])
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
    assert!(!db.exists());
}

#[test]
fn an_empty_tag_flag_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest_in_env(&db, &["--tag", ""], &[])
        .code(2)
        .stderr(predicate::str::contains("a value is required for '--tag"));
    assert!(!db.exists());
}
