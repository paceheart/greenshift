//! `greenshift init lein` on scratch projects: the `:greenshift` selector
//! merged into project.clj, the workflow only with `--ci`, that a second
//! run changes nothing, `--force`, and what it refuses.
//!
//! The generated workflows are compared with `tests/golden/init-lein-github.yml`
//! and `tests/golden/init-lein-forgejo.yml`, and with `--setup` and
//! `--project-dir` as an overlay repository such as vaelii-lacuna gives
//! them, `tests/golden/init-lein-forgejo-overlay.yml`, and with
//! `--test-args-file`, `tests/golden/init-lein-github-test-args.yml` and
//! `tests/golden/init-lein-forgejo-overlay-test-args.yml`; after a deliberate change to a
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

/// The YAML file at `path` as JSON, by mikefarah's yq v4, which the
/// other workflow tests need too: without it the test fails rather than
/// passing having checked nothing.
fn yaml(path: &Path) -> serde_json::Value {
    let out = std::process::Command::new("yq")
        .args(["-o=json", "."])
        .arg(path)
        .output()
        .expect("mikefarah yq installed as for cache_guard tests");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
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
    let workflow = yaml(&dir.path().join(WORKFLOW));
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
    assert!(written.contains("lein_version=2.12.0"));
    assert_eq!(written.matches("runs-on: 'docker'").count(), 2);
    for name in [
        "RELEASE",
        "SHA256",
        "RUNS_ON",
        "INSTALL",
        "JAVA",
        "LEIN",
        "LEIN_SHA256",
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
    let workflow = yaml(&dir.path().join(FORGEJO_WORKFLOW));
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
    for action in ["setup-java", "actions/cache"] {
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

#[test]
fn the_forgejo_workflow_is_the_github_one_but_for_what_forgejo_needs() {
    assert_forges_agree(&[], || project(VAELII));
}

/// The workflows `init lein --ci github` and `--ci forgejo` write with
/// `extra` arguments, into a fresh `repo()` each, as JSON.
fn both_forges(extra: &[&str], repo: impl Fn() -> tempfile::TempDir) -> [serde_json::Value; 2] {
    let mut workflows = Vec::new();
    for args in [&ci_args()[..], &forgejo_args()[..]] {
        let dir = repo();
        init(dir.path(), &[args, extra].concat()).assert().success();
        let path = if args.contains(&"forgejo") {
            FORGEJO_WORKFLOW
        } else {
            WORKFLOW
        };
        workflows.push(yaml(&dir.path().join(path)));
    }
    workflows.try_into().unwrap()
}

/// The Forgejo workflow `init lein` writes with `extra` arguments is the
/// GitHub one, step for step, but for what Forgejo needs.
fn assert_forges_agree(extra: &[&str], repo: impl Fn() -> tempfile::TempDir) {
    let [github, forgejo] = both_forges(extra, repo);
    let (github, forgejo) = (&github, &forgejo);
    // Everything but the jobs is the same; `permissions` are GitHub's
    // alone, as Forgejo ignores them.
    assert_eq!(
        without(github, &["jobs", "permissions"]),
        without(forgejo, &["jobs", "permissions"])
    );
    assert_eq!(
        github["jobs"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        forgejo["jobs"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>()
    );
    for job in ["test", "publish"] {
        let (github, forgejo) = (&github["jobs"][job], &forgejo["jobs"][job]);
        // `if`, `needs`, `concurrency`, `env` and the rest; `runs-on` is
        // each forge's default runner.
        assert_eq!(
            without(github, &["steps", "permissions", "runs-on"]),
            without(forgejo, &["steps", "permissions", "runs-on"]),
            "{job}"
        );
        let github = github["steps"].as_array().unwrap();
        let forgejo = forgejo["steps"].as_array().unwrap();
        assert_eq!(github.len(), forgejo.len(), "{job}");
        for (github, forgejo) in github.iter().zip(forgejo) {
            let (mut github_step, forgejo_step) =
                (without(github, &["uses"]), without(forgejo, &["uses"]));
            if forgejo["name"] == "Set up Leiningen" {
                // The setup-clojure action on GitHub; a direct install,
                // pinned by checksum, on Forgejo.
                assert_eq!(github["name"], forgejo["name"]);
                assert!(github["uses"].as_str().unwrap().contains("setup-clojure"));
                assert!(forgejo["uses"].is_null());
                assert_eq!(forgejo["shell"], "bash");
                continue;
            }
            let (Some(github), Some(forgejo)) = (github["uses"].as_str(), forgejo["uses"].as_str())
            else {
                assert!(github["uses"].is_null() && forgejo["uses"].is_null());
                assert_eq!(github_step, forgejo_step, "{job}");
                continue;
            };
            if github.starts_with("actions/") && github.contains("-artifact@") {
                // Forgejo's own forks: stock v4 refuses to run there.
                assert!(
                    forgejo.starts_with("https://code.forgejo.org/forgejo/"),
                    "{forgejo}"
                );
                if github.contains("upload-artifact@") {
                    // The fork, v4.3.1, has no such input: it predates
                    // leaving hidden files out by default.
                    let with = github_step["with"].as_object_mut().unwrap();
                    assert_eq!(with.remove("include-hidden-files"), Some(true.into()));
                }
            } else if github.starts_with("actions/checkout@") || github.starts_with("actions/cache")
            {
                // Forgejo's mirror: the same repository at the same commit.
                assert_eq!(forgejo, format!("https://data.forgejo.org/{github}"));
            } else {
                assert_eq!(forgejo, format!("https://github.com/{github}"));
            }
            // Name, `id`, `if`, `with`, `env`, `run` and the rest.
            assert_eq!(github_step, forgejo_step, "{job}: {github}");
        }
    }
}

/// `value`, a JSON object, without `keys`.
fn without(value: &serde_json::Value, keys: &[&str]) -> serde_json::Value {
    let mut value = value.clone();
    let object = value.as_object_mut().unwrap();
    for key in keys {
        object.remove(*key);
    }
    value
}

/// A repository with no project.clj at its root, and `text` as the
/// project.clj of the project in `subdir`.
fn nested(subdir: &str, text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(subdir)).unwrap();
    std::fs::write(dir.path().join(subdir).join("project.clj"), text).unwrap();
    dir
}

/// The step of `job` named `name`.
fn step<'w>(workflow: &'w serde_json::Value, job: &str, name: &str) -> &'w serde_json::Value {
    workflow["jobs"][job]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["name"] == name)
        .unwrap_or_else(|| panic!("{job} has no step {name:?}"))
}

#[test]
fn project_dir_names_the_project_clj_init_edits() {
    let dir = nested("upstream/app", VAELII);
    init(dir.path(), &["--project-dir", "upstream/app"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "added the :greenshift test selector to upstream/app/project.clj",
        ));
    assert_eq!(
        read(dir.path(), "upstream/app/project.clj"),
        merged_vaelii(VAELII)
    );
    assert_eq!(
        snapshot(dir.path()).len(),
        1,
        "only the project.clj is there"
    );
    init(dir.path(), &["--project-dir", "./upstream/app/"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "upstream/app/project.clj is already up to date",
        ));
    // Without it, the root's project.clj is the one, and there is none.
    init(dir.path(), &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no project.clj"));
}

#[test]
fn project_dir_without_a_project_clj_is_an_error_naming_it() {
    let dir = project(VAELII);
    init(
        dir.path(),
        &[&ci_args()[..], &["--project-dir", "vaelii"]].concat(),
    )
    .assert()
    .code(1)
    .stderr(predicate::str::contains("no vaelii/project.clj"))
    .stderr(
        predicate::str::contains("nothing written").or(predicate::str::contains("Nothing written")),
    );
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert!(!dir.path().join(".github").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_project_dir_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::fs::write(dir.path().join("real/project.clj"), VAELII).unwrap();
    std::os::unix::fs::symlink("real", dir.path().join("vaelii")).unwrap();
    init(dir.path(), &["--project-dir", "vaelii"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("symlink"));
    assert_eq!(read(dir.path(), "real/project.clj"), VAELII);
}

#[test]
fn a_bad_project_dir_is_a_usage_error_and_nothing_is_written() {
    let dir = project(VAELII);
    for (bad, why) in [
        ("", "relative to the repository root"),
        ("/srv/vaelii", "not an absolute path"),
        ("../vaelii", "no `..` parts"),
        ("vaelii/../..", "no `..` parts"),
        ("vaelii/..", "no `..` parts"),
        ("my project", "letters, digits"),
    ] {
        for forge in [&ci_args()[..], &forgejo_args()[..], &[]] {
            init(dir.path(), &[forge, &["--project-dir", bad]].concat())
                .assert()
                .code(2)
                .stderr(predicate::str::contains(why));
        }
    }
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(snapshot(dir.path()).len(), 1);
}

#[test]
fn project_dir_dot_is_the_root_and_writes_the_golden_workflows() {
    for (args, path, golden) in [
        (&ci_args()[..], WORKFLOW, "init-lein-github.yml"),
        (
            &forgejo_args()[..],
            FORGEJO_WORKFLOW,
            "init-lein-forgejo.yml",
        ),
    ] {
        let dir = project(VAELII);
        init(dir.path(), &[args, &["--project-dir", "."]].concat())
            .assert()
            .success();
        assert_golden(&read(dir.path(), path), golden);
        assert_eq!(read(dir.path(), "project.clj"), merged_vaelii(VAELII));
    }
}

#[test]
fn project_dir_runs_lein_there_on_both_forges() {
    let workflows = both_forges(&["--project-dir", "vaelii"], || nested("vaelii", VAELII));
    for workflow in &workflows {
        assert_two_jobs(workflow);
        let run = step(workflow, "test", "Run tests and export data");
        assert_eq!(run["working-directory"], "vaelii");
        // greenshift's own files stay at the repository root, where the
        // cache steps and the publisher look for them.
        let command = run["run"].as_str().unwrap();
        assert!(
            command.contains(r#"--history-cache-dir "$GITHUB_WORKSPACE/.greenshift/cache""#),
            "{command}"
        );
        assert!(
            command
                .contains(r#"--history-artifact "$GITHUB_WORKSPACE/.greenshift/publish.sqlite""#),
            "{command}"
        );
        assert_eq!(
            step(workflow, "test", "Restore fast copy")["with"]["path"],
            ".greenshift/cache"
        );
        assert_eq!(
            step(workflow, "test", "Upload SQLite data only")["with"]["path"],
            ".greenshift/publish.sqlite"
        );
        let key = step(workflow, "test", "Cache Maven dependencies")["with"]["key"]
            .as_str()
            .unwrap();
        assert!(
            key.ends_with("${{ hashFiles('vaelii/project.clj') }}"),
            "{key}"
        );
        // Only the step that runs lein runs elsewhere.
        for job in ["test", "publish"] {
            for other in workflow["jobs"][job]["steps"].as_array().unwrap() {
                if other["name"] != "Run tests and export data" {
                    assert!(other["working-directory"].is_null(), "{other}");
                }
            }
        }
    }
    assert_forges_agree(&["--project-dir", "vaelii"], || nested("vaelii", VAELII));
}

/// The names of `job`'s steps, in order.
fn step_names(workflow: &serde_json::Value, job: &str) -> Vec<String> {
    workflow["jobs"][job]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["name"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[test]
fn setup_runs_in_the_root_after_the_toolchain_and_before_the_tests_on_both_forges() {
    let without = both_forges(&[], || project(VAELII));
    let workflows = both_forges(&["--setup", "make assemble"], || project(VAELII));
    for (workflow, without) in workflows.iter().zip(&without) {
        assert_two_jobs(workflow);
        let names = step_names(workflow, "test");
        let at = |name: &str| names.iter().position(|n| n == name).unwrap();
        let setup = at("Set up the project");
        assert!(at("Set up Leiningen") < setup, "{names:?}");
        assert!(setup < at("Cache Maven dependencies"), "{names:?}");
        assert!(setup < at("Run tests and export data"), "{names:?}");
        // One step more, and nothing else changed.
        let mut names_without = step_names(without, "test");
        names_without.insert(setup, "Set up the project".to_owned());
        assert_eq!(names, names_without);
        let step = step(workflow, "test", "Set up the project");
        assert_eq!(step["run"], "make assemble\n");
        assert_eq!(step["shell"], "bash");
        assert!(step["working-directory"].is_null());
        assert!(step["uses"].is_null() && step["if"].is_null());
        assert_eq!(
            step_names(workflow, "publish"),
            step_names(without, "publish")
        );
    }
    assert_forges_agree(&["--setup", "make assemble"], || project(VAELII));
    assert_forges_agree(
        &["--setup", "make assemble", "--project-dir", "vaelii"],
        || nested("vaelii", VAELII),
    );
}

#[test]
fn the_setup_command_reaches_the_workflow_as_given() {
    let command = "set -x\n# a comment: with 'quotes' and \"more\"\nif true; then\n\techo @RELEASE@ ${{ 1 }} |\n  cat\n\nfi";
    for (args, path) in [
        (&ci_args()[..], WORKFLOW),
        (&forgejo_args()[..], FORGEJO_WORKFLOW),
    ] {
        let dir = project(VAELII);
        init(
            dir.path(),
            &[args, &["--setup", &format!("\n  {command}  \n\n")]].concat(),
        )
        .assert()
        .success();
        let workflow = yaml(&dir.path().join(path));
        assert_eq!(
            step(&workflow, "test", "Set up the project")["run"],
            format!("{command}\n")
        );
        // The command's own `@RELEASE@` is its own, not a placeholder.
        assert_eq!(read(dir.path(), path).matches(RELEASE).count(), 1);
    }
}

#[test]
fn a_bad_setup_is_a_usage_error_and_nothing_is_written() {
    let dir = project(VAELII);
    for (bad, why) in [
        ("", "not an empty one"),
        (" \n\t", "not an empty one"),
        ("make\r\nall", "no carriage returns"),
    ] {
        for forge in [&ci_args()[..], &forgejo_args()[..]] {
            init(dir.path(), &[forge, &["--setup", bad]].concat())
                .assert()
                .code(2)
                .stderr(predicate::str::contains(why));
        }
    }
    // Only a workflow runs it.
    init(dir.path(), &["--setup", "make"]).assert().code(2);
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(snapshot(dir.path()).len(), 1);
}

/// vaelii-lacuna's setup, as its own Forgejo CI does it: check out
/// upstream vaelii at the commit overlay.sh pins, then apply the overlay.
const LACUNA_SETUP: &str = r#"pin="$(sed -n 's/^PINNED_BASE_SHA="\([0-9a-f]\{40\}\)"$/\1/p' overlay.sh)"
[ "${#pin}" -eq 40 ] || { echo "no PINNED_BASE_SHA in overlay.sh" >&2; exit 1; }
git init -q vaelii
git -C vaelii fetch -q --depth 1 https://github.com/vaelii/vaelii.git "$pin"
git -C vaelii checkout -q --detach FETCH_HEAD
VAELII_UPSTREAM="$PWD/vaelii" ./overlay.sh"#;

#[test]
fn an_overlay_repository_assembles_its_project_in_setup() {
    // No project at the root, nor in vaelii/ until the setup makes it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("overlay.sh"), "#!/bin/sh\n").unwrap();
    let args = [
        &forgejo_args()[..],
        &["--setup", LACUNA_SETUP, "--project-dir", "vaelii"],
    ]
    .concat();
    init(dir.path(), &args)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no vaelii/project.clj here, so the :greenshift test selector is not added",
        ));
    let written = read(dir.path(), FORGEJO_WORKFLOW);
    assert_golden(&written, "init-lein-forgejo-overlay.yml");
    assert!(!dir.path().join("vaelii").exists());
    let workflow = yaml(&dir.path().join(FORGEJO_WORKFLOW));
    assert_eq!(
        step(&workflow, "test", "Set up the project")["run"],
        format!("{LACUNA_SETUP}\n")
    );
    assert_eq!(
        step(&workflow, "test", "Run tests and export data")["working-directory"],
        "vaelii"
    );
    let before = snapshot(dir.path());
    init(dir.path(), &args)
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to change"));
    assert_eq!(snapshot(dir.path()), before);
    // Without a setup to make it, a missing project.clj is still an error.
    init(
        dir.path(),
        &[&forgejo_args()[..], &["--project-dir", "vaelii", "--force"]].concat(),
    )
    .assert()
    .code(1)
    .stderr(predicate::str::contains("no vaelii/project.clj"));
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn the_header_names_the_flags_to_give_init_again() {
    let setup_line = "# Give it the same --setup too, or the new workflow goes without it.\n";
    let dir_line = "# Give it the same --project-dir too, or the new workflow goes without it.\n";
    let both_lines = "# Give it the same --setup and --project-dir too, or the new workflow goes\n\
                      # without them.\n";
    for (extra, repo, wanted) in [
        (
            &["--setup", "make"][..],
            project as fn(&str) -> tempfile::TempDir,
            setup_line,
        ),
        (
            &["--project-dir", "vaelii"][..],
            |text| nested("vaelii", text),
            dir_line,
        ),
        (
            &["--setup", "make", "--project-dir", "vaelii"][..],
            |text| nested("vaelii", text),
            both_lines,
        ),
    ] {
        for (args, path) in [
            (&ci_args()[..], WORKFLOW),
            (&forgejo_args()[..], FORGEJO_WORKFLOW),
        ] {
            let dir = repo(VAELII);
            init(dir.path(), &[args, extra].concat()).assert().success();
            let written = read(dir.path(), path);
            let header: String = written
                .lines()
                .take_while(|line| line.starts_with('#'))
                .take(5)
                .map(|line| format!("{line}\n"))
                .collect();
            assert!(header.contains(wanted), "{extra:?}: {header}");
            for other in [setup_line, dir_line, both_lines] {
                if other != wanted {
                    assert!(!written.contains(other), "{extra:?}: {header}");
                }
            }
        }
    }
}

/// The arguments file of the flag the tests below give.
const TEST_ARGS: &str = "test-selection";

#[test]
fn test_args_file_needs_ci_and_init_does_not_need_the_file() {
    let dir = project(VAELII);
    // Only a workflow reads it.
    init(dir.path(), &["--test-args-file", TEST_ARGS])
        .assert()
        .code(2);
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(snapshot(dir.path()).len(), 1);
    // With --ci, the file need not be there when init runs: the setup
    // may write it, and only the workflow reads it.
    for (args, path) in [
        (&ci_args()[..], WORKFLOW),
        (&forgejo_args()[..], FORGEJO_WORKFLOW),
    ] {
        let dir = project(VAELII);
        let args = [args, &["--test-args-file", TEST_ARGS]].concat();
        init(dir.path(), &args).assert().success();
        assert!(dir.path().join(path).exists());
        assert!(!dir.path().join(TEST_ARGS).exists());
        let before = snapshot(dir.path());
        init(dir.path(), &args)
            .assert()
            .success()
            .stdout(predicate::str::contains("nothing to change"));
        assert_eq!(snapshot(dir.path()), before);
    }
}

#[test]
fn a_bad_test_args_file_is_a_usage_error_and_nothing_is_written() {
    let dir = project(VAELII);
    for (bad, why) in [
        ("", "relative to the project directory"),
        ("/srv/args", "not an absolute path"),
        ("../args", "no `..` parts"),
        ("ci/../../args", "no `..` parts"),
        (".", "a file, not a directory"),
        ("./", "a file, not a directory"),
        ("ci/", "a file, not a directory"),
        ("my args", "letters, digits"),
        ("args'", "letters, digits"),
        ("a\nb", "letters, digits"),
    ] {
        for forge in [&ci_args()[..], &forgejo_args()[..]] {
            init(dir.path(), &[forge, &["--test-args-file", bad]].concat())
                .assert()
                .code(2)
                .stderr(predicate::str::contains(why));
        }
    }
    assert_eq!(read(dir.path(), "project.clj"), VAELII);
    assert_eq!(snapshot(dir.path()).len(), 1);
}

#[test]
fn test_args_file_writes_the_golden_workflows() {
    let dir = project(VAELII);
    init(
        dir.path(),
        &[&ci_args()[..], &["--test-args-file", TEST_ARGS]].concat(),
    )
    .assert()
    .success();
    assert_golden(
        &read(dir.path(), WORKFLOW),
        "init-lein-github-test-args.yml",
    );
    // vaelii-lacuna's shape: the setup assembles the project, and the
    // arguments file is in it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("overlay.sh"), "#!/bin/sh\n").unwrap();
    let args = [
        &forgejo_args()[..],
        &[
            "--setup",
            LACUNA_SETUP,
            "--project-dir",
            "vaelii",
            "--test-args-file",
            "ci/test-selection",
        ],
    ]
    .concat();
    init(dir.path(), &args).assert().success();
    assert_golden(
        &read(dir.path(), FORGEJO_WORKFLOW),
        "init-lein-forgejo-overlay-test-args.yml",
    );
}

#[test]
fn test_args_file_changes_only_the_test_step_on_both_forges() {
    for (extra, repo) in [
        (&[][..], (|| project(VAELII)) as fn() -> tempfile::TempDir),
        (&["--project-dir", "vaelii"][..], || {
            nested("vaelii", VAELII)
        }),
    ] {
        let without = both_forges(extra, repo);
        let with_args = [extra, &["--test-args-file", "./ci//args"]].concat();
        let workflows = both_forges(&with_args, repo);
        for (workflow, without) in workflows.iter().zip(&without) {
            assert_two_jobs(workflow);
            assert_eq!(step_names(workflow, "test"), step_names(without, "test"));
            for job in ["test", "publish"] {
                for (step, other) in workflow["jobs"][job]["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(without["jobs"][job]["steps"].as_array().unwrap())
                {
                    if step["name"] == "Run tests and export data" {
                        assert_eq!(without_run(step), without_run(other));
                    } else {
                        assert_eq!(step, other);
                    }
                }
            }
            let run = step(workflow, "test", "Run tests and export data")["run"]
                .as_str()
                .unwrap();
            assert!(run.contains("mapfile -t"), "{run}");
            assert!(run.contains("args_file='ci/args'"), "{run}");
            assert!(
                run.trim_end()
                    .ends_with(r#"--commit "$GITHUB_SHA" -- "${args[@]}""#),
                "{run}"
            );
            for unsafe_ in ["eval", "$(cat", "`"] {
                assert!(!run.contains(unsafe_), "{unsafe_}: {run}");
            }
        }
        assert_forges_agree(&with_args, repo);
    }
}

/// `step` without its `run`.
fn without_run(step: &serde_json::Value) -> serde_json::Value {
    without(step, &["run"])
}

#[test]
fn the_header_names_test_args_file_and_the_selection_it_runs() {
    let default = "# greenshift runs the Leiningen tests (`lein test`'s default selection),\n";
    for (extra, repo, again, file) in [
        (
            &["--test-args-file", TEST_ARGS][..],
            project as fn(&str) -> tempfile::TempDir,
            "# Give it the same --test-args-file too, or the new workflow goes without it.\n",
            TEST_ARGS,
        ),
        (
            &["--project-dir", "vaelii", "--test-args-file", TEST_ARGS][..],
            |text| nested("vaelii", text),
            "# Give it the same --project-dir and --test-args-file too, or the new workflow\n\
             # goes without them.\n",
            "vaelii/test-selection",
        ),
        (
            &[
                "--setup",
                "make",
                "--project-dir",
                "vaelii",
                "--test-args-file",
                TEST_ARGS,
            ][..],
            |text| nested("vaelii", text),
            "# Give it the same --setup, --project-dir and --test-args-file too, or the new\n\
             # workflow goes without them.\n",
            "vaelii/test-selection",
        ),
    ] {
        for (args, path) in [
            (&ci_args()[..], WORKFLOW),
            (&forgejo_args()[..], FORGEJO_WORKFLOW),
        ] {
            let dir = repo(VAELII);
            init(dir.path(), &[args, extra].concat()).assert().success();
            let written = read(dir.path(), path);
            let header: String = written
                .lines()
                .take_while(|line| line.starts_with('#'))
                .map(|line| format!("{line}\n"))
                .collect();
            assert!(header.contains(again), "{extra:?}: {header}");
            assert!(!header.contains(default), "{extra:?}: {header}");
            assert!(
                words(&header).contains(&format!(
                    "the Leiningen tests (those `lein test` selects with the arguments in {file}, \
                     one per line, from --test-args-file),"
                )),
                "{extra:?}: {header}"
            );
        }
    }
    // Without it, the header says the default selection, as before.
    let dir = project(VAELII);
    init(dir.path(), &ci_args()).assert().success();
    assert!(read(dir.path(), WORKFLOW).contains(default));
}

/// `text`'s words, one space apart, its comment markers dropped.
fn words(text: &str) -> String {
    text.split_whitespace()
        .filter(|word| *word != "#")
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the workflow's test step does, run in bash as the forge runs it
/// (`bash --noprofile --norc -eo pipefail`), in a scratch workspace with
/// `files` in the project directory and a stub greenshift that records
/// its arguments.
struct TestStep {
    ok: bool,
    stdout: String,
    /// The stub's arguments, if it ran.
    argv: Option<Vec<String>>,
    /// The project directory the step ran in.
    _workspace: tempfile::TempDir,
    project: PathBuf,
}

fn run_test_step(workflow: &serde_json::Value, files: &[(&str, &[u8])]) -> TestStep {
    let run = step(workflow, "test", "Run tests and export data");
    assert_eq!(run["shell"], "bash");
    let script = run["run"].as_str().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let project = match run["working-directory"].as_str() {
        Some(dir) => workspace.path().join(dir),
        None => workspace.path().to_owned(),
    };
    std::fs::create_dir_all(&project).unwrap();
    for (name, content) in files {
        let path = project.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let temp = workspace.path().join("runner-temp");
    std::fs::create_dir(&temp).unwrap();
    let argv_file = workspace.path().join("argv");
    let stub = temp.join("greenshift");
    std::fs::write(
        &stub,
        "#!/usr/bin/env bash\nprintf '%s\\0' \"$@\" > \"$GREENSHIFT_TEST_ARGV\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c", script])
        .current_dir(&project)
        .env("RUNNER_TEMP", &temp)
        .env("GITHUB_WORKSPACE", workspace.path())
        .env("GITHUB_SHA", "f".repeat(40))
        .env("HISTORY_REMOTE", "https://example.invalid/repo.git")
        .env("GREENSHIFT_TEST_ARGV", &argv_file)
        .output()
        .unwrap();
    let argv = std::fs::read(&argv_file).ok().map(|bytes| {
        bytes
            .split(|b| *b == 0)
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect::<Vec<_>>()
    });
    // `printf '%s\0'` ends every argument with a NUL: drop the empty
    // piece after the last.
    let argv = argv.map(|mut argv| {
        assert_eq!(argv.pop().as_deref(), Some(""));
        argv
    });
    TestStep {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        argv,
        _workspace: workspace,
        project,
    }
}

/// The arguments greenshift was given after `--`, which it gives `lein
/// test`, checking that what comes before is the workflow's own.
fn lein_args(argv: &[String]) -> Vec<String> {
    let at = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or_else(|| panic!("no -- in {argv:?}"));
    assert_eq!(&argv[..3], ["run", "lein", "--history-remote"], "{argv:?}");
    assert_eq!(argv[at - 2..at], ["--commit".to_owned(), "f".repeat(40)]);
    argv[at + 1..].to_vec()
}

#[test]
fn the_test_step_passes_each_line_of_the_file_as_one_argument() {
    for (extra, repo) in [
        (
            &["--test-args-file", "ci/args"][..],
            (|| project(VAELII)) as fn() -> tempfile::TempDir,
        ),
        (
            &["--project-dir", "vaelii", "--test-args-file", "ci/args"][..],
            || nested("vaelii", VAELII),
        ),
    ] {
        for workflow in &both_forges(extra, repo) {
            // Three arguments, blank lines between (one empty, one only
            // whitespace), and no newline at the end.
            let ran = run_test_step(
                workflow,
                &[(
                    "ci/args",
                    b":integration\n\nvaelii.core-test\n \t \n:only vaelii.x/y",
                )],
            );
            assert!(ran.ok, "{}", ran.stdout);
            assert_eq!(
                lein_args(&ran.argv.unwrap()),
                [":integration", "vaelii.core-test", ":only vaelii.x/y"]
            );
            // Nothing in a line is the shell's: each is one literal argument.
            let lines = [
                "has spaces $(touch x) `touch y` ${HOME}",
                "*",
                "-v",
                "--",
                "'quoted' \"too\"",
                "  padded  ",
                "a\\nb;touch z",
            ];
            let ran = run_test_step(workflow, &[("ci/args", lines.join("\n").as_bytes())]);
            assert!(ran.ok, "{}", ran.stdout);
            assert_eq!(lein_args(&ran.argv.unwrap()), lines);
            for made in ["x", "y", "z"] {
                assert!(!ran.project.join(made).exists(), "{made}");
            }
        }
    }
}

#[test]
fn a_missing_or_empty_arguments_file_fails_the_test_step() {
    for (extra, repo, shown) in [
        (
            &["--test-args-file", "ci/args"][..],
            (|| project(VAELII)) as fn() -> tempfile::TempDir,
            "ci/args",
        ),
        (
            &["--project-dir", "vaelii", "--test-args-file", "ci/args"][..],
            || nested("vaelii", VAELII),
            "vaelii/ci/args",
        ),
    ] {
        for workflow in &both_forges(extra, repo) {
            for (files, why) in [
                (&[][..], "is missing"),
                // Elsewhere than where the step looks is missing too.
                (&[("args", &b"a\n"[..])][..], "is missing"),
                (&[("ci/args/x", &b"a\n"[..])][..], "is missing"),
                (&[("ci/args", &b""[..])][..], "has no arguments"),
                (&[("ci/args", &b"\n\n\n"[..])][..], "has no arguments"),
                (&[("ci/args", &b"  \n\t\n \t "[..])][..], "has no arguments"),
                (&[("ci/args", &b"a\r\nb\r\n"[..])][..], "carriage return"),
                (&[("ci/args", &b"a\0b\n"[..])][..], "NUL"),
            ] {
                let ran = run_test_step(workflow, files);
                assert!(!ran.ok, "{files:?}: {}", ran.stdout);
                assert!(ran.argv.is_none(), "greenshift ran: {files:?}");
                assert!(
                    ran.stdout.contains(&format!("::error::{shown} ")),
                    "{files:?}: {}",
                    ran.stdout
                );
                assert!(ran.stdout.contains(why), "{files:?}: {}", ran.stdout);
                assert!(ran.stdout.contains("--test-args-file"), "{}", ran.stdout);
            }
        }
    }
}

#[test]
fn without_test_args_file_the_test_step_passes_no_lein_arguments() {
    for workflow in &both_forges(&[], || project(VAELII)) {
        let ran = run_test_step(workflow, &[]);
        assert!(ran.ok, "{}", ran.stdout);
        let argv = ran.argv.unwrap();
        assert!(!argv.contains(&"--".to_owned()), "{argv:?}");
        assert_eq!(argv.last().unwrap(), &"f".repeat(40));
    }
}

/// The sha256 of Leiningen 2.12.0's `bin/lein`, which the Forgejo
/// workflow pins.
const LEIN_SHA256: &str = "f8e1266c0c78c08bd4af6e111889ecc316c9dd56d1e8645bbee6c1703d351bc3";
const LEIN_URL: &str = "https://raw.githubusercontent.com/technomancy/leiningen/2.12.0/bin/lein";

#[test]
fn forgejo_installs_lein_itself_and_github_still_uses_setup_clojure() {
    let [github, forgejo] = both_forges(&[], || project(VAELII));
    let (github, forgejo) = (
        serde_json::to_string(&github).unwrap(),
        serde_json::to_string(&forgejo).unwrap(),
    );
    assert!(github.contains("setup-clojure"), "{github}");
    assert!(!forgejo.contains("setup-clojure"), "{forgejo}");
    // Nor in a comment, which the JSON leaves out.
    let dir = project(VAELII);
    init(dir.path(), &forgejo_args()).assert().success();
    let written = read(dir.path(), FORGEJO_WORKFLOW);
    assert!(!written.contains("setup-clojure"), "{written}");
}

/// What the Forgejo workflow's "Set up Leiningen" step does, run in bash
/// as the forge runs it, with `script` for its `run`, and a stub `curl`
/// first on PATH that records its arguments and writes `download` where
/// it is told to: nothing is fetched.
struct LeinStep {
    ok: bool,
    stdout: String,
    /// The stub curl's arguments, if it ran.
    curl: Option<Vec<String>>,
    /// What the step appended to `$GITHUB_PATH`.
    github_path: String,
    /// Every file left under `$RUNNER_TEMP`, with its content.
    temp: Vec<(PathBuf, Vec<u8>)>,
    runner_temp: PathBuf,
    _scratch: tempfile::TempDir,
}

fn lein_step_script(workflow: &serde_json::Value) -> String {
    let lein = step(workflow, "test", "Set up Leiningen");
    assert_eq!(lein["shell"], "bash");
    assert!(lein["uses"].is_null(), "{lein}");
    lein["run"].as_str().unwrap().to_owned()
}

fn run_lein_step(script: &str, download: &[u8]) -> LeinStep {
    let scratch = tempfile::tempdir().unwrap();
    let bin = scratch.path().join("bin");
    let temp = scratch.path().join("runner-temp");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&temp).unwrap();
    let download_file = scratch.path().join("download");
    std::fs::write(&download_file, download).unwrap();
    let curl_argv = scratch.path().join("curl-argv");
    let github_path = scratch.path().join("github-path");
    std::fs::write(&github_path, "").unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        concat!(
            "#!/usr/bin/env bash\n",
            "printf '%s\\0' \"$@\" > \"$STUB_CURL_ARGV\"\n",
            "while (($#)); do\n",
            "  if [[ $1 == --output || $1 == -o ]]; then cp \"$STUB_CURL_DOWNLOAD\" \"$2\"; exit 0; fi\n",
            "  shift\n",
            "done\n",
            "echo 'stub curl: no --output' >&2; exit 2\n",
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c", script])
        .current_dir(scratch.path())
        .env("PATH", path)
        .env("RUNNER_TEMP", &temp)
        .env("GITHUB_PATH", &github_path)
        .env("STUB_CURL_ARGV", &curl_argv)
        .env("STUB_CURL_DOWNLOAD", &download_file)
        .output()
        .unwrap();
    let curl = std::fs::read(&curl_argv).ok().map(|bytes| {
        let mut argv: Vec<String> = bytes
            .split(|b| *b == 0)
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        assert_eq!(argv.pop().as_deref(), Some(""));
        argv
    });
    LeinStep {
        ok: output.status.success(),
        stdout: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        curl,
        github_path: std::fs::read_to_string(&github_path).unwrap(),
        temp: snapshot(&temp),
        runner_temp: temp,
        _scratch: scratch,
    }
}

fn forgejo_lein_scripts() -> Vec<String> {
    // The plain workflow and vaelii-lacuna's overlay shape.
    let plain = project(VAELII);
    init(plain.path(), &forgejo_args()).assert().success();
    let overlay = tempfile::tempdir().unwrap();
    std::fs::write(overlay.path().join("overlay.sh"), "#!/bin/sh\n").unwrap();
    init(
        overlay.path(),
        &[
            &forgejo_args()[..],
            &["--setup", "./overlay.sh", "--project-dir", "vaelii"],
        ]
        .concat(),
    )
    .assert()
    .success();
    [plain, overlay]
        .iter()
        .map(|dir| lein_step_script(&yaml(&dir.path().join(FORGEJO_WORKFLOW))))
        .collect()
}

#[test]
fn the_lein_step_pins_exactly_the_real_lein_by_url_and_checksum() {
    for script in forgejo_lein_scripts() {
        assert_eq!(script.matches(LEIN_SHA256).count(), 1, "{script}");
        assert!(script.contains("sha256sum --check --strict"), "{script}");
        assert!(script.starts_with("set -euo pipefail\n"), "{script}");
        // The stub sees the URL and flags the step gives curl.
        let ran = run_lein_step(&script, b"not lein\n");
        let curl = ran.curl.unwrap();
        assert!(curl.contains(&LEIN_URL.to_owned()), "{curl:?}");
        let joined = curl.join(" ");
        for flag in [
            "--fail",
            "--location",
            "--retry 3",
            "--proto =https",
            "--proto-redir =https",
            "--tlsv1.2",
        ] {
            assert!(joined.contains(flag), "{flag}: {joined}");
        }
    }
}

#[test]
fn a_lein_download_not_matching_the_pin_fails_closed() {
    for script in forgejo_lein_scripts() {
        for download in [&b"#!/bin/sh\necho tampered\n"[..], b""] {
            let ran = run_lein_step(&script, download);
            assert!(!ran.ok, "{}", ran.stdout);
            assert!(ran.curl.is_some(), "the stub was not used: {}", ran.stdout);
            assert!(
                ran.stdout
                    .contains("::error::Leiningen 2.12.0 does not match its pinned SHA-256"),
                "{}",
                ran.stdout
            );
            // Nothing installed, nothing left behind, nothing on PATH.
            assert!(ran.temp.is_empty(), "{:?}", ran.temp);
            assert_eq!(ran.github_path, "");
            assert!(!ran.stdout.contains("tampered"), "{}", ran.stdout);
        }
    }
}

#[test]
fn a_lein_download_matching_the_pin_is_installed_on_the_path_and_run() {
    use sha2::{Digest, Sha256};
    // The real lein is not fetched: the step's pin is swapped for the
    // checksum of a stand-in, so that this shows the checksum is the
    // only gate and what happens once it passes.
    let stand_in = b"#!/usr/bin/env bash\necho \"stand-in lein $*\"\n";
    let sha256: String = Sha256::digest(stand_in)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    for script in forgejo_lein_scripts() {
        let script = script.replacen(LEIN_SHA256, &sha256, 1);
        let ran = run_lein_step(&script, stand_in);
        assert!(ran.ok, "{}", ran.stdout);
        let dir = ran.runner_temp.join("lein-bin");
        assert_eq!(ran.github_path, format!("{}\n", dir.display()));
        assert_eq!(
            ran.temp,
            vec![(PathBuf::from("lein-bin/lein"), stand_in.to_vec())]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("lein"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        }
        // `lein version`, which bootstraps the standalone jar.
        assert!(
            ran.stdout.contains("stand-in lein version"),
            "{}",
            ran.stdout
        );
    }
}
