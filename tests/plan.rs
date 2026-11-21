//! Runs `greenshift plan` on stores built by `greenshift ingest` from
//! generated JUnit, with names that `select` has to escape for display.

use std::collections::BTreeSet;
use std::path::PathBuf;

use assert_cmd::Command;

/// The binary, without the CI branch and tag variables of the environment
/// this test runs in, so a run records neither unless a test sets one.
fn greenshift() -> Command {
    let mut cmd = Command::cargo_bin("greenshift").expect("binary builds");
    for var in greenshift::cli::CI_REF_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// `text` as an XML attribute value.
fn attr(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

/// A JUnit report of `tests` as `(suite, name, failed)`, each taking 5 ms.
fn junit(tests: &[(&str, &str, bool)]) -> String {
    let mut xml = String::from("<testsuites>\n<testsuite name=\"run\">\n");
    for &(suite, name, failed) in tests {
        xml.push_str(&format!(
            "<testcase classname=\"{}\" name=\"{}\" time=\"0.005\">{}</testcase>\n",
            attr(suite),
            attr(name),
            if failed { "<failure/>" } else { "" }
        ));
    }
    xml.push_str("</testsuite>\n</testsuites>\n");
    xml
}

/// `tests` as an inventory: one JSON object per line.
fn inventory(tests: &[(&str, &str)]) -> String {
    tests
        .iter()
        .map(|&(suite, name)| serde_json::json!({"suite": suite, "name": name}).to_string() + "\n")
        .collect()
}

const COMMA: &str = "a, b (c)";
const BACKSLASH: &str = r"back\slash/x";
const TAB: &str = "tab\there";
const UNI: &str = "uni \u{2713} \u{fc}n\u{ef}";

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

    fn ingest(&mut self, tests: &[(&str, &str, bool)]) {
        self.runs += 1;
        // Distinct observations at distinct commits may have identical results.
        let commit = format!("{:07x}", self.runs);
        let file = self.dir.path().join(format!("run{}.xml", self.runs));
        std::fs::write(&file, junit(tests)).unwrap();
        greenshift()
            .arg("ingest")
            .arg("--db")
            .arg(&self.db)
            .args(["--commit", &commit])
            .arg(&file)
            .assert()
            .success();
    }

    fn write_inventory(&self, text: &str) -> PathBuf {
        let path = self.dir.path().join("inventory.jsonl");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn plan(&self, inventory: &str, args: &[&str]) -> assert_cmd::assert::Assert {
        let path = self.write_inventory(inventory);
        greenshift()
            .arg("plan")
            .arg("--db")
            .arg(&self.db)
            .arg("--inventory")
            .arg(path)
            .args(args)
            .assert()
    }
}

/// Three runs of the `kit` suite: the first fails [`COMMA`], and the last
/// two add `late`. `gone` is in every run.
fn history() -> History {
    let mut h = History::new();
    let base = [
        ("kit", COMMA, false),
        ("kit", BACKSLASH, false),
        ("kit", TAB, false),
        ("kit", UNI, false),
        ("kit", "gone", false),
    ];
    let mut first = base;
    first[0].2 = true;
    h.ingest(&first);
    let mut later = base.to_vec();
    later.push(("kit", "late", false));
    h.ingest(&later);
    h.ingest(&later);
    h
}

/// The tests now: every stored test but `gone`, plus `fresh`, which the
/// store has never seen, and `kit2`'s `late`, a stored name in another
/// suite.
const NOW: &[(&str, &str)] = &[
    ("kit", UNI),
    ("kit", "fresh"),
    ("kit", TAB),
    ("kit", COMMA),
    ("kit", "late"),
    ("kit", BACKSLASH),
    ("kit2", "late"),
];

fn stdout(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

type Groups = (Vec<(String, String)>, Vec<(String, String)>);

/// The history state of a plan, and its rich and poor groups as
/// `(suite, name)`s.
fn read_plan(out: &str) -> (String, Groups) {
    assert!(out.ends_with('\n') && out.lines().count() == 1, "{out:?}");
    assert!(
        out.starts_with(r#"{"history":"#),
        "history is not first: {out}"
    );
    let plan: serde_json::Value = serde_json::from_str(out).unwrap();
    let group = |key: &str| {
        plan[key]
            .as_array()
            .unwrap_or_else(|| panic!("no {key} array: {out}"))
            .iter()
            .map(|t| {
                (
                    t["suite"].as_str().unwrap().to_owned(),
                    t["name"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(plan.as_object().unwrap().len(), 3, "{out}");
    let history = plan["history"]
        .as_str()
        .unwrap_or_else(|| panic!("no history string: {out}"))
        .to_owned();
    (history, (group("rich"), group("poor")))
}

/// The rich and poor groups of a plan whose history is `ok`.
fn groups(out: &str) -> Groups {
    let (history, groups) = read_plan(out);
    assert_eq!(history, "ok", "{out}");
    groups
}

fn owned(tests: &[(&str, &str)]) -> Vec<(String, String)> {
    tests
        .iter()
        .map(|&(s, n)| (s.to_owned(), n.to_owned()))
        .collect()
}

/// Each test of `now` is in exactly one group, and nothing else is.
fn assert_partition((rich, poor): &Groups, now: &[(&str, &str)]) {
    let all: Vec<&(String, String)> = rich.iter().chain(poor).collect();
    let distinct: BTreeSet<&(String, String)> = all.iter().copied().collect();
    assert_eq!(distinct.len(), all.len(), "a test is planned twice");
    let want = owned(now);
    assert_eq!(distinct, want.iter().collect(), "not the inventory");
}

#[test]
fn plans_the_inventory_from_history() {
    let h = history();
    let out = h.plan(&inventory(NOW), &["--min-runs", "3"]).success();
    let plan = groups(&stdout(&out));
    // COMMA covers the one failing run. `late` has 2 results, under
    // --min-runs, and `fresh` and kit2's `late` have none: all rich, in
    // inventory order. Poor is select's order: same median, so by name.
    assert_eq!(
        plan.0,
        owned(&[
            ("kit", COMMA),
            ("kit", "fresh"),
            ("kit", "late"),
            ("kit2", "late")
        ])
    );
    assert_eq!(
        plan.1,
        owned(&[("kit", BACKSLASH), ("kit", TAB), ("kit", UNI)])
    );
    assert_partition(&plan, NOW);
    assert_eq!(stderr(&out), "");
}

#[test]
fn names_are_written_raw_not_as_select_shows_them() {
    let h = history();
    let select = greenshift()
        .arg("select")
        .arg("--db")
        .arg(&h.db)
        .args(["--min-runs", "3"])
        .assert()
        .success();
    assert!(stdout(&select).contains(r"tab\there"));
    assert!(stdout(&select).contains(r"back\\slash"));
    let out = stdout(&h.plan(&inventory(NOW), &["--min-runs", "3"]).success());
    // The JSON string escapes are JSON's: a tab is \t, a backslash \\.
    assert!(
        out.contains(r#"{"suite":"kit","name":"tab\there"}"#),
        "{out}"
    );
    assert!(
        out.contains(r#"{"suite":"kit","name":"back\\slash/x"}"#),
        "{out}"
    );
    assert!(out.contains(UNI), "{out}");
}

#[test]
fn under_observed_tests_are_rich() {
    // With the default --min-runs 120 no test is poor.
    let h = history();
    let plan = groups(&stdout(&h.plan(&inventory(NOW), &[]).success()));
    assert_eq!(plan.0[0], ("kit".to_owned(), COMMA.to_owned()));
    assert!(plan.1.is_empty(), "{plan:?}");
    assert_partition(&plan, NOW);
}

#[test]
fn a_missing_store_is_created_and_every_test_is_rich() {
    let mut h = History::new();
    h.db = h.dir.path().join("new/dir/history.sqlite");
    let out = h.plan(&inventory(NOW), &[]).success();
    let plan = groups(&stdout(&out));
    assert_eq!(plan.0, owned(NOW));
    assert!(plan.1.is_empty());
    assert_eq!(stderr(&out), "no history: run the full suite\n");
    // The store now exists, marked as greenshift's.
    let conn =
        rusqlite::Connection::open_with_flags(&h.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("the store was created");
    let application_id: i32 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .unwrap();
    assert_eq!(application_id, greenshift::store::APPLICATION_ID);
    drop(conn);
    // A greenshift store with no runs: select reads it as no history.
    greenshift()
        .arg("select")
        .arg("--db")
        .arg(&h.db)
        .assert()
        .code(2);
    h.ingest(&[("kit", "late", false)]);
}

#[test]
fn a_store_with_no_runs_is_ok_and_every_test_is_rich() {
    let h = History::new();
    // Created empty by the first plan; an empty file is no history too.
    h.plan(&inventory(NOW), &[]).success();
    let empty_file = h.dir.path().join("empty.sqlite");
    std::fs::write(&empty_file, "").unwrap();
    for db in [&h.db, &empty_file] {
        let before = std::fs::read(db).unwrap();
        let out = greenshift()
            .arg("plan")
            .arg("--db")
            .arg(db)
            .arg("--inventory")
            .arg(h.write_inventory(&inventory(NOW)))
            .assert()
            .success();
        let plan = groups(&stdout(&out));
        assert_eq!(plan.0, owned(NOW), "{db:?}");
        assert!(plan.1.is_empty(), "{db:?}");
        assert_eq!(stderr(&out), "no history: run the full suite\n");
        // An existing store is only read: never migrated or written.
        assert_eq!(std::fs::read(db).unwrap(), before, "{db:?} was written");
    }
}

#[test]
fn an_empty_inventory_plans_nothing() {
    let h = history();
    for text in ["", "\n\n"] {
        let out = h.plan(text, &["--min-runs", "3"]).success();
        assert_eq!(
            stdout(&out),
            "{\"history\":\"ok\",\"rich\":[],\"poor\":[]}\n"
        );
    }
}

#[test]
fn the_inventory_can_come_from_stdin() {
    let h = history();
    let out = greenshift()
        .arg("plan")
        .arg("--db")
        .arg(&h.db)
        .args(["--min-runs", "3", "--inventory", "-"])
        .write_stdin(inventory(NOW))
        .assert()
        .success();
    assert_partition(&groups(&stdout(&out)), NOW);
}

/// Three runs of `kit`: `left` fails the first, `right` the second, and
/// `still` never fails, so the cover needs two picks.
fn two_pick_history() -> History {
    let mut h = History::new();
    h.ingest(&[
        ("kit", "left", true),
        ("kit", "right", false),
        ("kit", "still", false),
    ]);
    h.ingest(&[
        ("kit", "left", false),
        ("kit", "right", true),
        ("kit", "still", false),
    ]);
    h.ingest(&[
        ("kit", "left", false),
        ("kit", "right", false),
        ("kit", "still", false),
    ]);
    h
}

const PICKS: &[(&str, &str)] = &[("kit", "still"), ("kit", "right"), ("kit", "left")];

#[test]
fn coverage_ends_rich_early() {
    let h = two_pick_history();
    let whole = groups(&stdout(
        &h.plan(&inventory(PICKS), &["--min-runs", "3"]).success(),
    ));
    // The whole cover is rich, then the rest in inventory order.
    assert_eq!(whole.0.len(), 2, "{whole:?}");
    assert!(!whole.0.contains(&("kit".to_owned(), "still".to_owned())));
    assert_eq!(whole.1, owned(&[("kit", "still")]));
    // Half the failing runs are caught by the first pick alone.
    let out = h
        .plan(&inventory(PICKS), &["--min-runs", "3", "--coverage", "50"])
        .success();
    let half = groups(&stdout(&out));
    assert_eq!(half.0, whole.0[..1], "{half:?}");
    assert_eq!(half.1.len(), 2, "{half:?}");
    assert!(half.1.contains(&whole.0[1]), "{half:?}");
    assert_partition(&half, PICKS);
    assert_eq!(stderr(&out), "");
}

#[test]
fn the_cover_is_chosen_from_the_inventory_not_cut_down_to_it() {
    // A fails with B in the first run and with C in the second, so over
    // the whole store A alone is the cover, and B and C are poor.
    let mut h = History::new();
    h.ingest(&[("s", "A", true), ("s", "B", true), ("s", "C", false)]);
    h.ingest(&[("s", "A", true), ("s", "B", false), ("s", "C", true)]);
    h.ingest(&[("s", "A", false), ("s", "B", false), ("s", "C", false)]);
    let select = greenshift()
        .arg("select")
        .arg("--db")
        .arg(&h.db)
        .args(["--min-runs", "1"])
        .assert()
        .success();
    let lines = stdout(&select);
    let classes: Vec<(&str, &str)> = lines
        .lines()
        .map(|line| {
            (
                line.split('\t').next().unwrap(),
                line.rsplit('\t').next().unwrap(),
            )
        })
        .collect();
    assert_eq!(classes, [("rich", "A"), ("poor", "B"), ("poor", "C")]);
    // With A gone, B is the witness left for the first run and C for the
    // second: both rich, none poor.
    let now = &[("s", "B"), ("s", "C")];
    let out = h.plan(&inventory(now), &["--min-runs", "1"]).success();
    let (rich, poor) = groups(&stdout(&out));
    let rich: BTreeSet<(String, String)> = rich.into_iter().collect();
    assert_eq!(rich, owned(now).into_iter().collect());
    assert_eq!(poor, owned(&[]));
}

#[test]
fn a_run_failing_only_a_test_gone_from_the_inventory_is_not_green() {
    // B fails in the first run and C in the second; the third fails only
    // A, which is gone; the fourth is green.
    let mut h = History::new();
    h.ingest(&[("s", "A", true), ("s", "B", true), ("s", "C", false)]);
    h.ingest(&[("s", "A", false), ("s", "B", false), ("s", "C", true)]);
    h.ingest(&[("s", "A", true), ("s", "B", false), ("s", "C", false)]);
    h.ingest(&[("s", "A", false), ("s", "B", false), ("s", "C", false)]);
    let now = &[("s", "B"), ("s", "C")];
    // Once B passes, one row is left, and only the fourth run is green:
    // empirical confidence 1/2, under 60%, so C is rich too. Were the
    // third run green, B alone would reach 2/3 and C would be poor.
    let out = h
        .plan(
            &inventory(now),
            &[
                "--min-runs",
                "1",
                "--estimator",
                "empirical",
                "--confidence",
                "60",
            ],
        )
        .success();
    let (rich, poor) = groups(&stdout(&out));
    assert_eq!(rich, owned(now));
    assert_eq!(poor, owned(&[]));
    assert_eq!(stderr(&out), "");
}

#[test]
fn an_unreached_threshold_is_named_on_stderr() {
    let h = two_pick_history();
    let out = h
        .plan(
            &inventory(PICKS),
            &["--min-runs", "3", "--confidence", "99.9"],
        )
        .success();
    let plan = groups(&stdout(&out));
    assert_eq!(plan.0.len(), 2, "{plan:?}");
    assert_partition(&plan, PICKS);
    let err = stderr(&out);
    assert!(
        err.starts_with("greenshift: confidence 99.9% not reached; the highest is ")
            && err.ends_with("%\n")
            && err.lines().count() == 1,
        "{err}"
    );
}

#[test]
fn a_malformed_inventory_is_an_error() {
    let h = history();
    let good = inventory(&[("kit", "fresh")]);
    for (text, line) in [
        ("not json\n".to_owned(), 1),
        (format!("{good}{{\"suite\":\"kit\"}}\n"), 2),
        (format!("{good}{good}[\"kit\",\"x\"]\n"), 3),
        ("{\"suite\":\"kit\",\"name\":null}\n".to_owned(), 1),
    ] {
        let out = h.plan(&text, &["--min-runs", "3"]).failure().code(1);
        assert_eq!(stdout(&out), "", "{text:?}");
        let err = stderr(&out);
        assert!(
            err.contains(&format!(": not a greenshift inventory: line {line}: ")),
            "{text:?}: {err}"
        );
    }
}

#[test]
fn an_unreadable_inventory_is_an_error() {
    let h = history();
    let out = greenshift()
        .arg("plan")
        .arg("--db")
        .arg(&h.db)
        .arg("--inventory")
        .arg(h.dir.path().join("missing.jsonl"))
        .assert()
        .failure()
        .code(1);
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("inventory "), "{}", stderr(&out));
}

#[test]
fn the_inventory_is_required() {
    let h = history();
    let out = greenshift()
        .arg("plan")
        .arg("--db")
        .arg(&h.db)
        .assert()
        .failure()
        .code(2);
    assert!(
        stderr(&out).contains("--inventory <FILE>"),
        "{}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), "");
}

/// A plan of [`NOW`] on a store `h` cannot use: history `error`, every
/// test rich, exit 0, and one line on stderr naming the error, which
/// says `why`.
fn assert_store_error(h: &History, why: &str) {
    let out = h.plan(&inventory(NOW), &["--min-runs", "1"]).success();
    let (history, plan) = read_plan(&stdout(&out));
    assert_eq!(history, "error");
    assert_eq!(plan.0, owned(NOW));
    assert!(plan.1.is_empty(), "{plan:?}");
    let err = stderr(&out);
    assert!(
        err.starts_with("greenshift: error: ") && err.ends_with('\n') && err.lines().count() == 1,
        "{err}"
    );
    assert!(err.contains(why), "{why:?} not in {err}");
}

/// A store under a file: whether it exists cannot be checked, so it is
/// read, and the read fails.
#[test]
fn a_store_under_a_file_is_an_error_and_every_test_is_rich() {
    let mut h = History::new();
    let file = h.dir.path().join("a-file");
    std::fs::write(&file, "").unwrap();
    h.db = file.join("history.sqlite");
    assert_store_error(&h, &format!("{}", file.display()));
    assert!(!h.db.exists());
}

/// A store known to be missing whose directory cannot be made: its
/// parent is a symlink to nowhere, so the store reads as missing and the
/// creation, not the read, fails.
#[cfg(unix)]
#[test]
fn a_missing_store_that_cannot_be_created_is_an_error_and_every_test_is_rich() {
    let mut h = History::new();
    let dangling = h.dir.path().join("dangling");
    std::os::unix::fs::symlink(h.dir.path().join("nowhere/deeper"), &dangling).unwrap();
    h.db = dangling.join("history.sqlite");
    assert!(matches!(h.db.try_exists(), Ok(false)));
    assert_store_error(&h, &format!("could not create {}", dangling.display()));
    assert!(!h.dir.path().join("nowhere").exists());
}

#[test]
fn a_bad_store_is_an_error_and_every_test_is_rich() {
    use greenshift::store::{APPLICATION_ID, SCHEMA_VERSION};
    let h = History::new();
    std::fs::write(&h.db, "not sqlite").unwrap();
    assert_store_error(&h, "file is not a database");
    for (application_id, user_version, why) in [
        (0, 0, "is not a greenshift history store".to_owned()),
        (
            APPLICATION_ID,
            SCHEMA_VERSION + 1,
            format!("has schema version {}, newer", SCHEMA_VERSION + 1),
        ),
    ] {
        let h = History::new();
        let conn = rusqlite::Connection::open(&h.db).unwrap();
        conn.execute("CREATE TABLE theirs (x)", []).unwrap();
        conn.pragma_update(None, "application_id", application_id)
            .unwrap();
        conn.pragma_update(None, "user_version", user_version)
            .unwrap();
        drop(conn);
        assert_store_error(&h, &why);
    }
}

#[test]
fn a_store_that_is_a_directory_is_an_error_and_every_test_is_rich() {
    let h = History::new();
    std::fs::create_dir(&h.db).unwrap();
    assert_store_error(&h, &format!("{}", h.db.display()));
}

#[test]
fn a_poor_name_in_another_suite_is_new_and_rich() {
    let h = history();
    let now = [("kit2", TAB), ("kit", TAB), ("", TAB)];
    let plan = groups(&stdout(
        &h.plan(&inventory(&now), &["--min-runs", "3"]).success(),
    ));
    assert_eq!(plan.0, owned(&[("kit2", TAB), ("", TAB)]));
    assert_eq!(plan.1, owned(&[("kit", TAB)]));
    assert_partition(&plan, &now);
}

/// One text and two that differ from it only in one whitespace or
/// invisible character: a space, a tab and a zero-width space.
const SPACED: [&str; 3] = ["a b", "a\tb", "a\u{200b}b"];

/// History of the test `stored`, three runs, failing in the first when
/// `fails`, then the plan of `now` and its raw output.
fn plan_near_twins(stored: (&str, &str), fails: bool, now: &[(&str, &str)]) -> (Groups, String) {
    let mut h = History::new();
    for run in 0..3 {
        h.ingest(&[
            (stored.0, stored.1, fails && run == 0),
            ("kit", "steady", false),
        ]);
    }
    let out = stdout(&h.plan(&inventory(now), &["--min-runs", "3"]).success());
    let plan = groups(&out);
    assert_partition(&plan, now);
    (plan, out)
}

/// Identity is the exact suite and name: tests that differ only in one
/// whitespace or invisible character are different tests, so only the
/// stored one follows its history and the others are new, so rich.
fn assert_near_twins_are_distinct(test: impl Fn(&'static str) -> (&'static str, &'static str)) {
    let [space, tab, zero_width] = SPACED.map(&test);
    // The stored test last, so a rich pick shows by coming first.
    let now = [tab, zero_width, space];
    // Failing once, the stored test is select's rich pick: first.
    let (plan, out) = plan_near_twins(space, true, &now);
    assert_eq!(plan.0, owned(&[space, tab, zero_width]), "{out}");
    assert!(plan.1.is_empty(), "{out}");
    // Always passing, it is poor; the others are still rich.
    let (plan, out) = plan_near_twins(space, false, &now);
    assert_eq!(plan.0, owned(&[tab, zero_width]), "{out}");
    assert_eq!(plan.1, owned(&[space]), "{out}");
    // The JSON keeps them apart, the tab and U+200B as JSON escapes.
    for raw in ["a b", r"a\tb", concat!(r"a\u", "200b", "b")] {
        assert_eq!(
            out.matches(&format!("\"{raw}\"")).count(),
            1,
            "{raw}: {out}"
        );
    }
}

#[test]
fn names_that_differ_only_in_whitespace_are_different_tests() {
    assert_near_twins_are_distinct(|name| ("kit", name));
}

#[test]
fn suites_that_differ_only_in_whitespace_are_different_tests() {
    assert_near_twins_are_distinct(|suite| (suite, "case"));
}

#[test]
fn a_store_with_runs_is_only_read() {
    let h = history();
    let before = std::fs::read(&h.db).unwrap();
    h.plan(&inventory(NOW), &["--min-runs", "3"]).success();
    assert_eq!(
        std::fs::read(&h.db).unwrap(),
        before,
        "the store was written"
    );
}

#[test]
fn a_bad_inventory_creates_no_store() {
    let mut h = History::new();
    h.db = h.dir.path().join("new/history.sqlite");
    let out = h.plan("not json\n", &[]).failure().code(1);
    assert_eq!(stdout(&out), "");
    assert!(!h.dir.path().join("new").exists(), "the store was created");
}
