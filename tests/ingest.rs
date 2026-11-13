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

fn greenshift() -> Command {
    Command::cargo_bin("greenshift").expect("binary builds")
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

    let (commit, source, file_name, version): (Option<String>, String, String, i64) = conn
        .query_row(
            "SELECT commit_sha, source, file_name, (SELECT user_version FROM pragma_user_version)
             FROM runs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(commit.as_deref(), Some("0123abcd"));
    assert_eq!(source, "ci");
    assert_eq!(
        file_name,
        format!("{}\n{}", files[0].display(), files[1].display())
    );
    assert_eq!(version, 1);

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
fn each_invocation_is_a_new_run_sharing_test_identities() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "aaa"]).success();
    ingest(&db, &[fixture("nextest.xml")], &["--commit", "bbb"])
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
                .and(predicate::str::contains(".greenshift/history.sqlite")),
        );
}
