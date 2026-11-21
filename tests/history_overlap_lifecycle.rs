//! Real transport cycles must not strand unrelated observations behind a group conflict.

use std::path::Path;
use std::process::Output;

use greenshift::history::{GroupOverlap, MergeReport, PublishOutcome, Remote, RestoreOutcome};
use greenshift::junit::{Outcome, TestCase};
use greenshift::store::{NewRun, Store};
use greenshift::tainted::Tainted;
use rusqlite::Connection;

#[cfg(unix)]
#[path = "common/history_cas.rs"]
mod history_cas;

#[derive(Debug, PartialEq, Eq)]
struct RunRow {
    identity: String,
    commit: Option<String>,
    source: String,
    at: String,
    partial: bool,
    branch: Option<String>,
    tag: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResultRow {
    identity: String,
    suite: String,
    name: String,
    outcome: String,
    duration: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    runs: Vec<RunRow>,
    results: Vec<ResultRow>,
    members: Vec<(String, String, i64, String)>,
    files: Vec<(String, i64, Vec<u8>)>,
    tests: Vec<(String, String)>,
}

fn query<T>(
    connection: &Connection,
    sql: &str,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Vec<T> {
    connection
        .prepare(sql)
        .unwrap()
        .query_map([], map)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn state(path: &Path) -> State {
    let connection = Connection::open(path).unwrap();
    State {
        runs: query(&connection, "SELECT history_id,commit_sha,source,ingested_at,partial,branch,tag FROM runs ORDER BY history_id", |r| Ok(RunRow {
            identity: r.get(0)?, commit: r.get(1)?, source: r.get(2)?, at: r.get(3)?, partial: r.get(4)?, branch: r.get(5)?, tag: r.get(6)?,
        })),
        results: query(&connection, "SELECT r.history_id,t.suite,t.name,x.outcome,x.duration_ms FROM results x JOIN runs r ON r.id=x.run_id JOIN tests t ON t.id=x.test_id ORDER BY r.history_id,t.suite,t.name", |r| Ok(ResultRow {
            identity: r.get(0)?, suite: r.get(1)?, name: r.get(2)?, outcome: r.get(3)?, duration: r.get(4)?,
        })),
        members: query(&connection, "SELECT r.history_id,m.namespace,m.fingerprint_version,m.digest FROM report_memberships m JOIN runs r ON r.id=m.run_id ORDER BY r.history_id,m.namespace,m.digest", |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))),
        files: query(&connection, "SELECT r.history_id,f.position,f.path FROM run_files f JOIN runs r ON r.id=f.run_id ORDER BY r.history_id,f.position", |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))),
        tests: query(&connection, "SELECT suite,name FROM tests ORDER BY suite,name", |r| Ok((r.get(0)?,r.get(1)?))),
    }
}

fn command() -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("greenshift").unwrap();
    for name in greenshift::cli::CI_REF_VARS.into_iter().chain([
        "CI",
        "GITHUB_ACTIONS",
        "GITHUB_EVENT_NAME",
        "GITHUB_REF",
        "GREENSHIFT_HISTORY_TOKEN",
    ]) {
        command.env_remove(name);
    }
    command
}

fn history(operation: &str, repository: &Path, db: &Path) -> Output {
    command()
        .args(["history", operation, "--remote"])
        .arg(repository)
        .arg("--db")
        .arg(db)
        .assert()
        .success()
        .get_output()
        .clone()
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn head(repository: &Path) -> String {
    git(repository, &["rev-parse", "refs/greenshift/history"])
}

fn assert_published(output: &Output, repository: &Path) {
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(
        oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()),
        "nonconflicting history must publish despite a group conflict: {output:?}"
    );
    assert_eq!(oid, head(repository));
    assert_eq!(
        git(
            repository,
            &["rev-list", "--parents", "-1", "refs/greenshift/history"]
        )
        .split_whitespace()
        .count(),
        1
    );
    assert_eq!(
        git(
            repository,
            &["rev-list", "--count", "refs/greenshift/history"]
        ),
        "1"
    );
}

fn canonical(repository: &Path) -> State {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("canonical.sqlite");
    let before = head(repository);
    history("restore", repository, &db);
    assert_eq!(
        head(repository),
        before,
        "reading canonical history must not publish"
    );
    state(&db)
}

fn import(db: &Path, reports: &[&Path], source: &str, partial: bool) -> String {
    let mut command = command();
    command
        .args(["ingest", "--db"])
        .arg(db)
        .args(["--commit", "abcdef1", "--source", source]);
    if partial {
        command.args(["--partial", "--branch", "first-branch"]);
    } else {
        command.args(["--tag", "first-tag"]);
    }
    command.args(reports).assert().success();
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT history_id FROM runs WHERE source=?1",
            [source],
            |r| r.get(0),
        )
        .unwrap()
}

fn execution(db: &Path, source: &str) -> String {
    let case = TestCase::new(
        Tainted::raw("executions"),
        Tainted::raw(source),
        Outcome::Passed,
        Some(11),
    );
    Store::open(db)
        .unwrap()
        .record_run(
            &NewRun {
                commit: None,
                source,
                files: &[],
                partial: false,
                branch: Some("execution-branch"),
                tag: None,
            },
            [&case],
        )
        .unwrap();
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT history_id FROM runs WHERE source=?1",
            [source],
            |r| r.get(0),
        )
        .unwrap()
}

fn orphan(db: &Path, name: &str) {
    Connection::open(db)
        .unwrap()
        .execute("INSERT INTO tests(suite,name) VALUES('unused',?1)", [name])
        .unwrap();
}

fn assert_ids(observed: &State, expected: &[&str]) {
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(
        observed
            .runs
            .iter()
            .map(|run| run.identity.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_run_preserved(before: &State, after: &State, identity: &str) {
    assert_eq!(
        before
            .runs
            .iter()
            .find(|run| run.identity == identity)
            .unwrap(),
        after
            .runs
            .iter()
            .find(|run| run.identity == identity)
            .unwrap()
    );
    assert_eq!(
        before
            .results
            .iter()
            .filter(|row| row.identity == identity)
            .collect::<Vec<_>>(),
        after
            .results
            .iter()
            .filter(|row| row.identity == identity)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        before
            .members
            .iter()
            .filter(|row| row.0 == identity)
            .collect::<Vec<_>>(),
        after
            .members
            .iter()
            .filter(|row| row.0 == identity)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        before
            .files
            .iter()
            .filter(|row| row.0 == identity)
            .collect::<Vec<_>>(),
        after
            .files
            .iter()
            .filter(|row| row.0 == identity)
            .collect::<Vec<_>>()
    );
}

fn init_repository(repository: &Path) {
    let output = std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(repository)
        .output()
        .unwrap();
    assert!(output.status.success());
}

fn fixture(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let repository = directory.join("history.git");
    init_repository(&repository);
    let a = directory.join("a.xml");
    let b = directory.join("b.xml");
    for (file, name) in [(&a, "a"), (&b, "b")] {
        std::fs::write(
            file,
            format!(
                r#"<testsuite name="gears"><testcase name="{name}" time="0.025"/></testsuite>"#
            ),
        )
        .unwrap();
    }
    (repository, a, b)
}

#[test]
fn public_transport_reports_expose_named_types_and_nonempty_shared_readers() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, a, b) = fixture(directory.path());
    let destination = directory.path().join("destination.sqlite");
    let incoming = directory.path().join("incoming.sqlite");
    import(&destination, &[&a], "remote-group", true);
    let incoming_id = import(&incoming, &[&a, &b], "local-group", false);
    let remote = Remote::new(repository.to_str().unwrap()).unwrap();
    let initial: PublishOutcome = remote.publish(&destination, None).unwrap();
    assert!(initial.report().excluded_groups().is_empty());
    let published: PublishOutcome = remote.publish(&incoming, None).unwrap();
    assert_eq!(remote.head().unwrap().as_deref(), Some(published.oid()));
    let report: &MergeReport = published.report();
    assert_eq!(report.excluded_groups().len(), 1);
    let excluded: &GroupOverlap = &report.excluded_groups()[0];
    assert_eq!(excluded.history_id(), incoming_id);
    assert_eq!(excluded.unseen_memberships(), 1);
    assert!(report.retained_groups().is_empty());
    assert_eq!(
        (report.nonexact_replays(), report.metadata_replays()),
        (0, 0)
    );

    let restored: RestoreOutcome = remote.restore(&incoming, None).unwrap();
    let RestoreOutcome::Restored(report) = restored else {
        panic!("canonical history exists");
    };
    assert!(report.excluded_groups().is_empty());
    assert_eq!(report.retained_groups().len(), 1);
    let retained: &GroupOverlap = &report.retained_groups()[0];
    assert_eq!(retained.history_id(), incoming_id);
    assert_eq!(retained.unseen_memberships(), 1);
    assert_eq!(
        (report.nonexact_replays(), report.metadata_replays()),
        (1, 1)
    );
}

#[cfg(unix)]
#[test]
fn successful_cas_retry_reports_groups_excluded_by_the_new_remote_memberships() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, a, b) = fixture(directory.path());
    let baseline = directory.path().join("baseline.sqlite");
    let baseline_id = execution(&baseline, "baseline-d");
    let baseline_state = state(&baseline);
    assert_published(&history("publish", &repository, &baseline), &repository);

    let local = directory.path().join("local.sqlite");
    let competing = directory.path().join("competing.sqlite");
    history("restore", &repository, &local);
    history("restore", &repository, &competing);
    let excluded_id = import(&local, &[&a, &b], "local-group", false);
    let u = execution(&local, "local-u");
    let competing_id = import(&competing, &[&a], "competing-group", true);
    let w = execution(&competing, "competing-w");
    let local_before = state(&local);
    let competing_before = state(&competing);

    let competing_repo = directory.path().join("competing.git");
    init_repository(&competing_repo);
    let competing_publish = history("publish", &competing_repo, &competing);
    assert_published(&competing_publish, &competing_repo);
    let competing_oid = String::from_utf8(competing_publish.stdout).unwrap();

    let bytes = std::fs::read(&local).unwrap();
    let (mut command, push_log) = history_cas::command(
        directory.path(),
        &repository,
        &competing_repo,
        competing_oid.trim(),
        &local,
    );
    let published = command.assert().success().get_output().clone();
    assert_published(&published, &repository);
    assert_eq!(
        std::fs::read_to_string(push_log).unwrap().lines().count(),
        2
    );
    assert_eq!(std::fs::read(&local).unwrap(), bytes);
    assert_eq!(state(&local), local_before);

    // The first candidate had no keyed overlap. Only the accepted retry can
    // explain this omission, so an obsolete first-attempt report is insufficient.
    let warning = String::from_utf8_lossy(&published.stderr);
    assert!(
        warning.contains(&excluded_id)
            && warning.contains("was not published")
            && warning.contains("no automatic recovery"),
        "{warning}"
    );
    let observed = canonical(&repository);
    assert_ids(&observed, &[&baseline_id, &competing_id, &w, &u]);
    assert_eq!(observed.members.len(), 1);
    assert_eq!(observed.results.len(), 4);
    assert_eq!(
        observed
            .results
            .iter()
            .filter(|row| row.name == "a")
            .count(),
        1
    );
    assert!(!observed.tests.iter().any(|(_, name)| name == "b"));
    assert_run_preserved(&baseline_state, &observed, &baseline_id);
    assert_run_preserved(&competing_before, &observed, &competing_id);
    assert_run_preserved(&competing_before, &observed, &w);
    assert_run_preserved(&local_before, &observed, &u);
}

#[test]
fn publish_restore_publish_skips_only_local_overlapping_group_and_keeps_unrelated_runs() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, a, b) = fixture(directory.path());
    let remote = directory.path().join("remote.sqlite");
    let local = directory.path().join("local.sqlite");
    let remote_id = import(&remote, &[&a], "remote-group", true);
    let local_id = import(&local, &[&a, &b], "local-group", false);
    let w = execution(&remote, "remote-w");
    let u = execution(&local, "local-u");
    orphan(&remote, "remote-orphan");
    orphan(&local, "local-orphan");
    let remote_before = state(&remote);
    let local_before = state(&local);
    assert_published(&history("publish", &repository, &remote), &repository);

    let bytes = std::fs::read(&local).unwrap();
    let published = history("publish", &repository, &local);
    assert_published(&published, &repository);
    let warning = String::from_utf8_lossy(&published.stderr);
    assert!(
        warning.contains(&local_id)
            && warning.contains("was not published")
            && warning.contains("no automatic recovery"),
        "{warning}"
    );
    assert_eq!(std::fs::read(&local).unwrap(), bytes);
    let canonical_before = canonical(&repository);
    assert_ids(&canonical_before, &[&remote_id, &w, &u]);
    assert_run_preserved(&remote_before, &canonical_before, &remote_id);
    assert_run_preserved(&remote_before, &canonical_before, &w);
    assert_run_preserved(&local_before, &canonical_before, &u);
    assert_eq!(canonical_before.members.len(), 1);
    assert_eq!(
        canonical_before
            .results
            .iter()
            .filter(|row| row.name == "a")
            .count(),
        1
    );
    assert!(!canonical_before.tests.iter().any(|(_, name)| name == "b"));
    assert!(canonical_before
        .tests
        .contains(&("unused".into(), "remote-orphan".into())));
    assert!(canonical_before
        .tests
        .contains(&("unused".into(), "local-orphan".into())));

    let before_restore = head(&repository);
    let restored = history("restore", &repository, &local);
    let warning = String::from_utf8_lossy(&restored.stderr);
    assert!(
        warning.contains(&local_id) && warning.contains("did not reconcile"),
        "{warning}"
    );
    assert_eq!(head(&repository), before_restore);
    assert_ids(&state(&local), &[&local_id, &u, &w]);
    assert_run_preserved(&local_before, &state(&local), &local_id);
    assert_run_preserved(&remote_before, &state(&local), &w);

    let v = execution(&local, "local-v");
    let local_with_v = state(&local);
    for _ in 0..2 {
        let bytes = std::fs::read(&local).unwrap();
        let published = history("publish", &repository, &local);
        assert_published(&published, &repository);
        let warning = String::from_utf8_lossy(&published.stderr);
        assert!(
            warning.contains(&local_id)
                && warning.contains("was not published")
                && warning.contains("no automatic recovery"),
            "{warning}"
        );
        assert_eq!(std::fs::read(&local).unwrap(), bytes);
        let observed = canonical(&repository);
        assert_ids(&observed, &[&remote_id, &w, &u, &v]);
        assert_eq!(observed.members.len(), 1);
        assert_eq!(observed.results.len(), 4);
        assert_run_preserved(&remote_before, &observed, &remote_id);
        assert_run_preserved(&remote_before, &observed, &w);
        assert_run_preserved(&local_before, &observed, &u);
        assert_run_preserved(&local_with_v, &observed, &v);
        let before_restore = head(&repository);
        let restored = history("restore", &repository, &local);
        let warning = String::from_utf8_lossy(&restored.stderr);
        assert!(
            warning.contains(&local_id) && warning.contains("did not reconcile"),
            "{warning}"
        );
        assert_eq!(head(&repository), before_restore);
        assert_eq!(state(&local), local_with_v);
    }
}

#[test]
fn restore_publish_restore_skips_only_remote_overlapping_group_and_keeps_unrelated_runs() {
    let directory = tempfile::tempdir().unwrap();
    let (repository, a, b) = fixture(directory.path());
    let remote = directory.path().join("remote.sqlite");
    let local = directory.path().join("local.sqlite");
    let remote_id = import(&remote, &[&a, &b], "remote-group", true);
    let local_id = import(&local, &[&a], "local-group", false);
    let w = execution(&remote, "remote-w");
    let u = execution(&local, "local-u");
    orphan(&remote, "remote-orphan");
    orphan(&local, "local-orphan");
    let remote_before = state(&remote);
    let local_before = state(&local);
    assert_published(&history("publish", &repository, &remote), &repository);

    let before_restore = head(&repository);
    let restored = history("restore", &repository, &local);
    let warning = String::from_utf8_lossy(&restored.stderr);
    assert!(
        warning.contains(&remote_id) && warning.contains("was not restored"),
        "{warning}"
    );
    assert_eq!(head(&repository), before_restore);
    let restored_state = state(&local);
    assert_ids(&restored_state, &[&local_id, &u, &w]);
    assert_run_preserved(&local_before, &restored_state, &local_id);
    assert_run_preserved(&remote_before, &restored_state, &w);
    assert_eq!(restored_state.members.len(), 1);
    assert!(!restored_state.tests.iter().any(|(_, name)| name == "b"));
    assert!(restored_state
        .tests
        .contains(&("unused".into(), "local-orphan".into())));
    assert!(restored_state
        .tests
        .contains(&("unused".into(), "remote-orphan".into())));

    let v = execution(&local, "local-v");
    let local_with_v = state(&local);
    for _ in 0..2 {
        let bytes = std::fs::read(&local).unwrap();
        assert_published(&history("publish", &repository, &local), &repository);
        assert_eq!(std::fs::read(&local).unwrap(), bytes);
        let observed = canonical(&repository);
        assert_ids(&observed, &[&remote_id, &w, &u, &v]);
        assert_eq!(observed.members.len(), 2);
        assert_eq!(observed.results.len(), 5);
        assert_eq!(
            observed
                .results
                .iter()
                .filter(|row| row.name == "a")
                .count(),
            1
        );
        assert_run_preserved(&remote_before, &observed, &remote_id);
        assert_run_preserved(&remote_before, &observed, &w);
        assert_run_preserved(&local_before, &observed, &u);
        assert_run_preserved(&local_with_v, &observed, &v);
        let before_restore = head(&repository);
        let restored = history("restore", &repository, &local);
        let warning = String::from_utf8_lossy(&restored.stderr);
        assert!(
            warning.contains(&remote_id) && warning.contains("was not restored"),
            "{warning}"
        );
        assert_eq!(head(&repository), before_restore);
        assert_eq!(state(&local), local_with_v);
    }
}
