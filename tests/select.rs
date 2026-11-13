//! Runs `greenshift select` on stores built by `greenshift ingest` from the
//! fixtures, ingested several times with varied outcomes.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn greenshift() -> Command {
    Command::cargo_bin("greenshift").expect("binary builds")
}

/// `xml` without any `<tag ...>...</tag>` element.
fn without(xml: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag} "), format!("</{tag}>"));
    let mut out = xml.to_owned();
    while let Some(start) = out.find(&open) {
        let end = start + out[start..].find(&close).unwrap() + close.len();
        out.replace_range(start..end, "");
    }
    out
}

struct History {
    dir: tempfile::TempDir,
    db: PathBuf,
    runs: usize,
}

impl History {
    fn new() -> History {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite");
        History { dir, db, runs: 0 }
    }

    fn ingest(&mut self, xml: &str, source: &str) {
        self.runs += 1;
        let file = self.dir.path().join(format!("run{}.xml", self.runs));
        std::fs::write(&file, xml).unwrap();
        greenshift()
            .arg("ingest")
            .arg("--db")
            .arg(&self.db)
            .args(["--source", source])
            .arg(&file)
            .assert()
            .success();
    }

    fn select(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        greenshift()
            .arg("select")
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .assert()
    }
}

/// Four nextest runs: as recorded (a failure and an error), green, the
/// error alone, green; then one vaelii run.
fn history() -> History {
    let nextest = fixture("nextest.xml");
    let green = without(&without(&nextest, "failure"), "error");
    let error_only = without(&nextest, "failure");
    let mut h = History::new();
    h.ingest(&nextest, "nextest");
    h.ingest(&green, "nextest");
    h.ingest(&error_only, "nextest");
    h.ingest(&green, "nextest");
    h.ingest(&fixture("vaelii-run-vars.xml"), "vaelii");
    h
}

fn stdout(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

#[test]
fn selects_from_one_source() {
    let h = history();
    let before = std::fs::read(&h.db).unwrap();
    let out = h
        .select(&["--source", "nextest", "--min-runs", "4"])
        .success();
    // Rows: run 1 (cog and gearbox), run 3 (gearbox); runs 2 and 4 are
    // green. The 12 ms cog goes before the 1457 ms gearbox. Confidence is
    // the Wilson lower bound for 2 green of 3 runs, then of 2.
    assert_eq!(
        stdout(&out),
        "rich\t50.0\t20.8\tsprocket-works\twidgets::rejects_a_bent_cog\n\
         rich\t100.0\t34.2\tsprocket-works::assembly\tassembles_a_gearbox\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::needs_a_lathe\n\
         poor\t100.0\t34.2\tsprocket-works::assembly\tlists_parts\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::parses_a_sprocket\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::spins_eventually\n"
    );
    assert_eq!(stderr(&out), "");
    assert_eq!(
        std::fs::read(&h.db).unwrap(),
        before,
        "select wrote to the store"
    );
    // Each nextest test has 4 results: with 5 needed, only rich is listed.
    let out = h
        .select(&["--source", "nextest", "--min-runs", "5"])
        .success();
    assert_eq!(stdout(&out).lines().count(), 2);
}

#[test]
fn selects_from_every_source_by_default() {
    let h = history();
    // The vaelii run is the newest full run, so its tests are the universe;
    // it is the only run any of them failed in. fixtures at 0 ms covers it,
    // leaving the 4 nextest runs, all green for this universe.
    let out = h
        .select(&["--min-runs", "1", "--estimator", "empirical"])
        .success();
    assert_eq!(
        stdout(&out),
        "rich\t100.0\t100.0\twidgets.flywheel-test\tfixtures\n\
         poor\t100.0\t100.0\tghost-gate\tunknown-var\n\
         poor\t100.0\t100.0\twidgets.flywheel-test\tstores-momentum\n\
         poor\t100.0\t100.0\twidgets.sprocket-test\tcounts-teeth\n\
         poor\t100.0\t100.0\twidgets.sprocket-test\tparses-a-sprocket\n\
         poor\t100.0\t100.0\twidgets.sprocket-test\trejects-a-bent-cog\n\
         poor\t100.0\t100.0\twidgets.sprocket-test\tspins-under-load\n"
    );
    // With the default --min-runs of 120, every test here is new; the
    // default estimator gives the Wilson lower bound for 4 of 4 green.
    let out = h.select(&[]).success();
    assert_eq!(
        stdout(&out),
        "rich\t100.0\t51.0\twidgets.flywheel-test\tfixtures\n"
    );
}

#[test]
fn confidence_ends_rich_early() {
    let h = history();
    let out = h
        .select(&[
            "--source",
            "nextest",
            "--min-runs",
            "4",
            "--confidence",
            "60",
            "--estimator",
            "empirical",
        ])
        .success();
    assert_eq!(
        stdout(&out),
        "rich\t50.0\t66.7\tsprocket-works\twidgets::rejects_a_bent_cog\n\
         poor\t100.0\t100.0\tsprocket-works\twidgets::needs_a_lathe\n\
         poor\t100.0\t100.0\tsprocket-works::assembly\tlists_parts\n\
         poor\t100.0\t100.0\tsprocket-works\twidgets::parses_a_sprocket\n\
         poor\t100.0\t100.0\tsprocket-works\twidgets::spins_eventually\n\
         poor\t100.0\t100.0\tsprocket-works::assembly\tassembles_a_gearbox\n"
    );
    assert_eq!(stderr(&out), "");
}

#[test]
fn an_unreached_confidence_keeps_the_whole_cover_and_says_so() {
    let h = history();
    let out = h
        .select(&[
            "--source",
            "nextest",
            "--min-runs",
            "5",
            "--confidence",
            "99",
            "--estimator",
            "smoothed",
        ])
        .success();
    // G = 2: (2 + 1) / (2 + 1 + 2), then (2 + 1) / (2 + 0 + 2).
    assert_eq!(
        stdout(&out),
        "rich\t50.0\t60.0\tsprocket-works\twidgets::rejects_a_bent_cog\n\
         rich\t100.0\t75.0\tsprocket-works::assembly\tassembles_a_gearbox\n"
    );
    assert_eq!(
        stderr(&out),
        "greenshift: confidence 99% not reached; the highest is 75.0%\n"
    );
}

#[test]
fn coverage_ends_rich_early() {
    let h = history();
    let out = h
        .select(&["--source", "nextest", "--min-runs", "4", "--coverage", "50"])
        .success();
    assert_eq!(
        stdout(&out),
        "rich\t50.0\t20.8\tsprocket-works\twidgets::rejects_a_bent_cog\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::needs_a_lathe\n\
         poor\t100.0\t34.2\tsprocket-works::assembly\tlists_parts\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::parses_a_sprocket\n\
         poor\t100.0\t34.2\tsprocket-works\twidgets::spins_eventually\n\
         poor\t100.0\t34.2\tsprocket-works::assembly\tassembles_a_gearbox\n"
    );
    assert_eq!(stderr(&out), "");
}

#[test]
fn a_reader_that_stops_early_is_not_an_error() {
    // More output than a pipe holds, so select is still writing when the
    // reader goes away.
    let cases: String = (0..5000)
        .map(|i| format!("<testcase classname=\"s\" name=\"t{i}\" time=\"0.1\"/>"))
        .collect();
    let mut h = History::new();
    h.ingest(
        &format!("<testsuites><testsuite name=\"s\">{cases}</testsuite></testsuites>"),
        "ci",
    );
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("greenshift"))
        .arg("select")
        .arg("--db")
        .arg(&h.db)
        .args(["--min-runs", "1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 4];
    std::io::Read::read_exact(child.stdout.as_mut().unwrap(), &mut first).unwrap();
    assert_eq!(&first, b"poor");
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
}

#[test]
fn with_both_thresholds_rich_ends_where_the_later_is_reached() {
    let h = history();
    // 50% coverage comes at the first pick, 30% confidence at the second.
    let out = h
        .select(&[
            "--source",
            "nextest",
            "--min-runs",
            "5",
            "--coverage",
            "50",
            "--confidence",
            "30",
        ])
        .success();
    assert_eq!(
        stdout(&out),
        "rich\t50.0\t20.8\tsprocket-works\twidgets::rejects_a_bent_cog\n\
         rich\t100.0\t34.2\tsprocket-works::assembly\tassembles_a_gearbox\n"
    );
    assert_eq!(stderr(&out), "");
    // A confidence never reached keeps the whole cover, whatever the
    // coverage, and stderr names it.
    let out = h
        .select(&[
            "--source",
            "nextest",
            "--min-runs",
            "5",
            "--coverage",
            "50",
            "--confidence",
            "50",
        ])
        .success();
    assert_eq!(stdout(&out).lines().count(), 2);
    assert_eq!(
        stderr(&out),
        "greenshift: confidence 50% not reached; the highest is 34.2%\n"
    );
}

#[test]
fn no_full_run_exits_2() {
    let h = history();
    let out = h.select(&["--source", "elsewhere"]).code(2);
    assert_eq!(stdout(&out), "");
    assert_eq!(stderr(&out), "no history: run the full suite\n");

    // No store at all: the same, and nothing is created.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sub/history.sqlite");
    let out = greenshift()
        .arg("select")
        .arg("--db")
        .arg(&db)
        .assert()
        .code(2);
    assert_eq!(stdout(&out), "");
    assert_eq!(stderr(&out), "no history: run the full suite\n");
    assert!(!dir.path().join("sub").exists());
}

#[test]
fn a_file_that_is_not_a_store_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("notes.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT);")
        .unwrap();
    drop(conn);
    let out = greenshift()
        .arg("select")
        .arg("--db")
        .arg(&db)
        .assert()
        .code(1);
    assert!(
        stderr(&out).contains("not a greenshift history store") && !stderr(&out).contains("write"),
        "{}",
        stderr(&out)
    );
}
