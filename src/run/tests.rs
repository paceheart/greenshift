use super::*;
use crate::cli::{parse, Backend as BackendCommand, Command};
use crate::plan::HistoryState;

fn test(suite: &str, name: &str) -> InventoryTest {
    InventoryTest::new(Tainted::raw(suite), Tainted::raw(name))
}

#[test]
fn both_groups_run_rich_first() {
    let (a, b) = (test("s", "a"), test("s", "b"));
    let (rich, poor) = ([&a], [&b]);
    let groups: Vec<_> = planned_groups(&rich, &poor).collect();
    let classes: Vec<Class> = groups.iter().map(|(class, _)| *class).collect();
    assert_eq!(classes, [Class::Rich, Class::Poor]);
    assert_eq!(groups[0].1, [&a]);
    assert_eq!(groups[1].1, [&b]);
}

#[test]
fn an_empty_group_is_not_run() {
    let a = test("s", "a");
    let classes = |rich: &[&InventoryTest], poor: &[&InventoryTest]| -> Vec<Class> {
        planned_groups(rich, poor).map(|g| g.0).collect()
    };
    assert_eq!(classes(&[&a], &[]), [Class::Rich]);
    assert_eq!(classes(&[], &[&a]), [Class::Poor]);
    // With no test at all, rich runs empty, so the runner decides.
    let none: Vec<_> = planned_groups(&[], &[]).collect();
    assert_eq!(none.len(), 1);
    assert_eq!(none[0].0, Class::Rich);
    assert!(none[0].1.is_empty());
}

#[test]
fn the_exit_status_is_the_first_failure() {
    assert_eq!(combined([]), 0);
    assert_eq!(combined([0, 0]), 0);
    assert_eq!(combined([100, 0]), 100);
    assert_eq!(combined([0, 100]), 100);
    assert_eq!(combined([0, 101, 100]), 101);
    assert_eq!(combined([4, 100]), 4);
}

#[cfg(unix)]
#[test]
fn a_process_exit_status_becomes_greenshifts() {
    use std::os::unix::process::ExitStatusExt;
    // Wait statuses: the code in the second byte, a signal in the first.
    assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
    assert_eq!(exit_code(ExitStatus::from_raw(100 << 8)), 100);
    assert_eq!(exit_code(ExitStatus::from_raw(9)), 128 + 9);
}

fn options(args: &[&str]) -> RunOptions {
    let argv = ["greenshift", "run", "nextest"].iter().chain(args);
    let Command::Run(run) = parse(argv).unwrap().command else {
        panic!("not run");
    };
    let BackendCommand::Nextest(args) = run.backend;
    args.options
}

#[test]
fn the_run_is_recorded_as_from_its_source() {
    let opts = options(&[]);
    assert_eq!(opts.select.source, ["nextest"]);
    assert_eq!(record_source(&opts, "nextest"), "nextest");
    let opts = options(&["--source", "ci-linux"]);
    assert_eq!(opts.select.source, ["ci-linux"]);
    assert_eq!(record_source(&opts, "nextest"), "ci-linux");
}

#[test]
fn test_record_identical_execution_reports_create_distinct_unkeyed_observations() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("execution.xml");
    let xml = r#"<testsuite name="gears"><testcase name="turns" time="0.012"/></testsuite>"#;
    std::fs::write(&file, xml).unwrap();
    let database = directory.path().join("history.sqlite");
    let opts = options(&[
        "--db",
        database.to_str().unwrap(),
        "--commit",
        "abcdef1",
        "--branch",
        "fixture",
    ]);
    // The real runner adapter must record executions, not import retries.
    // Keep the file bytes and commit identical across both calls.
    record(&opts, "nextest", std::slice::from_ref(&file));
    record(&opts, "nextest", std::slice::from_ref(&file));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), xml);
    let connection = rusqlite::Connection::open(database).unwrap();
    let counts: (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT count(*), count(DISTINCT history_id),
             (SELECT count(*) FROM results),
             (SELECT count(*) FROM report_memberships) FROM runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(counts, (2, 2, 2, 0));
}

#[test]
fn a_store_that_cannot_be_read_plans_every_test_rich() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.sqlite");
    std::fs::write(&db, "not a SQLite file, but long enough to be read as one").unwrap();
    let opts = options(&["--db", db.to_str().unwrap()]);
    let tests = [test("s", "a"), test("s", "b")];
    let (history, groups) = cli::plan_groups(&opts.select, &tests).unwrap();
    assert_eq!(history, HistoryState::Error);
    assert_eq!(groups.rich(), tests.iter().collect::<Vec<_>>());
    assert!(groups.poor().is_empty());
}

#[test]
fn no_store_plans_every_test_rich() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new/history.sqlite");
    let opts = options(&["--db", db.to_str().unwrap()]);
    let tests = [test("s", "a")];
    let (history, groups) = cli::plan_groups(&opts.select, &tests).unwrap();
    assert_eq!(history, HistoryState::Ok);
    assert_eq!(groups.rich(), [&tests[0]]);
    assert!(groups.poor().is_empty());
}

#[test]
fn run_takes_options_and_arguments_after_a_separator() {
    let argv = [
        "greenshift",
        "run",
        "nextest",
        "--db",
        "h.sqlite",
        "--commit",
        "ABCDEF1",
        "--min-runs",
        "3",
        "--max-filterset-bytes",
        "500",
        "--",
        "--no-fail-fast",
        "-E",
        "test(x)",
    ];
    let Command::Run(run) = parse(argv).unwrap().command else {
        panic!("not run");
    };
    let BackendCommand::Nextest(args) = run.backend;
    assert_eq!(args.options.select.db, PathBuf::from("h.sqlite"));
    assert_eq!(
        args.options.commit.as_ref().map(|c| c.as_str()),
        Some("abcdef1")
    );
    assert_eq!(args.options.select.min_runs, 3);
    assert_eq!(args.max_filterset_bytes, 500);
    assert_eq!(args.args, ["--no-fail-fast", "-E", "test(x)"]);
    // Arguments for nextest come after `--`, and a backend is named.
    assert!(parse(["greenshift", "run", "nextest", "--no-fail-fast"]).is_err());
    assert!(parse(["greenshift", "run"]).is_err());
    assert!(parse(["greenshift", "run", "lein"]).is_err());
    assert!(parse(["greenshift", "run", "nextest", "--branch", ""]).is_err());
    assert!(parse(["greenshift", "run", "nextest", "--tag", ""]).is_err());
    let both = ["--branch", "main", "--tag", "v1"];
    assert!(parse(["greenshift", "run", "nextest"].into_iter().chain(both)).is_err());
    assert_eq!(
        options(&["--branch", "topic"]).branch.as_deref(),
        Some("topic")
    );
    assert_eq!(options(&["--tag", "v1.2.0"]).tag.as_deref(), Some("v1.2.0"));
}
