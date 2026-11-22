//! `greenshift init lein` on scratch projects: the `:greenshift` selector
//! merged into project.clj, the workflow only with `--ci`, that a second
//! run changes nothing, `--force`, and what it refuses.
//!
//! The generated workflows are compared with `tests/golden/init-lein-github.yml`
//! and `tests/golden/init-lein-forgejo.yml`; after a deliberate change to a
//! template, rewrite them with
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
const FORGEJO_WORKFLOW: &str = ".forgejo/workflows/greenshift.yml";

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

fn forgejo_args() -> [&'static str; 6] {
    ["--ci", "forgejo", "--release", RELEASE, "--sha256", SHA256]
}

/// Assert `written` is the golden file `name`, rewriting it first under
/// `GREENSHIFT_BLESS`.
fn assert_golden(written: &str, name: &str) {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("GREENSHIFT_BLESS").is_some() {
        std::fs::write(&golden, written).unwrap();
    }
    let expected = std::fs::read_to_string(&golden).unwrap();
    assert!(
        written == expected,
        "differs from {}; rerun with GREENSHIFT_BLESS=1 if meant\n{written}",
        golden.display()
    );
}

/// The YAML file at `path` as JSON, or `None` without yq.
fn yaml(path: &Path) -> Option<serde_json::Value> {
    let Ok(out) = std::process::Command::new("yq")
        .args(["-o=json", "."])
        .arg(path)
        .output()
    else {
        eprintln!("skipped: no yq");
        return None;
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(serde_json::from_slice(&out.stdout).unwrap())
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
        .stderr(predicate::str::contains("computed"))
        // run lein cannot see an entry added by hand to a computed map.
        .stderr(predicate::str::contains("by hand").not());
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
    assert_golden(&written, "init-lein-github.yml");
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
        &["--ci", "forgejo"],
        &["--ci", "forgejo", "--release", RELEASE],
        &["--ci", "forgejo", "--sha256", SHA256],
        &["--ci", "gitlab", "--release", RELEASE, "--sha256", SHA256],
        // Releases up to v0.2.0 have no `run lein`.
        &["--ci", "--release", "v0.2.0", "--sha256", SHA256],
        &["--ci", "--release", "v0.1.1", "--sha256", SHA256],
        &["--ci", "forgejo", "--release", "v0.2.0", "--sha256", SHA256],
    ] {
        init(dir.path(), args).assert().code(2);
    }
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert!(!dir.path().join(".github").exists());
    assert!(!dir.path().join(".forgejo").exists());
    init(
        dir.path(),
        &["--ci", "github", "--release", RELEASE, "--sha256", SHA256],
    )
    .assert()
    .success();
    init(dir.path(), &forgejo_args()).assert().success();
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
    let Some(workflow) = yaml(&dir.path().join(WORKFLOW)) else {
        return;
    };
    assert_two_jobs(&workflow);
    assert_eq!(workflow["jobs"]["test"]["permissions"]["contents"], "read");
    assert_eq!(
        workflow["jobs"]["publish"]["permissions"]["contents"],
        "write"
    );
}

/// What every lein workflow has, whatever the forge: on pushes and by
/// hand only, a publisher that needs the test job and runs no lein, and
/// every action pinned to a commit.
fn assert_two_jobs(workflow: &serde_json::Value) {
    assert_eq!(workflow["jobs"]["publish"]["needs"], "test");
    assert_eq!(
        workflow["on"],
        serde_json::json!(["push", "workflow_dispatch"])
    );
    for step in workflow["jobs"]["publish"]["steps"].as_array().unwrap() {
        assert!(!step["uses"].as_str().unwrap_or("").contains("checkout"));
        assert!(!step["run"].as_str().unwrap_or("").contains("lein"));
    }
    for job in ["test", "publish"] {
        for step in workflow["jobs"][job]["steps"].as_array().unwrap() {
            if let Some(uses) = step["uses"].as_str() {
                let (_, rev) = uses.rsplit_once('@').unwrap();
                assert!(
                    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{uses} is not pinned to a commit"
                );
            }
        }
    }
}

#[test]
fn forgejo_writes_the_golden_workflow_and_says_it_is_not_isolated() {
    let dir = project(VAELII);
    let out = init(dir.path(), &forgejo_args()).assert().success();
    let out = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(out.contains(&format!("wrote {FORGEJO_WORKFLOW}")), "{out}");
    assert!(
        out.contains("added the :greenshift test selector to project.clj"),
        "{out}"
    );
    assert_eq!(
        out.lines()
            .filter(|line| line.contains("not isolated"))
            .count(),
        1,
        "{out}"
    );
    assert!(!dir.path().join(".github").exists());
    let written = read(dir.path(), FORGEJO_WORKFLOW);
    assert_golden(&written, "init-lein-forgejo.yml");
    assert!(written.starts_with("# Generated by `greenshift init lein --ci forgejo`"));
    assert!(
        written.contains("THIS WORKFLOW IS NOT ISOLATED"),
        "{written}"
    );
    assert!(
        !written.contains("permissions:"),
        "permissions would be ignored"
    );
    assert!(written.contains("run lein --history-remote"));
    assert!(written.contains("java-version: '25'"));
    assert!(written.contains("lein: 2.12.0"));
    assert_eq!(written.matches("runs-on: 'docker'").count(), 2);
    for name in [
        "RELEASE",
        "SHA256",
        "RUNS_ON",
        "INSTALL",
        "JAVA",
        "LEIN",
        "NOT_ISOLATED",
        "FORGEJO_ACTIONS",
    ] {
        assert!(!written.contains(&format!("@{name}@")), "{name}");
    }
}

#[test]
fn forgejo_names_every_action_by_url_and_claims_no_permissions() {
    let dir = project(VAELII);
    init(dir.path(), &forgejo_args()).assert().success();
    let Some(workflow) = yaml(&dir.path().join(FORGEJO_WORKFLOW)) else {
        return;
    };
    assert_two_jobs(&workflow);
    assert!(workflow["permissions"].is_null());
    let mut seen = Vec::new();
    for job in ["test", "publish"] {
        assert!(workflow["jobs"][job]["permissions"].is_null(), "{job}");
        assert_eq!(workflow["jobs"][job]["runs-on"], "docker", "{job}");
        for step in workflow["jobs"][job]["steps"].as_array().unwrap() {
            if let Some(uses) = step["uses"].as_str() {
                // Not resolved against the instance's DEFAULT_ACTIONS_URL.
                assert!(uses.starts_with("https://"), "{uses}");
                if uses.contains("artifact@") {
                    // Stock v4 artifact actions refuse to run on Forgejo.
                    assert!(uses.contains("/forgejo/"), "{uses}");
                }
                seen.push(uses.rsplit_once('@').unwrap().0.to_owned());
            }
        }
    }
    for action in ["setup-java", "setup-clojure", "actions/cache"] {
        assert!(seen.iter().any(|uses| uses.contains(action)), "{seen:?}");
    }
}

#[test]
fn forgejo_takes_the_runner_and_java_version_and_is_idempotent() {
    let dir = project(VAELII);
    let mut args = forgejo_args().to_vec();
    args.extend(["--java-version", "21", "--runs-on", "self-hosted"]);
    init(dir.path(), &args).assert().success();
    let written = read(dir.path(), FORGEJO_WORKFLOW);
    assert!(written.contains("java-version: '21'"));
    assert!(written.contains("jdk21-"));
    assert_eq!(written.matches("runs-on: 'self-hosted'").count(), 2);
    let before = snapshot(dir.path());
    init(dir.path(), &args)
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to change"))
        .stdout(predicate::str::contains("not isolated"));
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn forgejo_says_when_its_workflow_would_stop_the_github_ones_running() {
    let dir = project(VAELII);
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::write(dir.path().join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    init(dir.path(), &forgejo_args())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "stops running the workflows in .github/workflows",
        ));
    init(dir.path(), &forgejo_args())
        .assert()
        .success()
        .stdout(predicate::str::contains("stops running").not());
}

#[test]
fn a_conflicting_forgejo_workflow_means_project_clj_is_not_written_either() {
    let dir = project(VAELII);
    std::fs::create_dir_all(dir.path().join(".forgejo/workflows")).unwrap();
    std::fs::write(dir.path().join(FORGEJO_WORKFLOW), "mine\n").unwrap();
    init(dir.path(), &forgejo_args())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("nothing written"));
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(read(dir.path(), FORGEJO_WORKFLOW), "mine\n");
    init(dir.path(), &[&forgejo_args()[..], &["--force"]].concat())
        .assert()
        .success();
    assert!(read(dir.path(), FORGEJO_WORKFLOW).contains("THIS WORKFLOW IS NOT ISOLATED"));
}
