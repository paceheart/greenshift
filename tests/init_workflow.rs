//! What the workflows `greenshift init nextest` writes do: the trust
//! boundary between the test job and the publisher, as the durable-history
//! reference workflow draws it, and the install step refusing a binary
//! that does not match the pinned checksum. Needs bash, sha256sum and
//! mikefarah's yq v4, as the other workflow tests do.

use std::process::Command;

use serde_json::Value;

const RELEASE: &str = "v0.2.0";

/// The workflow `init nextest --ci <forge>` writes, as JSON, with `sha256`
/// pinned, and `extra` arguments.
fn generated(forge: &str, sha256: &str, extra: &[&str]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "nextest", "--ci", forge, "--release", RELEASE])
        .args(["--sha256", sha256])
        .args(extra)
        .assert()
        .success();
    let path = dir
        .path()
        .join(format!(".{forge}/workflows/greenshift.yml"));
    yaml(&path)
}

/// The YAML file at `path`, as JSON.
fn yaml(path: &std::path::Path) -> Value {
    let output = Command::new("yq")
        .args(["-o=json", "."])
        .arg(path)
        .output()
        .expect("mikefarah yq installed as for cache_guard tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn steps<'w>(workflow: &'w Value, job: &str) -> &'w Vec<Value> {
    workflow["jobs"][job]["steps"].as_array().unwrap()
}

fn step<'w>(workflow: &'w Value, job: &str, name: &str) -> &'w Value {
    steps(workflow, job)
        .iter()
        .find(|step| step["name"] == name)
        .unwrap_or_else(|| panic!("no step {name:?} in {job}"))
}

/// The jobs and steps that make the reference's boundary, whatever the
/// forge: the publisher needs the test job, runs only for pushes to the
/// default branch, never checks out or builds test code, and only
/// greenshift steps see the history token.
fn assert_publisher_separation(workflow: &Value) {
    let publisher = &workflow["jobs"]["publish"];
    assert_eq!(publisher["needs"], "test");
    let condition = publisher["if"].as_str().unwrap();
    assert!(condition.contains("github.event_name == 'push'"));
    assert!(condition.contains("github.event.repository.default_branch"));
    for step in steps(workflow, "publish") {
        assert!(!step["uses"].as_str().unwrap_or("").contains("checkout"));
        assert!(!step["run"].as_str().unwrap_or("").contains("cargo"));
    }
    assert!(workflow["env"]["GREENSHIFT_HISTORY_TOKEN"].is_null());
    // Exactly the greenshift steps that read or write the history get
    // the token.
    for (job, with_token) in [
        (
            "test",
            &["Resolve canonical cache key", "Run tests and export data"][..],
        ),
        ("publish", &["Validate, merge and publish"]),
    ] {
        assert!(workflow["jobs"][job]["env"]["GREENSHIFT_HISTORY_TOKEN"].is_null());
        let given: Vec<&str> = steps(workflow, job)
            .iter()
            .filter(|step| !step["env"]["GREENSHIFT_HISTORY_TOKEN"].is_null())
            .map(|step| {
                assert!(step["run"].as_str().unwrap().contains("greenshift"));
                assert_eq!(
                    step["env"]["GREENSHIFT_HISTORY_TOKEN"],
                    "${{ github.token }}"
                );
                step["name"].as_str().unwrap()
            })
            .collect();
        assert_eq!(given, with_token, "{job}");
    }
    let checkout = steps(workflow, "test")
        .iter()
        .find(|step| step["uses"].as_str().unwrap_or("").contains("checkout@"))
        .unwrap();
    assert_eq!(checkout["with"]["persist-credentials"], false);
    let save = steps(workflow, "publish")
        .iter()
        .find(|step| step["id"] == "save")
        .unwrap();
    assert!(save["if"]
        .as_str()
        .unwrap()
        .contains("steps.artifact.outcome == 'success'"));
    assert!(save["run"]
        .as_str()
        .unwrap()
        .contains("--default-branch \"$DEFAULT_BRANCH\""));
    for (job, id) in [
        ("test", "cache"),
        ("publish", "artifact"),
        ("publish", "cache"),
    ] {
        let step = steps(workflow, job)
            .iter()
            .find(|step| step["id"] == id)
            .unwrap();
        assert_eq!(step["continue-on-error"], true, "{job}.{id}");
    }
    let run = step(workflow, "test", "Run tests and export data")["run"]
        .as_str()
        .unwrap();
    assert!(run.contains("run nextest --history-remote"));
    assert!(run.contains("--history-artifact .greenshift/publish.sqlite"));
    // Every action is pinned to a commit.
    for job in ["test", "publish"] {
        for step in steps(workflow, job) {
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
fn github_keeps_the_reference_trust_boundary() {
    let workflow = generated("github", &"a".repeat(64), &[]);
    assert_publisher_separation(&workflow);
    assert_eq!(workflow["permissions"]["contents"], "read");
    assert_eq!(workflow["jobs"]["test"]["permissions"]["contents"], "read");
    assert_eq!(
        workflow["jobs"]["publish"]["permissions"]["contents"],
        "write"
    );
}

/// How the stand-in `curl` downloads.
#[derive(Clone, Copy)]
enum Curl {
    /// It writes the whole payload.
    Works,
    /// It writes part of the payload and then fails.
    Breaks,
}

/// What running an install step did.
struct Install {
    ok: bool,
    /// What it installed, if anything.
    installed: Option<Vec<u8>>,
    /// Whether it downloaded anything.
    fetched: bool,
    /// Whether what it installed has an execute bit set.
    executable: bool,
}

/// Run the install step's script of `job`, with `curl` replaced by one
/// that downloads `binary` as `curl` says.
fn install(workflow: &Value, job: &str, binary: &[u8], curl: Curl) -> Install {
    let script = step(workflow, job, "Install pinned greenshift release")["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let payload = dir.path().join("payload");
    std::fs::write(&payload, binary).unwrap();
    let urls = dir.path().join("urls");
    fake_curl(&bin, &payload, &urls, curl);
    let temp = dir.path().join("runner-temp");
    std::fs::create_dir(&temp).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = workflow["env"].as_object().unwrap();
    let status = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script])
        .env("PATH", path)
        .env("RUNNER_TEMP", &temp)
        .env(
            "GREENSHIFT_RELEASE",
            env["GREENSHIFT_RELEASE"].as_str().unwrap(),
        )
        .env(
            "GREENSHIFT_SHA256",
            env["GREENSHIFT_SHA256"].as_str().unwrap(),
        )
        .status()
        .unwrap();
    let installed = temp.join("greenshift");
    let leftovers: Vec<_> = std::fs::read_dir(&temp)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "greenshift")
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    let release = env["GREENSHIFT_RELEASE"].as_str().unwrap();
    let fetched = std::fs::read_to_string(&urls).unwrap_or_default();
    assert!(
        fetched.is_empty()
            || fetched
                == format!(
                    "https://github.com/paceheart/greenshift/releases/download/{release}/\
             greenshift-{release}-x86_64-unknown-linux-musl\n"
                ),
        "the one download, if any, is the pinned release's Linux binary: {fetched}"
    );
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&installed).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let executable = installed.exists();
    Install {
        ok: status.success(),
        executable,
        installed: installed
            .exists()
            .then(|| std::fs::read(installed).unwrap()),
        fetched: !fetched.is_empty(),
    }
}

/// An executable `curl` in `bin` that writes `payload` where the real one
/// would write the download, and appends each URL it is given to `urls`.
/// Like the real one with --fail, it fails without an --output. It knows
/// only the options curl 7.61 (RHEL 8's) knows, of those the scripts may
/// use, and refuses any other as an older curl does: exit 2. When it
/// [`Curl::Breaks`], it writes part of the download and then fails as the
/// real one does on a broken transfer: exit 22, leaving the partial file.
fn fake_curl(bin: &std::path::Path, payload: &std::path::Path, urls: &std::path::Path, how: Curl) {
    let curl = bin.join("curl");
    let finish = match how {
        Curl::Breaks => "head -c 3 {payload} > \"$out\"\nexit 22\n",
        Curl::Works => "cp {payload} \"$out\"\n",
    }
    .replace("{payload}", &format!("{payload:?}"));
    std::fs::write(
        &curl,
        format!(
            "#!/usr/bin/env bash\nout=\nwhile [[ $# -gt 0 ]]; do\n  \
             case $1 in\n    --output) out=$2; shift ;;\n    \
             --retry|--proto|--proto-redir) shift ;;\n    \
             --fail|--location|--tlsv1.2) ;;\n    \
             https://*) echo \"$1\" >> {urls:?} ;;\n    \
             *) echo \"curl: option $1: is unknown\" >&2; exit 2 ;;\n  esac\n  \
             shift\ndone\n[[ -n $out ]] || exit 22\n{finish}"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn both_jobs_inherit_one_release_and_checksum_pin_without_overrides() {
    let checksum = "a".repeat(64);
    for forge in forges() {
        let workflow = generated(forge, &checksum, &[]);
        assert_eq!(workflow["env"]["GREENSHIFT_RELEASE"], RELEASE);
        assert_eq!(workflow["env"]["GREENSHIFT_SHA256"], checksum);
        for job in ["test", "publish"] {
            for key in ["GREENSHIFT_RELEASE", "GREENSHIFT_SHA256"] {
                assert!(workflow["jobs"][job]["env"][key].is_null());
                for step in steps(&workflow, job) {
                    assert!(step["env"][key].is_null(), "{forge} {job}: {key} override");
                }
            }
        }
        assert_eq!(
            step(&workflow, "test", "Install pinned greenshift release")["run"],
            step(&workflow, "publish", "Install pinned greenshift release")["run"]
        );
    }
}

#[cfg(unix)]
#[test]
fn the_install_step_fails_closed_on_a_checksum_mismatch() {
    let binary = b"#!/bin/sh\necho pretend greenshift\n";
    let pinned = sha256_hex(binary);
    for forge in forges() {
        let workflow = generated(forge, &pinned, &[]);
        for job in ["test", "publish"] {
            let good = install(&workflow, job, binary, Curl::Works);
            assert!(
                good.ok && good.fetched,
                "{forge} {job}: the matching binary is installed"
            );
            assert_eq!(good.installed.as_deref(), Some(&binary[..]));
            assert!(good.executable, "{forge} {job}: and can be run");
            let bad = install(&workflow, job, b"tampered", Curl::Works);
            assert!(!bad.ok, "{forge} {job}: a mismatch fails the step");
            assert_eq!(bad.installed, None, "{forge} {job}: nothing is installed");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_broken_download_fails_the_install_step_and_leaves_nothing() {
    let binary = b"#!/bin/sh\necho pretend greenshift\n";
    for forge in forges() {
        let workflow = generated(forge, &sha256_hex(binary), &[]);
        for job in ["test", "publish"] {
            // install() asserts that nothing but an installed binary is left.
            let broken = install(&workflow, job, binary, Curl::Breaks);
            assert!(
                !broken.ok,
                "{forge} {job}: a broken download fails the step"
            );
            assert_eq!(
                broken.installed, None,
                "{forge} {job}: nothing is installed"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn the_install_step_refuses_a_hand_edited_pin_before_downloading() {
    let binary = b"#!/bin/sh\necho pretend greenshift\n";
    let sha256 = sha256_hex(binary);
    let too_long = format!("{sha256}0");
    for forge in forges() {
        for (name, bad) in [
            ("GREENSHIFT_RELEASE", "v0.2.0/../../evil"),
            ("GREENSHIFT_RELEASE", "main"),
            ("GREENSHIFT_SHA256", "not-a-checksum"),
            ("GREENSHIFT_SHA256", &sha256[1..]),
            ("GREENSHIFT_SHA256", &too_long),
        ] {
            let mut workflow = generated(forge, &sha256, &[]);
            workflow["env"][name] = Value::String(bad.into());
            for job in ["test", "publish"] {
                let refused = install(&workflow, job, binary, Curl::Works);
                assert!(!refused.ok, "{forge} {job}: {name}={bad} fails the step");
                assert!(
                    !refused.fetched,
                    "{forge} {job}: {name}={bad} downloads nothing"
                );
                assert_eq!(refused.installed, None);
            }
        }
    }
}

#[test]
fn the_pin_reaches_the_workflow_as_the_strings_given() {
    // A checksum of decimal digits only, or digits around one `e`, is a
    // number if written bare.
    let digits = "1".repeat(64);
    let exponent = format!("0e{}", "0".repeat(62));
    for sha256 in [digits, exponent] {
        for forge in forges() {
            let workflow = generated(forge, &sha256, &[]);
            assert_eq!(
                workflow["env"]["GREENSHIFT_SHA256"],
                Value::String(sha256.clone()),
                "{forge}"
            );
            assert_eq!(
                workflow["env"]["GREENSHIFT_RELEASE"],
                Value::String(RELEASE.into()),
                "{forge}"
            );
        }
    }
}

/// The forges `init nextest` writes for.
fn forges() -> &'static [&'static str] {
    &["github", "forgejo"]
}

#[test]
fn forgejo_keeps_the_publisher_apart_but_claims_no_permissions() {
    let workflow = generated("forgejo", &"a".repeat(64), &[]);
    assert_publisher_separation(&workflow);
    // Forgejo ignores `permissions`; writing them would claim an
    // isolation the test job does not have.
    assert!(workflow["permissions"].is_null());
    for job in ["test", "publish"] {
        assert!(workflow["jobs"][job]["permissions"].is_null(), "{job}");
        for step in steps(&workflow, job) {
            if let Some(uses) = step["uses"].as_str() {
                // Not resolved against the instance's DEFAULT_ACTIONS_URL.
                assert!(uses.starts_with("https://"), "{uses}");
                if uses.contains("artifact@") {
                    // Stock v4 artifact actions refuse to run on Forgejo.
                    assert!(uses.contains("/forgejo/"), "{uses}");
                }
            }
        }
    }
    let rust = step(&workflow, "test", "Install Rust when the runner has none");
    assert!(rust["run"].as_str().unwrap().contains("sha256sum --check"));
}

#[test]
fn every_runner_label_reaches_the_workflow_as_that_string() {
    // Each would be something else, or no YAML, if written bare.
    for label in [
        "-",
        ":",
        "a:",
        "null",
        "true",
        "0x1F",
        ".inf",
        "ubuntu-24.04",
    ] {
        for forge in forges() {
            let workflow = generated(forge, &"a".repeat(64), &["--runs-on", label]);
            for job in ["test", "publish"] {
                assert_eq!(
                    workflow["jobs"][job]["runs-on"],
                    Value::String(label.into()),
                    "{forge} {job} {label:?}"
                );
            }
        }
    }
}

/// `text` with whitespace runs made single spaces.
fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_forgejo_header_quotes_the_capability_gate_as_the_design_says_it() {
    let root = env!("CARGO_MANIFEST_DIR");
    let doc = std::fs::read_to_string(format!("{root}/docs/design/durable-history.md")).unwrap();
    // Only the section the header names.
    let (_, gate) = doc
        .split_once("\n## Forgejo capability gate\n")
        .expect("the design has the section");
    let gate = gate.split("\n## ").next().unwrap();
    let doc = words(&gate.replace('`', "").replace("**", ""));
    let dir = tempfile::tempdir().unwrap();
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .current_dir(dir.path())
        .args(["init", "nextest", "--ci", "forgejo"])
        .args(["--release", RELEASE, "--sha256", &"a".repeat(64)])
        .assert()
        .success();
    let text =
        std::fs::read_to_string(dir.path().join(".forgejo/workflows/greenshift.yml")).unwrap();
    // The quoted paragraphs: the comment lines indented under "says:".
    let mut quoted = vec![String::new()];
    let mut inside = false;
    for line in text.lines().take_while(|line| line.starts_with('#')) {
        let body = line.trim_start_matches('#');
        if body.trim_end().ends_with("says:") {
            inside = true;
        } else if inside && body.starts_with("   ") {
            quoted.last_mut().unwrap().push_str(&format!(" {body}"));
        } else if inside && body.trim().is_empty() {
            quoted.push(String::new());
        } else if inside {
            break;
        }
    }
    let quoted: Vec<String> = quoted
        .iter()
        .map(|q| words(q))
        .filter(|q| !q.is_empty())
        .collect();
    assert!(quoted.len() >= 2, "{quoted:?}");
    for paragraph in &quoted {
        assert!(
            doc.contains(paragraph.as_str()),
            "not in the design: {paragraph:?}"
        );
    }
    // Whole sentences, none dropped from either paragraph.
    assert!(
        quoted[0].ends_with("read-only-test-job guarantee."),
        "{:?}",
        quoted[0]
    );
    assert!(doc.starts_with(&quoted[0]), "{:?}", quoted[0]);
    assert!(
        quoted[1].ends_with("is offered as isolated."),
        "{:?}",
        quoted[1]
    );
}

/// Each `uses` of a step in `workflow`'s jobs, as (action, commit).
fn pins(workflow: &Value) -> Vec<(String, String)> {
    workflow["jobs"]
        .as_object()
        .unwrap()
        .keys()
        .flat_map(|job| steps(workflow, job))
        .filter_map(|step| step["uses"].as_str())
        .map(|uses| {
            let (action, rev) = uses.rsplit_once('@').unwrap();
            (action.to_owned(), rev.to_owned())
        })
        .collect()
}

/// The `cargo install ... cargo-nextest` command of `workflow`'s test job.
fn nextest_install(workflow: &Value) -> String {
    steps(workflow, "test")
        .iter()
        .filter_map(|step| step["run"].as_str())
        .find(|run| run.contains("cargo-nextest"))
        .expect("the test job installs cargo-nextest")
        .trim()
        .to_owned()
}

#[test]
fn the_pins_are_the_reference_workflows() {
    let root = env!("CARGO_MANIFEST_DIR");
    let reference = yaml(std::path::Path::new(&format!(
        "{root}/examples/durable-history.github.yml"
    )));
    let reference_pins = pins(&reference);
    for forge in forges() {
        let workflow = generated(forge, &"a".repeat(64), &[]);
        assert_eq!(
            nextest_install(&workflow),
            nextest_install(&reference),
            "{forge}"
        );
        let mut compared = 0;
        for (action, rev) in pins(&workflow) {
            // Forgejo's mirror of GitHub's actions is the same repository
            // at the same commits; its forks of the artifact actions have
            // no counterpart in the reference.
            let action = action
                .strip_prefix("https://data.forgejo.org/")
                .unwrap_or(&action);
            if action.starts_with("https://code.forgejo.org/forgejo/") {
                continue;
            }
            let expected: Vec<&String> = reference_pins
                .iter()
                .filter(|(name, _)| name == action)
                .map(|(_, rev)| rev)
                .collect();
            assert!(
                !expected.is_empty(),
                "{forge}: {action} is not in the reference"
            );
            assert!(
                expected.iter().all(|pinned| **pinned == rev),
                "{forge}: {action}@{rev}, the reference pins {expected:?}"
            );
            compared += 1;
        }
        let want = if *forge == "github" { 5 } else { 3 };
        assert_eq!(
            compared, want,
            "{forge}: every corresponding action compared"
        );
    }
}

/// What running the Forgejo test job's Rust step did.
#[cfg(unix)]
struct RustStep {
    ok: bool,
    /// Each URL fetched, one per line.
    urls: String,
    /// Whether the downloaded rustup-init was run.
    ran: bool,
    /// The arguments it was run with, one per line, if it records them.
    args: String,
    /// What the step appended to `$GITHUB_PATH`.
    github_path: String,
    /// The `$HOME` it ran with.
    home: std::path::PathBuf,
}

/// Whether the runner already has `cargo`.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Cargo {
    OnRunner,
    Missing,
}

/// The rustup-init checksum the Rust step pins.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Pin {
    /// The published one, which the stand-in payload does not match.
    Published,
    /// The stand-in payload's, standing in for the published rustup-init.
    Payload,
}

/// Run the Forgejo test job's Rust step with a PATH that has only the
/// tools it needs, `cargo` among them as `cargo` says, and a `curl` that
/// downloads `payload` as `curl` says, with the rustup-init checksum
/// `pin`; `cargo_home` is `$CARGO_HOME`, if any.
#[cfg(unix)]
fn install_rust(
    cargo: Cargo,
    payload: &[u8],
    pin: Pin,
    cargo_home: Option<&str>,
    curl: Curl,
) -> RustStep {
    let workflow = generated("forgejo", &"a".repeat(64), &[]);
    let mut script = step(&workflow, "test", "Install Rust when the runner has none")["run"]
        .as_str()
        .unwrap()
        .to_owned();
    if let Pin::Payload = pin {
        let pin = script
            .lines()
            .find_map(|line| line.trim().strip_prefix("rustup_sha256="))
            .expect("the step pins rustup-init's checksum")
            .to_owned();
        script = script.replace(&pin, &sha256_hex(payload));
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for tool in [
        "bash",
        "sha256sum",
        "rm",
        "chmod",
        "env",
        "cp",
        "touch",
        "head",
    ] {
        let found = Command::new("bash")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .unwrap();
        let found = String::from_utf8(found.stdout).unwrap();
        std::os::unix::fs::symlink(found.trim(), bin.join(tool)).unwrap();
    }
    let ran = dir.path().join("ran");
    if let Cargo::OnRunner = cargo {
        let cargo = bin.join("cargo");
        std::fs::write(&cargo, "#!/usr/bin/env bash\necho cargo 1.0.0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let download = dir.path().join("payload");
    std::fs::write(&download, payload).unwrap();
    let urls = dir.path().join("urls");
    fake_curl(&bin, &download, &urls, curl);
    let temp = dir.path().join("runner-temp");
    std::fs::create_dir(&temp).unwrap();
    let home = dir.path().join("home");
    let github_path = dir.path().join("github-path");
    let mut cmd = Command::new(bin.join("bash"));
    cmd.args(["--noprofile", "--norc", "-c", &script])
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", &home)
        .env("RUNNER_TEMP", &temp)
        .env("RAN", &ran)
        .env("GITHUB_PATH", &github_path);
    if let Some(cargo_home) = cargo_home {
        cmd.env("CARGO_HOME", cargo_home);
    }
    let status = cmd.status().unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap().collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    RustStep {
        ok: status.success(),
        urls: std::fs::read_to_string(&urls).unwrap_or_default(),
        ran: ran.exists(),
        args: std::fs::read_to_string(&ran).unwrap_or_default(),
        github_path: std::fs::read_to_string(&github_path).unwrap_or_default(),
        home,
    }
}

#[cfg(unix)]
#[test]
fn the_rust_step_uses_the_runners_cargo_and_otherwise_installs_only_a_matching_rustup() {
    let rustup = b"#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$RAN\"\n";
    let url =
        "https://static.rust-lang.org/rustup/archive/1.29.3/x86_64-unknown-linux-gnu/rustup-init\n";
    let present = install_rust(Cargo::OnRunner, rustup, Pin::Published, None, Curl::Works);
    assert!(
        present.ok && present.urls.is_empty() && !present.ran,
        "cargo present: nothing fetched"
    );
    assert!(present.github_path.is_empty(), "and PATH left as it is");
    let mismatch = install_rust(Cargo::Missing, rustup, Pin::Published, None, Curl::Works);
    assert!(
        !mismatch.ok,
        "a rustup-init that does not match the pin fails the step"
    );
    assert!(!mismatch.ran, "and is never run");
    assert!(mismatch.github_path.is_empty());
    assert_eq!(mismatch.urls, url);
    let matching = install_rust(Cargo::Missing, rustup, Pin::Payload, None, Curl::Works);
    assert!(matching.ok, "a matching rustup-init is installed");
    assert!(matching.ran, "and run");
    // Without -y, rustup-init asks for confirmation, and a runner has no
    // terminal to give it.
    for arg in ["-y", "--no-modify-path"] {
        assert!(
            matching.args.lines().any(|given| given == arg),
            "rustup-init is run with {arg}: {:?}",
            matching.args
        );
    }
    assert_eq!(matching.urls, url);
    assert_eq!(
        matching.github_path,
        format!("{}/.cargo/bin\n", matching.home.display()),
        "and cargo's directory is put on PATH for later steps"
    );
    let elsewhere = install_rust(
        Cargo::Missing,
        rustup,
        Pin::Payload,
        Some("/opt/cargo"),
        Curl::Works,
    );
    assert!(elsewhere.ok && elsewhere.ran);
    assert_eq!(
        elsewhere.github_path, "/opt/cargo/bin\n",
        "$CARGO_HOME wins"
    );
}

#[cfg(unix)]
#[test]
fn a_broken_rustup_download_or_a_failing_rustup_fails_the_step_and_leaves_nothing() {
    // install_rust() asserts that nothing is left in $RUNNER_TEMP.
    let rustup = b"#!/usr/bin/env bash\ntouch \"$RAN\"\n";
    let broken = install_rust(Cargo::Missing, rustup, Pin::Payload, None, Curl::Breaks);
    assert!(!broken.ok, "a broken download fails the step");
    assert!(!broken.ran && broken.github_path.is_empty());
    let failing = b"#!/usr/bin/env bash\ntouch \"$RAN\"\nexit 1\n";
    let failed = install_rust(Cargo::Missing, failing, Pin::Payload, None, Curl::Works);
    assert!(failed.ran, "the matching rustup-init is run");
    assert!(!failed.ok, "and its failure fails the step");
    assert!(failed.github_path.is_empty(), "and PATH is left as it is");
}
