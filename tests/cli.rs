use assert_cmd::Command;
use predicates::prelude::*;

/// The binary, without the CI branch and tag variables of the environment
/// this test runs in, so a run records neither unless a test sets one.
fn greenshift() -> Command {
    let mut cmd = Command::cargo_bin("greenshift").expect("binary builds");
    for var in greenshift::cli::CI_REF_VARS {
        cmd.env_remove(var);
    }
    cmd
}

#[test]
fn version_prints_crate_version() {
    greenshift()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn history_commands_reject_flags_they_do_not_apply() {
    for args in [
        vec![
            "history",
            "cache-key",
            "--remote",
            ".",
            "--db",
            "unused.sqlite",
        ],
        vec![
            "history",
            "cache-key",
            "--remote",
            ".",
            "--cache-dir",
            "unused",
        ],
        vec![
            "history",
            "restore",
            "--remote",
            ".",
            "--default-branch",
            "main",
        ],
    ] {
        greenshift()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

#[test]
fn help_describes_the_tool() {
    greenshift()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: greenshift"));
}

#[test]
fn ingest_help_distinguishes_new_replayed_and_overlapping_bundles() {
    for args in [vec!["--help"], vec!["ingest", "--help"]] {
        let output = greenshift()
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let help = String::from_utf8(output).unwrap();
        let normalized = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("all-new JUnit reports as one aggregate run"),
            "{help}"
        );
        assert!(normalized.contains("all-seen replays no-op"), "{help}");
        assert!(
            normalized.contains("mixing seen/unseen reports fails without recording"),
            "{help}"
        );
        assert!(!normalized.contains("all as one run"), "{help}");
    }
}

/// A name holding a terminal escape and a newline, and how greenshift
/// shows it.
const EVIL: &str = "\u{1b}[31mred\nline";
const EVIL_SHOWN: &str = r"\u{1b}[31mred\nline";

#[cfg(unix)]
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

/// Escaped twice: the backslash of an escaped ESC escaped again.
const EVIL_TWICE: &str = r"\\u{1b}";

/// The output holds `shown`, escaped exactly once, and neither a raw ESC
/// nor `EVIL` itself.
#[cfg(unix)]
fn shows_escaped(output: &[u8], shown: &str) {
    let text = String::from_utf8_lossy(output);
    assert!(text.contains(shown), "{text:?}");
    assert!(!text.contains(EVIL_TWICE), "escaped twice: {text:?}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(!text.contains(EVIL), "{text:?}");
}

// Control characters are not allowed in Windows file names.
#[cfg(unix)]
#[test]
fn a_junit_path_from_argv_is_shown_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let out = greenshift()
        .arg("ingest")
        .arg("--db")
        .arg(dir.path().join("history.sqlite"))
        .arg(dir.path().join(EVIL))
        .assert()
        .failure()
        .code(1);
    shows_escaped(
        &out.get_output().stderr,
        &format!("{EVIL_SHOWN}: could not read file"),
    );
}

#[cfg(unix)]
#[test]
fn a_db_path_from_argv_is_shown_escaped_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let out = greenshift()
        .arg("ingest")
        .arg("--db")
        .arg(dir.path().join(EVIL))
        .arg(fixture("nextest.xml"))
        .assert()
        .success();
    shows_escaped(&out.get_output().stdout, &format!("{EVIL_SHOWN}\n"));
}

#[cfg(unix)]
#[test]
fn a_branch_or_tag_from_argv_or_the_ci_env_is_shown_escaped() {
    let tag_push = [("GITHUB_REF_NAME", EVIL), ("GITHUB_REF_TYPE", "tag")];
    for (flag, env, kind) in [
        (&["--branch", EVIL][..], &[][..], "branch"),
        (&[], &[("GITHUB_HEAD_REF", EVIL)], "branch"),
        (&["--tag", EVIL], &[], "tag"),
        (&[], &tag_push, "tag"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let out = greenshift()
            .envs(env.iter().copied())
            .arg("ingest")
            .arg("--db")
            .arg(dir.path().join("history.sqlite"))
            .args(flag)
            .arg(fixture("nextest.xml"))
            .assert()
            .success();
        shows_escaped(
            &out.get_output().stdout,
            &format!(" on {kind} {EVIL_SHOWN}\n"),
        );
    }
}

#[cfg(unix)]
#[test]
fn a_db_path_from_argv_is_shown_escaped_in_store_errors() {
    let dir = tempfile::tempdir().unwrap();
    // A regular file where the store's directory would go.
    std::fs::write(dir.path().join(EVIL), "").unwrap();
    let out = greenshift()
        .arg("ingest")
        .arg("--db")
        .arg(dir.path().join(EVIL).join("history.sqlite"))
        .arg(fixture("nextest.xml"))
        .assert()
        .failure()
        .code(1);
    shows_escaped(
        &out.get_output().stderr,
        &format!("could not create {}", dir.path().join(EVIL_SHOWN).display()),
    );

    let db = dir.path().join(format!("{EVIL}.sqlite"));
    std::fs::write(
        &db,
        "not sqlite at all, but long enough to be read as a header....",
    )
    .unwrap();
    for args in [&["select"][..], &["nextest-filter", "--inventory", "-"]] {
        let out = greenshift()
            .args(args)
            .arg("--db")
            .arg(&db)
            .write_stdin(r#"{"rust-build-meta":{},"rust-suites":{}}"#)
            .assert()
            .failure()
            .code(1);
        shows_escaped(&out.get_output().stderr, &format!("{EVIL_SHOWN}.sqlite"));
    }
}

/// A SQLite file that `select` (read only) and `ingest` (read-write) both
/// refuse, in each way that names the store: another program's database,
/// one from a newer greenshift, and one with a negative schema version.
#[cfg(unix)]
#[test]
fn a_db_path_from_argv_is_shown_escaped_when_the_store_is_refused() {
    use greenshift::store::{APPLICATION_ID, SCHEMA_VERSION};
    let cases = [
        (0, 0, "is not a greenshift history store".to_owned()),
        (
            APPLICATION_ID,
            SCHEMA_VERSION + 1,
            format!("has schema version {}, newer", SCHEMA_VERSION + 1),
        ),
        (
            APPLICATION_ID,
            -1,
            "has schema version -1, which no greenshift writes".to_owned(),
        ),
    ];
    for (application_id, user_version, refusal) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(format!("{EVIL}.sqlite"));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE theirs (x)", []).unwrap();
        conn.pragma_update(None, "application_id", application_id)
            .unwrap();
        conn.pragma_update(None, "user_version", user_version)
            .unwrap();
        drop(conn);
        let shown = format!("{}.sqlite {refusal}", dir.path().join(EVIL_SHOWN).display());
        let mut select = greenshift();
        select.args(["select", "--db"]).arg(&db);
        let mut ingest = greenshift();
        ingest
            .args(["ingest", "--db"])
            .arg(&db)
            .arg(fixture("nextest.xml"));
        for mut cmd in [select, ingest] {
            let out = cmd.assert().failure().code(1);
            shows_escaped(&out.get_output().stderr, &shown);
        }
    }
}

#[cfg(unix)]
#[test]
fn an_inventory_path_from_argv_is_shown_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let out = greenshift()
        .args(["nextest-filter", "--inventory"])
        .arg(dir.path().join(EVIL))
        .assert()
        .failure()
        .code(1);
    shows_escaped(
        &out.get_output().stderr,
        &format!("inventory {}", dir.path().join(EVIL_SHOWN).display()),
    );
}

#[cfg(unix)]
#[test]
fn an_inventory_that_is_not_a_listing_is_shown_escaped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(EVIL), "not json").unwrap();
    let out = greenshift()
        .args(["nextest-filter", "--inventory"])
        .arg(dir.path().join(EVIL))
        .assert()
        .failure()
        .code(1);
    shows_escaped(
        &out.get_output().stderr,
        &format!(
            "inventory {}: not cargo nextest list JSON",
            dir.path().join(EVIL_SHOWN).display()
        ),
    );
}

// Bytes that are not UTF-8 show as U+FFFD, as Path::display shows them.
#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_is_shown_with_replacement_characters() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join(std::ffi::OsStr::from_bytes(b"bad\xffname.xml"));
    let out = greenshift()
        .arg("ingest")
        .arg("--db")
        .arg(dir.path().join("history.sqlite"))
        .arg(&file)
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("bad\u{fffd}name.xml: could not read file"),
        "{stderr:?}"
    );
}

/// clap's usage errors quote a rejected argument or value from argv; each
/// is shown escaped, whatever the flag, and the error is still clap's.
#[test]
fn a_rejected_argv_value_is_shown_escaped_in_usage_errors() {
    let unknown = format!("--{EVIL}");
    let cases: [(&[&str], String); 8] = [
        (
            &["select", "--estimator", EVIL],
            format!("error: invalid value '{EVIL_SHOWN}' for '--estimator <ESTIMATOR>'"),
        ),
        (
            &["select", "--coverage", EVIL],
            format!("error: invalid value '{EVIL_SHOWN}' for '--coverage <N>'"),
        ),
        (
            &["nextest-filter", "--inventory", "-", "--min-runs", EVIL],
            format!("error: invalid value '{EVIL_SHOWN}' for '--min-runs <N>'"),
        ),
        (
            &["ingest", "--commit", EVIL, "a.xml"],
            format!("error: invalid value '{EVIL_SHOWN}' for '--commit <SHA>'"),
        ),
        (
            &[EVIL],
            format!("error: unrecognized subcommand '{EVIL_SHOWN}'"),
        ),
        (
            &["select", EVIL],
            format!("error: unexpected argument '{EVIL_SHOWN}' found"),
        ),
        (
            &["ingest", "a.xml", &unknown],
            format!("error: unexpected argument '--{EVIL_SHOWN}' found"),
        ),
        // clap's tip quotes the argument too.
        (
            &["ingest", "a.xml", &unknown],
            format!("tip: to pass '--{EVIL_SHOWN}' as a value, use '-- --{EVIL_SHOWN}'"),
        ),
    ];
    for (args, shown) in &cases {
        let out = greenshift()
            .args(*args)
            .env_remove("CLICOLOR_FORCE")
            .assert()
            .failure()
            .code(2);
        let text = String::from_utf8_lossy(&out.get_output().stderr);
        assert!(text.contains(shown), "{args:?}: {text:?}");
        assert!(
            !text.contains(EVIL_TWICE),
            "{args:?}: escaped twice: {text:?}"
        );
        assert!(!text.contains('\u{1b}'), "{args:?}: {text:?}");
        assert!(!text.contains("red\nline"), "{args:?}: {text:?}");
    }
    // In colour, clap's own styling stays and only the value is escaped.
    for (args, _) in &cases {
        let out = greenshift()
            .args(*args)
            .env("CLICOLOR_FORCE", "1")
            .env_remove("NO_COLOR")
            .assert()
            .failure()
            .code(2);
        let text = String::from_utf8_lossy(&out.get_output().stderr);
        assert!(text.contains('\u{1b}'), "{args:?}: not styled: {text:?}");
        assert!(text.contains(EVIL_SHOWN), "{args:?}: {text:?}");
        assert!(
            !text.contains(EVIL_TWICE),
            "{args:?}: escaped twice: {text:?}"
        );
        assert!(!text.contains(EVIL), "{args:?}: {text:?}");
        assert!(!text.contains("red\nline"), "{args:?}: {text:?}");
    }
}
