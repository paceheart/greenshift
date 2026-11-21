//! Report identity is independent of local run IDs, paths, and capture metadata.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;

fn import(db: &Path, reports: &[&Path], extra: &[&str]) -> assert_cmd::assert::Assert {
    let mut command = Command::cargo_bin("greenshift").unwrap();
    for name in greenshift::cli::CI_REF_VARS {
        command.env_remove(name);
    }
    command.arg("ingest").arg("--db").arg(db).args(extra);
    for report in reports {
        command.arg(report);
    }
    command.assert()
}

fn write_report(path: &Path, name: &str) {
    std::fs::write(
        path,
        format!(r#"<testsuite name="gears"><testcase name="{name}" time="0.025"/></testsuite>"#),
    )
    .unwrap();
}

fn observations(db: &Path) -> Vec<(String, String, bool, Option<String>, i64)> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT history_id,ingested_at,partial,branch,(SELECT count(*) FROM results WHERE run_id=runs.id) FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn test_import_same_bytes_same_commit_replay_preserves_original_observation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let original = dir.path().join("original.xml");
    let renamed = dir.path().join("renamed.xml");
    write_report(&original, "turns");
    std::fs::copy(&original, &renamed).unwrap();
    import(
        &db,
        &[&original],
        &["--commit", "ABCDEF0", "--partial", "--branch", "first"],
    )
    .success();
    let before = observations(&db);
    import(
        &db,
        &[&renamed],
        &["--commit", "abcdef0", "--branch", "later"],
    )
    .success()
    .stdout(predicate::str::contains("already recorded"))
    .stderr(predicate::str::contains("metadata"));
    assert_eq!(observations(&db), before);
    assert_eq!(before.len(), 1);
    assert!(before[0].2);
    assert_eq!(before[0].3.as_deref(), Some("first"));
}

#[test]
fn test_import_duplicate_input_does_not_double_duration() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let report = dir.path().join("report.xml");
    write_report(&report, "turns");
    import(&db, &[&report, &report], &[]).success();
    let conn = Connection::open(&db).unwrap();
    let duration: i64 = conn
        .query_row("SELECT duration_ms FROM results", [], |r| r.get(0))
        .unwrap();
    assert_eq!(duration, 25);
    let files: i64 = conn
        .query_row("SELECT count(*) FROM run_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files, 1);
}

#[test]
fn test_import_partial_overlap_conflicts_without_admitting_unseen_report() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let a = dir.path().join("a.xml");
    let b = dir.path().join("b.xml");
    write_report(&a, "turns");
    write_report(&b, "holds");
    import(&db, &[&a], &[]).success();
    let before = std::fs::read(&db).unwrap();
    import(&db, &[&a, &b], &[])
        .failure()
        .stderr(predicate::str::contains("overlap"));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(observations(&db).len(), 1);
}

#[test]
fn test_import_all_seen_spanning_groups_preserves_first_grouping() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let a = dir.path().join("a.xml");
    let b = dir.path().join("b.xml");
    write_report(&a, "turns");
    write_report(&b, "holds");
    import(&db, &[&a], &[]).success();
    import(&db, &[&b], &[]).success();
    let before = observations(&db);
    import(&db, &[&b, &a], &[])
        .success()
        .stdout(predicate::str::contains("already recorded"))
        .stderr(predicate::str::contains("grouping"));
    assert_eq!(observations(&db), before);
    assert_eq!(before.len(), 2);
}

#[test]
fn test_import_different_commits_and_different_bytes_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let report = dir.path().join("report.xml");
    write_report(&report, "turns");
    import(&db, &[&report], &["--commit", "aaaaaaa"]).success();
    import(&db, &[&report], &["--commit", "bbbbbbb"]).success();
    std::fs::write(
        &report,
        r#"<testsuite name="gears"><testcase name="turns" time="0.025" /></testsuite>"#,
    )
    .unwrap();
    import(&db, &[&report], &["--commit", "bbbbbbb"]).success();
    assert_eq!(observations(&db).len(), 3);
}

#[test]
fn test_import_reordered_exact_bundle_and_subset_replays_keep_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let a = dir.path().join("a.xml");
    let b = dir.path().join("b.xml");
    write_report(&a, "turns");
    write_report(&b, "holds");
    import(&db, &[&a, &b], &[]).success();
    let before = observations(&db);
    import(&db, &[&b, &a], &[])
        .success()
        .stdout(predicate::str::contains("already recorded"))
        .stderr(predicate::str::is_empty());
    import(&db, &[&a], &[])
        .success()
        .stdout(predicate::str::contains("already recorded"))
        .stderr(predicate::str::contains("grouping"));
    assert_eq!(observations(&db), before);
}

#[test]
fn test_import_exact_group_stored_result_disagreement_is_not_silently_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    let report = dir.path().join("report.xml");
    write_report(&report, "turns");
    import(&db, &[&report], &[]).success();
    let conn = Connection::open(&db).unwrap();
    conn.execute("UPDATE results SET duration_ms=900", [])
        .unwrap();
    drop(conn);
    let before = std::fs::read(&db).unwrap();
    import(&db, &[&report], &[]).failure();
    assert_eq!(std::fs::read(&db).unwrap(), before);
}
