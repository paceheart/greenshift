//! `greenshift init lein` on scratch projects: the `:greenshift` selector
//! merged into project.clj, the workflow only with `--ci`, that a second
//! run changes nothing, `--force`, and what it refuses.
//!
//! The generated workflow is compared with `tests/golden/init-lein-github.yml`;
//! after a deliberate change to the template, rewrite it with
//! `GREENSHIFT_BLESS=1 cargo test --test init_lein` and review the diff.
//!
//! With `GREENSHIFT_VAELII_PROJECT_CLJ` naming a vaelii checkout's
//! project.clj, one test also merges into the real file.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

const RELEASE: &str = "v0.3.0";
const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const WORKFLOW: &str = ".github/workflows/greenshift.yml";

const VAELII: &str = include_str!("../fixtures/lein/vaelii-project.clj");

/// The entry the #35 scope card shows, keys at column 19.
const CARD_ENTRY: &str = r#"                   ;; Added by `greenshift init lein`: exactly the tests named, one ns/var
                   ;; per line, in the file -Dgreenshift.names gives (`greenshift run lein`
                   ;; writes it). Plain `lein test` never reads it.
                   :greenshift (let [names (delay (->> (.split ^String (slurp (or (System/getProperty "greenshift.names")
                                                                                  (throw (ex-info "lein test :greenshift needs -Dgreenshift.names=FILE" {}))))
                                                               "\n")
                                                       (remove empty?) set))
                                     nss   (delay (set (keep #(let [i (.indexOf ^String % "/")] (when (pos? i) (subs ^String % 0 i))) @names)))]
                                 [(fn [ns & _] (contains? @nss (str ns)))
                                  (fn [m & _] (contains? @names (str (ns-name (:ns m)) "/" (:name m))))])"#;

const VAELII_LAST: &str =
    "                   :all       (fn [m & _] (not (or (:multi-jvm m) (:fuzz m) (:full-kb m) (:red m))))}";

fn init(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("greenshift").expect("binary builds");
    cmd.current_dir(dir).args(["init", "lein"]).args(args);
    cmd
}

fn ci_args() -> [&'static str; 5] {
    ["--ci", "--release", RELEASE, "--sha256", SHA256]
}

fn project(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("project.clj"), text).unwrap();
    dir
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

/// Every file under `dir`, with its content, sorted by path.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn merged_vaelii(text: &str) -> String {
    let last = VAELII_LAST.trim_end_matches('}');
    text.replacen(VAELII_LAST, &format!("{last}\n{CARD_ENTRY}}}"), 1)
}

#[test]
fn init_adds_exactly_the_cards_selector_and_nothing_else() {
    let dir = project(VAELII);
    init(dir.path(), &[])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added the :greenshift test selector to project.clj",
        ));
    assert_eq!(read(dir.path(), "project.clj"), merged_vaelii(VAELII));
    assert_eq!(snapshot(dir.path()).len(), 1, "only project.clj is there");
}

#[test]
fn the_real_vaelii_project_clj_gets_exactly_the_cards_selector() {
    let Some(path) = std::env::var_os("GREENSHIFT_VAELII_PROJECT_CLJ") else {
        eprintln!("skipped: GREENSHIFT_VAELII_PROJECT_CLJ is not set");
        return;
    };
    let original = std::fs::read_to_string(path).unwrap();
    let dir = project(&original);
    init(dir.path(), &[]).assert().success();
    assert_eq!(read(dir.path(), "project.clj"), merged_vaelii(&original));
}

#[test]
fn a_second_run_changes_nothing() {
    let dir = project(VAELII);
    init(dir.path(), &ci_args()).assert().success();
    let before = snapshot(dir.path());
    init(dir.path(), &ci_args())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "project.clj is already up to date",
        ))
        .stdout(predicate::str::contains("nothing to change"));
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn a_different_selector_is_left_alone_unless_forced() {
    let dir = project(VAELII);
    init(dir.path(), &[]).assert().success();
    let merged = read(dir.path(), "project.clj");
    let edited = merged.replace("(remove empty?) set))", "(remove empty?) (into #{})))");
    std::fs::write(dir.path().join("project.clj"), &edited).unwrap();
    init(dir.path(), &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "project.clj has a :greenshift test selector that differs",
        ))
        .stderr(predicate::str::contains("nothing written"));
    assert_eq!(read(dir.path(), "project.clj"), edited);
    init(dir.path(), &["--force"]).assert().success();
    assert_eq!(read(dir.path(), "project.clj"), merged);
}

#[test]
fn with_no_map_init_adds_one_and_plain_selection_is_unchanged() {
    let text = "(defproject demo \"0.0.0\"\n  :dependencies [[org.clojure/clojure \"1.12.6\"]])\n";
    let dir = project(text);
    init(dir.path(), &[]).assert().success();
    let merged = read(dir.path(), "project.clj");
    assert!(
        merged.contains("\n  :test-selectors {:greenshift (let [names"),
        "{merged}"
    );
    assert!(!merged.contains(":default"));
    init(dir.path(), &[])
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));
}

#[test]
fn a_computed_map_is_refused_and_left_alone() {
    let text = "(defproject demo \"0.0.0\"\n  :test-selectors ~(merge {} {:fast :fast}))\n";
    let dir = project(text);
    init(dir.path(), &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("computed"));
    assert_eq!(read(dir.path(), "project.clj"), text);
}

#[test]
fn no_project_clj_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path(), &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no project.clj"));
    assert!(snapshot(dir.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlinked_project_clj_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.clj");
    std::fs::write(&real, VAELII).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("project.clj")).unwrap();
    init(dir.path(), &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("symlink"));
    assert_eq!(std::fs::read_to_string(&real).unwrap(), VAELII);
    assert!(std::fs::symlink_metadata(dir.path().join("project.clj"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn the_workflow_is_written_only_with_ci_and_matches_the_golden() {
    let dir = project(VAELII);
    init(dir.path(), &ci_args())
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("wrote {WORKFLOW}")));
    let written = read(dir.path(), WORKFLOW);
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/init-lein-github.yml");
    if std::env::var_os("GREENSHIFT_BLESS").is_some() {
        std::fs::write(&golden, &written).unwrap();
    }
    let expected = std::fs::read_to_string(&golden).unwrap();
    assert!(
        written == expected,
        "differs from {}; rerun with GREENSHIFT_BLESS=1 if meant\n{written}",
        golden.display()
    );
    assert!(written.contains("run lein --history-remote"));
    assert!(written.contains(&format!("GREENSHIFT_RELEASE: '{RELEASE}'")));
    assert!(written.contains("java-version: '25'"));
    assert!(written.contains("lein: 2.12.0"));
    for name in ["RELEASE", "SHA256", "RUNS_ON", "INSTALL", "JAVA", "LEIN"] {
        assert!(!written.contains(&format!("@{name}@")), "{name}");
    }
}

#[test]
fn the_java_version_and_runner_reach_the_workflow() {
    let dir = project(VAELII);
    let mut args = ci_args().to_vec();
    args.extend(["--java-version", "21", "--runs-on", "self-hosted"]);
    init(dir.path(), &args).assert().success();
    let written = read(dir.path(), WORKFLOW);
    assert!(written.contains("java-version: '21'"));
    assert!(written.contains("jdk21-"));
    assert_eq!(written.matches("runs-on: 'self-hosted'").count(), 2);
    for bad in ["", "2 1", "'21'", "x\ny"] {
        let mut args = ci_args().to_vec();
        args.extend(["--java-version", bad]);
        init(dir.path(), &args).assert().code(2);
    }
}

#[test]
fn ci_options_need_ci_and_ci_needs_its_pin() {
    let dir = project(VAELII);
    for args in [
        &["--release", RELEASE][..],
        &["--sha256", SHA256],
        &["--runs-on", "x"],
        &["--java-version", "21"],
        &["--ci"],
        &["--ci", "--release", RELEASE],
        &["--ci", "--sha256", SHA256],
        &["--ci", "forgejo", "--release", RELEASE, "--sha256", SHA256],
    ] {
        init(dir.path(), args).assert().code(2);
    }
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    init(
        dir.path(),
        &["--ci", "github", "--release", RELEASE, "--sha256", SHA256],
    )
    .assert()
    .success();
}

#[test]
fn a_conflicting_workflow_means_project_clj_is_not_written_either() {
    let dir = project(VAELII);
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::write(dir.path().join(WORKFLOW), "mine\n").unwrap();
    init(dir.path(), &ci_args())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing written"));
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(read(dir.path(), WORKFLOW), "mine\n");
}

#[test]
fn the_workflow_is_yaml_with_the_two_job_shape() {
    let dir = project(VAELII);
    init(dir.path(), &ci_args()).assert().success();
    let out = std::process::Command::new("yq")
        .args(["-o=json", "."])
        .arg(dir.path().join(WORKFLOW))
        .output();
    let Ok(out) = out else {
        eprintln!("skipped: no yq");
        return;
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let workflow: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(workflow["jobs"]["test"]["permissions"]["contents"], "read");
    assert_eq!(
        workflow["jobs"]["publish"]["permissions"]["contents"],
        "write"
    );
    assert_eq!(workflow["jobs"]["publish"]["needs"], "test");
    assert_eq!(
        workflow["on"],
        serde_json::json!(["push", "workflow_dispatch"])
    );
    for step in workflow["jobs"]["publish"]["steps"].as_array().unwrap() {
        assert!(!step["uses"].as_str().unwrap_or("").contains("checkout"));
        assert!(!step["run"].as_str().unwrap_or("").contains("lein"));
    }
}
