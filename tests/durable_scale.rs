//! Read-back and scale receipts for the durable history protocol.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use greenshift::history::{snapshot, Remote};
use greenshift::junit::{Outcome, TestCase};
use greenshift::store::{NewRun, Store};
use greenshift::tainted::Tainted;
use rusqlite::Connection;

fn record(path: &Path, count: usize) {
    let cases: Vec<_> = (0..count)
        .map(|index| {
            TestCase::new(
                Tainted::raw("scale-suite"),
                Tainted::raw(format!("case-{index:06}")),
                if index % 17 == 0 {
                    Outcome::Failed
                } else {
                    Outcome::Passed
                },
                Some((index % 1000) as i64),
            )
        })
        .collect();
    let mut store = Store::open(path).unwrap();
    let receipt = store
        .record_run(
            &NewRun {
                commit: None,
                source: "scale-producer",
                files: &[],
                partial: false,
                branch: Some("main"),
                tag: None,
            },
            &cases,
        )
        .unwrap();
    assert_eq!(receipt.results, count);
}

#[test]
fn independently_migrated_legacy_copies_keep_the_same_durable_run_identity() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("legacy.sqlite");
    let first = dir.path().join("first.sqlite");
    let second = dir.path().join("second.sqlite");
    let merged = dir.path().join("merged.sqlite");
    record(&legacy, 7);
    {
        let conn = Connection::open(&legacy).unwrap();
        conn.execute_batch(
            "INSERT INTO runs(commit_sha,source,ingested_at,partial,branch,tag,history_id)
                 SELECT commit_sha,source,ingested_at,partial,branch,tag,lower(hex(randomblob(16))) FROM runs;
             INSERT INTO results SELECT 2,test_id,outcome,duration_ms FROM results;
             DROP TABLE report_memberships;
             DROP INDEX runs_by_history_id;
             ALTER TABLE runs DROP COLUMN history_id;
             PRAGMA user_version=3;",
        )
        .unwrap();
    }
    std::fs::copy(&legacy, &first).unwrap();
    std::fs::copy(&legacy, &second).unwrap();
    drop(Store::open(&first).unwrap());
    drop(Store::open(&second).unwrap());
    let identity = |path: &Path| {
        Connection::open(path)
            .unwrap()
            .prepare("SELECT history_id FROM runs ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(identity(&first), identity(&second));
    assert_eq!(identity(&first).len(), 2);
    assert_ne!(identity(&first)[0], identity(&first)[1]);
    assert!(identity(&first).iter().all(|id| id.len() == 32));
    snapshot::merge(&first, &second, &merged).unwrap();
    let conn = Connection::open(&merged).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM results", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        14
    );
}

#[test]
fn fifty_thousand_rows_restore_from_ref_after_every_cache_entry_is_deleted() {
    const ROWS: usize = 50_001;
    let dir = tempfile::tempdir().unwrap();
    let remote_path = dir.path().join("remote.git");
    let input = dir.path().join("input.sqlite");
    let restored = dir.path().join("restored.sqlite");
    let cache = dir.path().join("cache");
    let output = Command::new("git")
        .args(["init", "--bare"])
        .arg(&remote_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    record(&input, ROWS);
    let url = remote_path.to_str().unwrap();
    let start = Instant::now();
    let published = Remote::new(url)
        .unwrap()
        .publish(&input, Some(&cache))
        .unwrap();
    let publish = start.elapsed();
    assert!(cache.join(published.oid()).is_dir());
    std::fs::remove_dir_all(&cache).unwrap();
    std::fs::remove_file(&input).unwrap();
    assert!(!cache.exists());
    assert!(!input.exists());

    // A new reader has neither local objects nor any cache files. The remote
    // compressed DB is the only remaining source of these result rows.
    let reader = Remote::new(url).unwrap();
    let start = Instant::now();
    assert!(matches!(
        reader.restore(&restored, Some(&cache)).unwrap(),
        greenshift::history::RestoreOutcome::Restored(_)
    ));
    let restore = start.elapsed();
    let start = Instant::now();
    let store = Store::open_read_only(&restored).unwrap().unwrap();
    let open = start.elapsed();
    drop(store);
    let conn = Connection::open(&restored).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM results", [], |row| row.get(0))
        .unwrap();
    let failures: i64 = conn
        .query_row(
            "SELECT count(*) FROM results WHERE outcome='failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, ROWS as i64);
    assert_eq!(failures, ((ROWS - 1) / 17 + 1) as i64);
    assert!(!cache.exists());
    eprintln!(
        "local bare-git cache-wipe receipt: rows={rows}; publish_ms={:.3}; fetch_decompress_validate_ms={:.3}; sqlite_open_ms={:.3}; db_bytes={}",
        publish.as_secs_f64() * 1000.0,
        restore.as_secs_f64() * 1000.0,
        open.as_secs_f64() * 1000.0,
        std::fs::metadata(&restored).unwrap().len(),
    );
}
