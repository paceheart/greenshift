//! Runs `greenshift run lein` and `greenshift init lein` on a scratch
//! Leiningen project with real lein, and checks what it listed, ran and
//! recorded (greenshift#35's I1 to I7).
//!
//! Needs a JDK and lein on PATH; without them every test here is skipped,
//! unless `GREENSHIFT_REQUIRE_LEIN` is set (as greenshift's CI sets it),
//! when a missing lein fails them. The tests take turns, through a file
//! lock, so also when nextest runs them as processes of their own: each
//! lein run is two JVMs (lein's own and the project's), and no more run
//! at once. Each JVM's heap is capped at 1 GiB.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Output;
use std::time::Instant;

use rusqlite::Connection;

/// A turn to run lein: held until dropped. One test holds it at a time,
/// in this process or another.
struct Turn(#[allow(dead_code)] std::fs::File);

fn turn() -> Turn {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("run-lein.lock");
    let file = std::fs::File::create(path).unwrap();
    file.lock().unwrap();
    Turn(file)
}

const JVM_OPTS: &str = "-Xmx1g";

const PROJECT: &str = r#"(defproject scratch "0.0.0"
  :dependencies [[org.clojure/clojure "1.12.6"]]
  :jvm-opts ["-Xmx1g"]
  :global-vars {*warn-on-reflection* true}
  :preserve-eval-meta true
  :profiles {:test {:dependencies [[org.clojure/test.check "1.1.3"]]
                    :injections [(println "profile-injection-ran")]}}
  :test-selectors {:default (fn [m & _] (not (or (:slow m) (:red m))))
                   :slow    (fn [m & _] (boolean (:slow m)))
                   :red     (fn [m & _] (boolean (:red m)))})
"#;

/// Every namespace starts with this: it notes that it loaded, in the
/// file `LOADED` names, and fails a test whose name `FAIL` has a line for.
const SUPPORT: &str = r#"(ns scratch.support (:require [clojure.string :as str]))
(defn loaded! [ns-sym]
  (when-let [file (System/getenv "LOADED")]
    (spit file (str ns-sym "\n") :append true)))
(defn fail? [name]
  (boolean (some #{(str name)} (str/split-lines (or (System/getenv "FAIL") "")))))
"#;

const NAMESPACES: &[(&str, &str)] = &[
    (
        "plain_test",
        r#"(ns scratch.plain-test (:require [clojure.test :refer :all] [scratch.support :as s]))
(s/loaded! 'scratch.plain-test)
(deftest adds (is (not (s/fail? "adds"))) (is (= 2 (+ 1 1))))
(deftest flips (is (not (s/fail? "flips"))))
(deftest- kept-private (is (not (s/fail? "kept-private"))))
(deftest ^:slow takes-long (is true))
(deftest ^:red not-yet (is false "red on purpose"))
"#,
    ),
    (
        "slow_ns_test",
        r#"(ns ^:slow scratch.slow-ns-test (:require [clojure.test :refer :all] [scratch.support :as s]))
(s/loaded! 'scratch.slow-ns-test)
(deftest whole-ns-is-slow (is true))
"#,
    ),
    (
        "macro_test",
        r#"(ns scratch.macro-test
  (:require [clojure.test :refer :all] [scratch.support :as s]
            [clojure.test.check.clojure-test :refer [defspec]]
            [clojure.test.check.generators :as gen]
            [clojure.test.check.properties :as prop]))
(s/loaded! 'scratch.macro-test)
(defmacro deftest-twice [name] `(deftest ~name (is (not (s/fail? ~(str name)))) (is true)))
(deftest-twice made-by-macro)
(defspec a-property 5 (prop/for-all [x gen/small-integer] (= x (identity x))))
"#,
    ),
    (
        "fixture_test",
        r#"(ns scratch.fixture-test (:require [clojure.test :refer :all] [scratch.support :as s]))
(s/loaded! 'scratch.fixture-test)
(use-fixtures :once (fn [f] (throw (ex-info "fixture boom" {}))))
(deftest never-runs (is true))
"#,
    ),
    (
        "errors_test",
        r#"(ns scratch.errors-test (:require [clojure.test :refer :all] [scratch.support :as s]))
(s/loaded! 'scratch.errors-test)
(use-fixtures :once (fn [f] (is (= :fixture :fixture) "an assertion in a fixture") (f)))
(deftest fails (is (= 1 2) "one is not two"))
(deftest throws (throw (IllegalStateException. "gear <7> jammed")))
(deftest passes (is true))
"#,
    ),
];

/// The tests `lein test` selects by default: not `^:slow` (by var or by
/// namespace), not `^:red`.
const DEFAULT: &[(&str, &str)] = &[
    ("scratch.errors-test", "fails"),
    ("scratch.errors-test", "passes"),
    ("scratch.errors-test", "throws"),
    ("scratch.fixture-test", "never-runs"),
    ("scratch.macro-test", "a-property"),
    ("scratch.macro-test", "made-by-macro"),
    ("scratch.plain-test", "adds"),
    ("scratch.plain-test", "flips"),
    ("scratch.plain-test", "kept-private"),
];

/// Whether lein can run here, or else why the tests are skipped.
fn lein_ready() -> bool {
    let ok = std::process::Command::new("lein")
        .arg("version")
        .env("LEIN_JVM_OPTS", JVM_OPTS)
        .output()
        .is_ok_and(|out| out.status.success());
    if !ok {
        assert!(
            std::env::var_os("GREENSHIFT_REQUIRE_LEIN").is_none(),
            "GREENSHIFT_REQUIRE_LEIN is set, but `lein version` does not run"
        );
        eprintln!("skipped: no working lein (and JDK) on PATH");
    }
    ok
}

struct Project {
    dir: tempfile::TempDir,
    db: PathBuf,
}

/// One stored run: source, partial, JUnit files, and its results as
/// `(suite, name, outcome)`, sorted.
#[derive(Debug, PartialEq)]
struct Run {
    source: String,
    partial: bool,
    files: usize,
    results: Vec<(String, String, String)>,
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src/scratch")).unwrap();
        std::fs::create_dir_all(root.join("test/scratch")).unwrap();
        std::fs::write(root.join("project.clj"), PROJECT).unwrap();
        std::fs::write(root.join("src/scratch/support.clj"), SUPPORT).unwrap();
        for (file, text) in NAMESPACES {
            std::fs::write(root.join(format!("test/scratch/{file}.clj")), text).unwrap();
        }
        let db = dir.path().join("history.sqlite");
        Project { dir, db }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn env(&self, cmd: &mut std::process::Command) {
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().into_owned();
            if key.starts_with("GREENSHIFT_")
                || key.starts_with("GITHUB_")
                || key == "LEIN_TEST_SELECTOR"
                || key == "JVM_OPTS"
            {
                cmd.env_remove(&key);
            }
        }
        cmd.current_dir(self.root()).env("LEIN_JVM_OPTS", JVM_OPTS);
    }

    fn greenshift(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("greenshift"));
        self.env(&mut cmd);
        cmd.envs(env.iter().copied()).args(args).output().unwrap()
    }

    /// `greenshift init lein` in the project.
    fn init(&self) {
        let out = self.greenshift(&["init", "lein"], &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
    }

    /// `greenshift run lein --db <db> <args>`, failing the tests `fail`
    /// names.
    fn run(&self, args: &[&str], fail: &[&str]) -> Output {
        let fail: String = fail.iter().map(|name| format!("{name}\n")).collect();
        let db = self.db.to_str().unwrap();
        let mut all = vec!["run", "lein", "--db", db];
        all.extend(args);
        self.greenshift(&all, &[("FAIL", &fail)])
    }

    /// `lein <args>` in the project, its environment as for greenshift.
    fn lein(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = std::process::Command::new("lein");
        self.env(&mut cmd);
        cmd.envs(env.iter().copied()).args(args).output().unwrap()
    }

    fn runs(&self) -> Vec<Run> {
        let conn = Connection::open(&self.db).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, source, partial, \
                 (SELECT count(*) FROM run_files f WHERE f.run_id = runs.id) \
                 FROM runs ORDER BY id",
            )
            .unwrap();
        let runs: Vec<(i64, Run)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    Run {
                        source: row.get(1)?,
                        partial: row.get::<_, i64>(2)? == 1,
                        files: row.get::<_, i64>(3)? as usize,
                        results: Vec::new(),
                    },
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        runs.into_iter()
            .map(|(id, mut run)| {
                let mut stmt = conn
                    .prepare(
                        "SELECT t.suite, t.name, r.outcome FROM results r \
                         JOIN tests t ON t.id = r.test_id WHERE r.run_id = ?1 \
                         ORDER BY t.suite, t.name",
                    )
                    .unwrap();
                run.results = stmt
                    .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                run
            })
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every default test with the outcome a run gives it, failing `failed`
/// too, plus the two fixtures testcases.
fn expected_results(failed: &[&str]) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = DEFAULT
        .iter()
        .map(|&(suite, name)| {
            let outcome = match name {
                "fails" => "failed",
                "throws" => "error",
                "never-runs" => "skipped",
                name if failed.contains(&name) => "failed",
                _ => "passed",
            };
            (suite.to_owned(), name.to_owned(), outcome.to_owned())
        })
        .collect();
    out.push((
        "scratch.errors-test".into(),
        "fixtures".into(),
        "passed".into(),
    ));
    out.push((
        "scratch.fixture-test".into(),
        "fixtures".into(),
        "error".into(),
    ));
    out.sort();
    out
}

#[test]
fn i1_i2_the_listing_is_leins_default_selection_with_the_profile_merged() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    let listing = project.dir.path().join("listing.jsonl");
    let script = project.dir.path().join("list.clj");
    std::fs::write(
        &script,
        greenshift::run::lein::list_script(&listing).unwrap(),
    )
    .unwrap();
    let injection = greenshift::run::lein::injection(&script).unwrap();
    let started = Instant::now();
    let out = project.lein(
        &[
            "update-in",
            ":injections",
            "conj",
            injection.to_str().unwrap(),
            "--",
            "test",
        ],
        &[],
    );
    let took = started.elapsed();
    eprintln!("I2: the scratch listing took {took:.1?}");
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    // I1: the :test profile's own injection ran as well as greenshift's.
    assert!(text(&out.stdout).contains("profile-injection-ran"));
    let tests = greenshift::run::lein::read_listing(&listing).unwrap();
    let listed: BTreeSet<(String, String)> = tests
        .iter()
        .map(|t| (t.suite().as_raw().to_owned(), t.name().as_raw().to_owned()))
        .collect();
    let expected: BTreeSet<(String, String)> = DEFAULT
        .iter()
        .map(|&(s, n)| (s.to_owned(), n.to_owned()))
        .collect();
    assert_eq!(listed, expected);
    assert!(
        !text(&out.stdout).contains("Ran "),
        "the listing runs nothing"
    );
    // lein's own count of the default selection: every listed test runs,
    // but for the one whose namespace's fixture throws.
    let plain = project.lein(&["test"], &[]);
    let ran = format!("Ran {} tests", DEFAULT.len() - 1);
    assert!(
        text(&plain.stdout).contains(&ran),
        "{}",
        text(&plain.stdout)
    );
    // Selectors given after -- choose what is listed, as lein chooses.
    let out = project.run(&["--", ":slow"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let slow: BTreeSet<(String, String)> = project.runs()[0]
        .results
        .iter()
        .map(|(s, n, _)| (s.clone(), n.clone()))
        .collect();
    assert_eq!(
        slow,
        [
            ("scratch.plain-test", "takes-long"),
            ("scratch.slow-ns-test", "whole-ns-is-slow")
        ]
        .iter()
        .map(|&(s, n)| (s.to_owned(), n.to_owned()))
        .collect()
    );
}

#[test]
fn i3_the_greenshift_selector_runs_exactly_the_named_tests_and_loads_only_their_namespaces() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    let dir = project.dir.path();
    let names = dir.join("group.names");
    std::fs::write(
        &names,
        "scratch.plain-test/flips\nscratch.plain-test/kept-private\n",
    )
    .unwrap();
    let junit = dir.join("group.xml");
    let script = dir.join("group.clj");
    std::fs::write(
        &script,
        greenshift::run::lein::run_script(&names, &junit).unwrap(),
    )
    .unwrap();
    let loaded = dir.join("loaded.txt");
    let injection = greenshift::run::lein::injection(&script).unwrap();
    let out = project.lein(
        &[
            "update-in",
            ":injections",
            "conj",
            injection.to_str().unwrap(),
            "--",
            "test",
            ":greenshift",
        ],
        &[("LOADED", loaded.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("Ran 2 tests"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&loaded).unwrap(),
        "scratch.plain-test\n"
    );
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert_eq!(xml.matches("<testcase ").count(), 2, "{xml}");
    assert!(xml.contains("name=\"flips\"") && xml.contains("name=\"kept-private\""));
    // ingest takes the reporter's file as it is (I5).
    let db = dir.join("ingested.sqlite");
    let out = project.greenshift(
        &[
            "ingest",
            junit.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn i4_plain_lein_test_is_unchanged_by_the_entry() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    // Stack traces name compiled classes by counters; the rest is compared.
    let normal = |out: &Output| -> (Option<i32>, String) {
        // Every run of digits as one #.
        let digits = |line: &str| -> String {
            let mut out = String::new();
            for c in line.chars() {
                if !c.is_ascii_digit() {
                    out.push(c);
                } else if !out.ends_with('#') {
                    out.push('#');
                }
            }
            out
        };
        (
            out.status.code(),
            text(&out.stdout)
                .lines()
                .map(digits)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let before = project.lein(&["test"], &[]);
    project.init();
    let after = project.lein(&["test"], &[]);
    assert_eq!(before.status.code(), Some(1));
    assert_eq!(normal(&before), normal(&after));
}

#[test]
fn i5_i6_a_run_records_every_outcome_and_the_second_plans_rich_and_poor() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    let out = project.run(&["--min-runs", "1"], &["flips"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("lein listed 9 tests in"), "{stderr}");
    let runs = project.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0],
        Run {
            source: "lein".into(),
            partial: false,
            files: 1,
            results: expected_results(&["flips"]),
        }
    );
    // A second cycle: the failures run rich, the passes poor, as two JVMs
    // recorded as one run. Nothing fails this time but what always does.
    let out = project.run(&["--min-runs", "1"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let runs = project.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].files, 2, "rich and poor each wrote JUnit");
    assert!(!runs[1].partial);
    assert_eq!(runs[1].results, expected_results(&[]));
}

#[test]
fn a_private_test_planned_apart_from_its_namespaces_public_ones_still_runs() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    // kept-private fails, so the second cycle plans it rich while the
    // public tests of its namespace, which pass, go poor. lein selects a
    // namespace only for a public test, so on its own the rich JVM would
    // skip it: a failing test, never run.
    let out = project.run(&["--min-runs", "1"], &["kept-private"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let out = project.run(&["--min-runs", "1"], &["kept-private"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let runs = project.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].files, 2, "rich and poor each wrote JUnit");
    assert_eq!(runs[1].results, expected_results(&["kept-private"]));
}

#[test]
fn i7_neither_injection_nor_the_selector_warns_of_reflection() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    let out = project.run(&[], &[]);
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(all.contains("lein listed"), "{all}");
    assert!(!all.contains("Reflection warning"), "{all}");
    // Without :preserve-eval-meta, lein drops the selector's type hints
    // when it writes the test form out (as vaelii's project.clj says), so
    // the selector warns; greenshift's own injections, loaded from files,
    // still never do.
    let clj = project.root().join("project.clj");
    let text_now = std::fs::read_to_string(&clj).unwrap();
    std::fs::write(&clj, text_now.replace("  :preserve-eval-meta true\n", "")).unwrap();
    let out = project.run(&[], &[]);
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    let warnings: Vec<&str> = all
        .lines()
        .filter(|line| line.starts_with("Reflection warning"))
        .collect();
    assert!(!warnings.is_empty(), "{all}");
    assert!(
        warnings.iter().all(|line| line.contains("/form-init")),
        "{warnings:?}"
    );
}

#[test]
fn without_the_selector_groups_run_with_only_and_record_the_same() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    let out = project.run(&["--max-only-bytes", "60"], &[]);
    let stderr = text(&out.stderr);
    assert!(stderr.contains("no :greenshift test selector"), "{stderr}");
    let runs = project.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].files > 1, "split into batches: {}", runs[0].files);
    assert!(!runs[0].partial);
    assert_eq!(runs[0].results, expected_results(&[]));
    assert_eq!(
        std::fs::read_to_string(project.root().join("project.clj")).unwrap(),
        PROJECT
    );
}

#[test]
fn a_listing_that_fails_ends_the_run_with_its_status_and_records_nothing() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    std::fs::write(
        project.root().join("test/scratch/broken_test.clj"),
        "(ns scratch.broken-test)\n(this-does-not-resolve)\n",
    )
    .unwrap();
    let out = project.run(&[], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(!text(&out.stderr).contains("lein listed"));
    assert!(!project.db.exists() || project.runs().is_empty());
}

#[test]
fn a_jvm_that_exits_inside_a_test_still_writes_every_tests_result() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    std::fs::write(
        project.root().join("test/scratch/exits_test.clj"),
        "(ns scratch.exits-test (:require [clojure.test :refer :all]))\n\
         (deftest exits (System/exit 3))\n",
    )
    .unwrap();
    let out = project.run(&[], &[]);
    // lein itself exits 1 when the project JVM fails, whatever its status.
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let runs = project.runs();
    assert_eq!(runs.len(), 1);
    let results = &runs[0].results;
    assert!(
        results.contains(&("scratch.exits-test".into(), "exits".into(), "error".into())),
        "{results:?}"
    );
    // Every listed test has a result: what ran before the exit, and what
    // never ran, skipped.
    let tests = results
        .iter()
        .filter(|(_, name, _)| name != "fixtures")
        .count();
    assert_eq!(tests, DEFAULT.len() + 1, "{results:?}");
}

#[test]
fn a_report_method_a_test_namespace_defines_still_has_its_failures_recorded() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    // Loaded after greenshift's reporter, this namespace's own :fail
    // method replaces greenshift's, chaining to nothing.
    std::fs::write(
        project.root().join("test/scratch/own_report_test.clj"),
        "(ns scratch.own-report-test (:require [clojure.test :as t :refer :all]))\n\
         (defmethod t/report :fail [m]\n  \
           (t/with-test-out (println \"own :fail\" (:message m)))\n  \
           (t/inc-report-counter :fail))\n\
         (deftest fails-its-own-way (is (= 1 2) \"mine\"))\n",
    )
    .unwrap();
    let out = project.run(&[], &[]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(stdout.contains("own :fail mine"), "{stdout}");
    let runs = project.runs();
    assert_eq!(runs.len(), 1);
    let results = &runs[0].results;
    let outcome = |suite: &str, name: &str| {
        results
            .iter()
            .find(|(s, n, _)| s == suite && n == name)
            .map(|(_, _, outcome)| outcome.clone())
    };
    assert_eq!(
        outcome("scratch.own-report-test", "fails-its-own-way").as_deref(),
        Some("failed"),
        "{results:?}"
    );
    // The other namespaces' failures go through that method too.
    assert_eq!(
        outcome("scratch.errors-test", "fails").as_deref(),
        Some("failed"),
        "{results:?}"
    );
}

#[test]
fn each_report_is_recorded_once_however_a_chained_method_passes_it_on() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    // A :fail method that chains to greenshift's with a map of its own,
    // and a test that reports one constant map twice: two failures and
    // two errors, as clojure.test counts them.
    std::fs::write(
        project.root().join("test/scratch/chain_test.clj"),
        "(ns scratch.chain-test (:require [clojure.test :as t :refer :all]))\n\
         (let [old (get-method t/report :fail)]\n  \
           (defmethod t/report :fail [m] (old (assoc m :seen-by ::chain))))\n\
         (defn- boom [] (t/report {:type :error :message \"same map\"}))\n\
         (deftest altered (is (= 1 2) \"one\") (is (= 3 4) \"two\"))\n\
         (deftest same-map-twice (boom) (boom))\n",
    )
    .unwrap();
    let dir = project.dir.path();
    let names = dir.join("group.names");
    std::fs::write(
        &names,
        "scratch.chain-test/altered\nscratch.chain-test/same-map-twice\n",
    )
    .unwrap();
    let junit = dir.join("group.xml");
    let script = dir.join("group.clj");
    std::fs::write(
        &script,
        greenshift::run::lein::run_script(&names, &junit).unwrap(),
    )
    .unwrap();
    let injection = greenshift::run::lein::injection(&script).unwrap();
    let out = project.lein(
        &[
            "update-in",
            ":injections",
            "conj",
            injection.to_str().unwrap(),
            "--",
            "test",
            ":greenshift",
        ],
        &[],
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("2 failures, 2 errors."), "{stdout}");
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert_eq!(xml.matches("<failure ").count(), 2, "{xml}");
    assert_eq!(xml.matches("<error ").count(), 2, "{xml}");
    assert_eq!(xml.matches("assertions=\"2\"").count(), 2, "{xml}");
}

#[test]
fn a_test_that_calls_another_keeps_its_own_assertions_after_the_call() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    // clojure.test composes tests by calling them: inner reports its own
    // begin and end inside outer's, and outer's failure comes after.
    std::fs::write(
        project.root().join("test/scratch/nested_test.clj"),
        "(ns scratch.nested-test (:require [clojure.test :refer :all]))\n\
         (deftest inner (is true))\n\
         (deftest outer (inner) (is (= 1 2) \"after inner\"))\n",
    )
    .unwrap();
    let dir = project.dir.path();
    let names = dir.join("group.names");
    // Both named, as a whole run names them: a test lein sets aside runs
    // nothing when called.
    std::fs::write(
        &names,
        "scratch.nested-test/inner\nscratch.nested-test/outer\n",
    )
    .unwrap();
    let junit = dir.join("group.xml");
    let script = dir.join("group.clj");
    std::fs::write(
        &script,
        greenshift::run::lein::run_script(&names, &junit).unwrap(),
    )
    .unwrap();
    let injection = greenshift::run::lein::injection(&script).unwrap();
    let out = project.lein(
        &[
            "update-in",
            ":injections",
            "conj",
            injection.to_str().unwrap(),
            "--",
            "test",
            ":greenshift",
        ],
        &[],
    );
    assert!(
        text(&out.stdout).contains("FAIL in (outer)"),
        "{}",
        text(&out.stdout)
    );
    let xml = std::fs::read_to_string(&junit).unwrap();
    let outer = xml
        .split("<testcase ")
        .find(|case| case.contains("name=\"outer\""))
        .unwrap_or_else(|| panic!("{xml}"));
    assert!(outer.contains("<failure message=\"after inner\""), "{xml}");
    assert!(!xml.contains("name=\"fixtures\""), "{xml}");
}

#[test]
fn a_report_var_a_project_injection_made_a_function_still_has_every_result_recorded() {
    let _turn = turn();
    if !lein_ready() {
        return;
    }
    let project = Project::new();
    project.init();
    // Some tools make clojure.test/report a function of their own from an
    // injection, before greenshift's, hiding the multimethod (test.check
    // allows for them, TCHECK-125).
    let clj = project.root().join("project.clj");
    let text_now = std::fs::read_to_string(&clj).unwrap();
    std::fs::write(
        &clj,
        text_now.replace(
            "  :preserve-eval-meta true\n",
            "  :preserve-eval-meta true\n  \
             :injections [(alter-var-root #'clojure.test/report (fn [r] (fn [m] (r m))))]\n",
        ),
    )
    .unwrap();
    let out = project.run(&[], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let runs = project.runs();
    assert_eq!(runs.len(), 1, "{}", text(&out.stderr));
    assert!(!runs[0].partial);
    assert_eq!(runs[0].results, expected_results(&[]));
}
