//! Runs `greenshift run nextest` on a scratch project with real
//! cargo-nextest, and checks what it ran and recorded. Needs
//! cargo-nextest installed.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use rusqlite::Connection;

const PACKAGE: &str = "run-me";

/// The tests of the `one` binary, and of the `two` binary.
const ONE: &[&str] = &["alpha", "beta (b), c", "gamma"];
const TWO: &[&str] = &["delta", "epsilon"];

/// A test binary that lists `names` and fails a test when `FAIL` has a
/// line naming it.
fn harness(names: &[&str]) -> String {
    format!(
        r#"const NAMES: &[&str] = &{names:?};

fn main() {{
    if std::env::var_os("DENY_HISTORY_TOKEN").is_some()
        && std::env::var_os("GREENSHIFT_HISTORY_TOKEN").is_some() {{
        eprintln!("test child inherited history credential");
        std::process::exit(42);
    }}
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {{
        if !args.iter().any(|a| a == "--ignored") {{
            for name in NAMES {{
                println!("{{name}}: test");
            }}
        }}
        return;
    }}
    let name = args.iter().find(|a| !a.starts_with("--")).expect("a test name");
    let fail = std::env::var("FAIL").unwrap_or_default();
    if fail.lines().any(|line| line == name) {{
        eprintln!("failing {{name}} as asked");
        std::process::exit(101);
    }}
}}
"#
    )
}

struct Project {
    dir: tempfile::TempDir,
    db: PathBuf,
}

/// One stored run: its source, commit, whether it is partial, how many
/// JUnit files it came from, and its results as `(binary, name,
/// outcome)`, sorted.
#[derive(Debug, PartialEq)]
struct Run {
    source: String,
    commit: Option<String>,
    partial: bool,
    files: usize,
    results: Vec<(String, String, String)>,
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".config")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("tests/one.rs"), harness(ONE)).unwrap();
        std::fs::write(root.join("tests/two.rs"), harness(TWO)).unwrap();
        let mut manifest = format!(
            "[package]\nname = \"{PACKAGE}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             publish = false\nautotests = false\n\n[workspace]\n\n\
             [lib]\ntest = false\ndoctest = false\n"
        );
        for binary in ["one", "two"] {
            manifest.push_str(&format!(
                "\n[[test]]\nname = \"{binary}\"\nharness = false\n"
            ));
        }
        std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
        // greenshift's own nextest config: it sets the default profile's
        // JUnit path, which run's profile must not take.
        std::fs::write(
            root.join(".config/nextest.toml"),
            include_str!("../.config/nextest.toml"),
        )
        .unwrap();
        let db = dir.path().join("history.sqlite");
        Project { dir, db }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// `greenshift run nextest <args>` in the project, failing the tests
    /// `fail` names, without the environment of the nextest or cargo run
    /// this test is part of.
    fn run(&self, args: &[&str], fail: &[&str]) -> Output {
        self.run_in_env(args, fail, &[])
    }

    /// [`Project::run`], with `env` set too. No CI branch or tag variable is
    /// inherited: only those in `env` are set.
    fn run_in_env(&self, args: &[&str], fail: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::cargo_bin("greenshift").expect("binary builds");
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().into_owned();
            let inherited = key.starts_with("NEXTEST_")
                || (key.starts_with("CARGO_") && key != "CARGO_HOME")
                || greenshift::cli::CI_REF_VARS.contains(&key.as_str());
            if inherited {
                cmd.env_remove(&key);
            }
        }
        cmd.envs(env.iter().copied());
        let fail: String = fail.iter().map(|name| format!("{name}\n")).collect();
        cmd.current_dir(self.root())
            .env("FAIL", fail)
            .args(["run", "nextest", "--db"])
            .arg(&self.db)
            .args(args)
            .output()
            .expect("greenshift runs")
    }

    /// Every stored run, oldest first.
    fn runs(&self) -> Vec<Run> {
        let conn = Connection::open(&self.db).unwrap();
        let mut runs = conn
            .prepare(
                "SELECT id, source, commit_sha, partial, \
                 (SELECT count(*) FROM run_files f WHERE f.run_id = runs.id) \
                 FROM runs ORDER BY id",
            )
            .unwrap();
        let runs: Vec<(i64, Run)> = runs
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    Run {
                        source: row.get(1)?,
                        commit: row.get(2)?,
                        partial: row.get::<_, i64>(3)? == 1,
                        files: row.get::<_, i64>(4)? as usize,
                        results: Vec::new(),
                    },
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let prefix = format!("{PACKAGE}::");
        runs.into_iter()
            .map(|(id, mut run)| {
                let mut results = conn
                    .prepare(
                        "SELECT t.suite, t.name, r.outcome FROM results r \
                         JOIN tests t ON t.id = r.test_id WHERE r.run_id = ?1 \
                         ORDER BY t.suite, t.name",
                    )
                    .unwrap();
                run.results = results
                    .query_map([id], |row| {
                        let suite: String = row.get(0)?;
                        Ok((
                            suite.strip_prefix(&prefix).unwrap_or(&suite).to_owned(),
                            row.get(1)?,
                            row.get(2)?,
                        ))
                    })
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                run
            })
            .collect()
    }
}

/// Every test, as stored, with `failed` failed and the rest passed.
fn every_test(failed: &[&str]) -> Vec<(String, String, String)> {
    let mut tests: Vec<(String, String, String)> = [("one", ONE), ("two", TWO)]
        .iter()
        .flat_map(|(binary, names)| {
            names.iter().map(move |name| {
                let outcome = if failed.contains(name) {
                    "failed"
                } else {
                    "passed"
                };
                (binary.to_string(), name.to_string(), outcome.to_owned())
            })
        })
        .collect();
    tests.sort();
    tests
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn history_credential_is_absent_from_listing_and_running_test_children() {
    let project = Project::new();
    let out = project.run_in_env(
        &[],
        &[],
        &[
            (
                "GREENSHIFT_HISTORY_TOKEN",
                "invented-test-history-credential",
            ),
            ("DENY_HISTORY_TOKEN", "1"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(project.runs()[0].results.len(), 5);
    assert!(!stderr(&out).contains("invented-test-history-credential"));
}

#[test]
fn durable_restore_failure_still_runs_all_tests_and_exports_failed_results() {
    let p = Project::new();
    let artifact = p.dir.path().join("publish.sqlite");
    let out = p.run(
        &[
            "--history-remote",
            "/nonexistent/history.git",
            "--history-artifact",
            artifact.to_str().unwrap(),
            "--",
            "--no-fail-fast",
        ],
        &["gamma"],
    );
    assert_eq!(out.status.code(), Some(100), "{}", stderr(&out));
    assert!(stderr(&out).contains('⚠'));
    greenshift::history::snapshot::validate(&artifact).unwrap();
    let conn = Connection::open(artifact).unwrap();
    let (total, failed): (i64, i64) = conn
        .query_row(
            "SELECT count(*),sum(outcome='failed') FROM results",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((total, failed), (5, 1));
}

#[test]
fn durable_ref_restored_before_plan_in_real_run() {
    let p = Project::new();
    assert!(p.run(&[], &[]).status.success());
    let repo = p.dir.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    let remote = greenshift::history::Remote::new(repo.to_str().unwrap()).unwrap();
    remote.publish(&p.db, None).unwrap();
    std::fs::remove_file(&p.db).unwrap();
    let out = p.run(
        &[
            "--history-remote",
            repo.to_str().unwrap(),
            "--min-runs",
            "1",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(p.runs().len(), 2);
    assert!(!stderr(&out).contains("no history: run the full suite"));
}

#[test]
fn durable_restore_grouping_limitation_is_reported_by_the_real_runner() {
    let project = Project::new();
    let a = project.dir.path().join("a.xml");
    let b = project.dir.path().join("b.xml");
    for (path, name) in [(&a, "a"), (&b, "b")] {
        std::fs::write(
            path,
            format!("<testsuite name=\"gears\"><testcase name=\"{name}\"/></testsuite>"),
        )
        .unwrap();
    }
    let remote_db = project.dir.path().join("remote.sqlite");
    for (database, files, source) in [
        (&remote_db, vec![&a], "remote-seed"),
        (&project.db, vec![&a, &b], "local-seed"),
    ] {
        Command::cargo_bin("greenshift")
            .unwrap()
            .env_remove("GREENSHIFT_HISTORY_TOKEN")
            .args(["ingest", "--db"])
            .arg(database)
            .args(["--commit", "abcdef1", "--source", source])
            .args(files)
            .assert()
            .success();
    }
    let retained_id: String = Connection::open(&project.db)
        .unwrap()
        .query_row(
            "SELECT history_id FROM runs WHERE source='local-seed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let repository = project.dir.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&repository)
        .output()
        .unwrap()
        .status
        .success());
    greenshift::history::Remote::new(repository.to_str().unwrap())
        .unwrap()
        .publish(&remote_db, None)
        .unwrap();
    let output = project.run(&["--history-remote", repository.to_str().unwrap()], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains(&retained_id) && diagnostics.contains("did not reconcile"),
        "{diagnostics}"
    );
    let runs = project.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].source, "local-seed");
    assert_eq!(runs[1].source, "nextest");
    assert_eq!(runs[1].results.len(), 5);
}

#[test]
fn a_fresh_store_runs_every_test_and_records_one_run() {
    let p = Project::new();
    let out = p.run(&["--commit", "ABCDEF1", "--", "--no-fail-fast"], &["gamma"]);
    // nextest's own exit status for failing tests.
    assert_eq!(out.status.code(), Some(100), "{}", stderr(&out));
    assert!(stderr(&out).contains("no history: run the full suite"));
    assert_eq!(
        p.runs(),
        [Run {
            source: "nextest".to_owned(),
            commit: Some("abcdef1".to_owned()),
            partial: false,
            // Every test is rich, so there is no poor run.
            files: 1,
            results: every_test(&["gamma"]),
        }]
    );
}

#[test]
fn rich_and_poor_both_run_and_are_recorded_as_one_run() {
    let p = Project::new();
    // A history in which only beta's failure is on record: beta is rich,
    // the rest poor.
    let args = ["--min-runs", "1", "--", "--no-fail-fast"];
    let first = p.run(&args, &["beta (b), c"]);
    assert_eq!(first.status.code(), Some(100), "{}", stderr(&first));
    let green = p.run(&args, &[]);
    assert!(green.status.success(), "{}", stderr(&green));
    // A failure in the poor group: the rich group passes, the poor one
    // still runs, and greenshift exits with its status.
    let out = p.run(&args, &["delta"]);
    assert_eq!(out.status.code(), Some(100), "{}", stderr(&out));
    let runs = p.runs();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[1].files, 2, "rich and poor ran");
    assert_eq!(runs[1].results, every_test(&[]));
    assert_eq!(runs[2].files, 2);
    assert!(!runs[2].partial);
    assert_eq!(runs[2].results, every_test(&["delta"]));
}

#[test]
fn the_poor_group_runs_after_the_rich_group_fails() {
    let p = Project::new();
    // As above: beta is rich, the rest poor.
    let args = ["--min-runs", "1", "--", "--no-fail-fast"];
    let first = p.run(&args, &["beta (b), c"]);
    assert_eq!(first.status.code(), Some(100), "{}", stderr(&first));
    let green = p.run(&args, &[]);
    assert!(green.status.success(), "{}", stderr(&green));
    // The rich group fails, under nextest's default fail-fast; the poor
    // group still runs, and both are recorded.
    let out = p.run(&["--min-runs", "1"], &["beta (b), c"]);
    assert_eq!(out.status.code(), Some(100), "{}", stderr(&out));
    let runs = p.runs();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[2].files, 2, "rich and poor ran");
    assert!(!runs[2].partial);
    assert_eq!(runs[2].results, every_test(&["beta (b), c"]));
}

#[test]
fn a_long_filterset_is_split_over_several_runs() {
    let p = Project::new();
    // Each test's term is over 40 bytes, so each runs alone.
    let out = p.run(&["--max-filterset-bytes", "40"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let runs = p.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].files, ONE.len() + TWO.len());
    assert!(!runs[0].partial);
    assert_eq!(runs[0].results, every_test(&[]));
}

#[test]
fn a_filterset_passed_on_chooses_the_tests() {
    let p = Project::new();
    // One run per test, each running only the test it names: the
    // filterset passed on chose them, and a run given it as well would
    // run both.
    let args = [
        "--max-filterset-bytes",
        "40",
        "--",
        "-E",
        "test(/^(alpha|delta)$/)",
    ];
    let out = p.run(&args, &[]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(err.matches(" 1 test run: 1 passed").count(), 2, "{err}");
    let runs = p.runs();
    assert_eq!(runs[0].files, 2);
    let names: Vec<&str> = runs[0]
        .results
        .iter()
        .map(|(_, name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "delta"]);
}

#[test]
fn a_store_that_cannot_be_read_still_runs_every_test() {
    let p = Project::new();
    std::fs::write(&p.db, "not a greenshift store, nor SQLite at all").unwrap();
    let out = p.run(&["--", "--no-fail-fast"], &["epsilon"]);
    let err = stderr(&out);
    // The tests' status, not the store's.
    assert_eq!(out.status.code(), Some(100), "{err}");
    assert!(err.contains("greenshift: error: history store"), "{err}");
    assert!(
        err.contains("greenshift: running every test as rich"),
        "{err}"
    );
    assert!(err.contains("5 tests run: 4 passed, 1 failed"), "{err}");
    assert_eq!(
        std::fs::read(&p.db).unwrap(),
        b"not a greenshift store, nor SQLite at all"
    );
}

#[test]
fn a_listing_that_fails_ends_the_run_with_its_status() {
    let p = Project::new();
    let out = p.run(&["--", "--no-such-option"], &[]);
    assert!(!out.status.success());
    assert!(!p.db.exists(), "nothing was recorded");
}

#[test]
fn the_run_is_recorded_with_its_branch() {
    let p = Project::new();
    // From the CI env when not given; the flag when given.
    let ci = p.run_in_env(&[], &[], &[("GITHUB_REF_NAME", "main")]);
    assert!(ci.status.success(), "{}", stderr(&ci));
    let given = p.run_in_env(
        &["--branch", "topic"],
        &[],
        &[("GITHUB_HEAD_REF", "feature")],
    );
    assert!(given.status.success(), "{}", stderr(&given));
    let none = p.run(&[], &[]);
    assert!(none.status.success(), "{}", stderr(&none));
    let branches: Vec<Option<String>> = Connection::open(&p.db)
        .unwrap()
        .prepare("SELECT branch FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        branches,
        [Some("main".to_owned()), Some("topic".to_owned()), None]
    );
}

#[test]
fn the_run_is_recorded_with_its_tag() {
    let p = Project::new();
    // From the CI env on a tag push; the flag when given.
    let tag_push = [("GITHUB_REF_NAME", "v1.2.0"), ("GITHUB_REF_TYPE", "tag")];
    let ci = p.run_in_env(&[], &[], &tag_push);
    assert!(ci.status.success(), "{}", stderr(&ci));
    let given = p.run_in_env(&["--tag", "v9"], &[], &[("GITHUB_REF_NAME", "main")]);
    assert!(given.status.success(), "{}", stderr(&given));
    let refs: Vec<(Option<String>, Option<String>)> = Connection::open(&p.db)
        .unwrap()
        .prepare("SELECT branch, tag FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        refs,
        [
            (None, Some("v1.2.0".to_owned())),
            (None, Some("v9".to_owned()))
        ]
    );
}
