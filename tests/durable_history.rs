use std::path::Path;
use std::process::Command;

use greenshift::history::{Remote, RestoreOutcome};
use greenshift::store::{NewRun, Store};
use rusqlite::Connection;

#[cfg(unix)]
#[path = "common/history_cas.rs"]
mod history_cas;

fn repository(path: &Path) {
    assert!(Command::new("git")
        .args(["init", "--bare"])
        .arg(path)
        .output()
        .unwrap()
        .status
        .success());
}

fn record(path: &Path, name: &str) {
    let mut store = Store::open(path).unwrap();
    store
        .record_run(
            &NewRun {
                commit: None,
                source: name,
                files: &[],
                partial: false,
                branch: Some("main"),
                tag: None,
            },
            std::iter::empty(),
        )
        .unwrap();
}

fn count(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM runs", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn publish_then_wipe_cache_restores_every_run_without_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let data = dir.path().join("input.sqlite");
    record(&data, "first");
    record(&data, "second");
    let cache = dir.path().join("cache");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let oid = remote
        .publish(&data, Some(&cache))
        .unwrap()
        .oid()
        .to_owned();
    assert_eq!(remote.head().unwrap(), Some(oid));
    record(&data, "third");
    remote.publish(&data, Some(&cache)).unwrap();
    std::fs::remove_dir_all(cache).unwrap();
    let restored = dir.path().join("restored.sqlite");
    assert!(matches!(
        remote.restore(&restored, None).unwrap(),
        RestoreOutcome::Restored(_)
    ));
    assert_eq!(count(&restored), 3);
    // A replacement commit has no parents: one live full snapshot, not a log.
    let parents = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["rev-list", "--parents", "-1", "refs/greenshift/history"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(parents.stdout)
            .unwrap()
            .split_whitespace()
            .count(),
        1
    );
    let count = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["rev-list", "--count", "refs/greenshift/history"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(count.stdout).unwrap().trim(), "1");
}

#[test]
fn repeated_and_stale_full_snapshots_merge_without_loss_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let first = dir.path().join("first.sqlite");
    record(&first, "baseline");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    remote.publish(&first, None).unwrap();
    let a = dir.path().join("a.sqlite");
    let b = dir.path().join("b.sqlite");
    remote.restore(&a, None).unwrap();
    remote.restore(&b, None).unwrap();
    record(&a, "writer-a");
    record(&b, "writer-b");
    remote.publish(&a, None).unwrap();
    remote.publish(&b, None).unwrap();
    remote.publish(&a, None).unwrap();
    let restored = dir.path().join("read.sqlite");
    remote.restore(&restored, None).unwrap();
    assert_eq!(count(&restored), 3);
}

#[test]
fn corrupt_or_substituted_cache_falls_back_to_ref_and_tampered_artifact_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let db = dir.path().join("db.sqlite");
    record(&db, "real");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let cache = dir.path().join("cache");
    let oid = remote.publish(&db, Some(&cache)).unwrap().oid().to_owned();
    std::fs::write(cache.join(&oid).join("history.sqlite.gz"), b"not a gzip").unwrap();
    let restored = dir.path().join("read.sqlite");
    remote.restore(&restored, Some(&cache)).unwrap();
    assert_eq!(count(&restored), 1);
    Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TRIGGER evil AFTER INSERT ON runs BEGIN DELETE FROM results; END;")
        .unwrap();
    assert!(remote.publish(&db, None).is_err());
    assert_eq!(remote.head().unwrap(), Some(oid));
}

#[test]
fn cache_key_and_storage_failure_do_not_fail_cli_and_pr_cannot_publish() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .args(["history", "cache-key", "--remote"])
        .arg(&repo)
        .assert()
        .success()
        .stdout("greenshift-history-empty\n");
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .args(["history", "restore", "--remote", "/nonexistent/history.git"])
        .assert()
        .success()
        .stderr(predicates::str::contains("⚠"));
    let db = dir.path().join("db.sqlite");
    record(&db, "fork");
    for event in ["pull_request", "pull_request_target"] {
        assert_cmd::Command::cargo_bin("greenshift")
            .unwrap()
            .env("GITHUB_EVENT_NAME", event)
            .env_remove("GITHUB_ACTIONS")
            .args(["history", "publish", "--remote"])
            .arg(&repo)
            .arg("--db")
            .arg(&db)
            .assert()
            .code(2)
            .stderr(predicates::str::contains("refused"));
        assert!(Remote::new(repo.to_str().unwrap())
            .unwrap()
            .head()
            .unwrap()
            .is_none());
    }
}

#[test]
fn remote_rejects_helpers_and_embedded_credentials() {
    for (url, message) in [
        ("", "invalid history remote"),
        ("-remote", "invalid history remote"),
        ("path\nnext", "invalid history remote"),
        ("evil::some-command", "unsupported history remote syntax"),
        (
            "ssh://example.invalid/repo",
            "unsupported history remote protocol",
        ),
        (
            "https://secret@example.invalid/repo",
            "history remote must not embed credentials",
        ),
    ] {
        let error = match Remote::new(url) {
            Ok(_) => panic!("unexpected accepted remote"),
            Err(error) => error,
        };
        assert!(error.is_usage());
        assert_eq!(
            error.to_string(),
            format!("history configuration: {message}")
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    assert!(Remote::new(&format!("file://{}", repo.display()))
        .unwrap()
        .head()
        .unwrap()
        .is_none());
}

#[test]
fn simultaneous_publishers_keep_both_new_runs() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    #[cfg(unix)]
    {
        // Hold both receive-pack processes after advertisement, forcing the
        // server-side create-ref CAS rather than relying on incidental overlap.
        let hook = repo.join("hooks/pre-receive");
        std::fs::write(&hook,"#!/bin/sh\nmkdir -p \"$GIT_DIR/race-barrier\"\ntouch \"$GIT_DIR/race-barrier/$$\"\ni=0\nwhile [ \"$(ls \"$GIT_DIR/race-barrier\" | wc -l)\" -lt 2 ]; do\n i=$((i+1)); [ \"$i\" -lt 500 ] || exit 1\n sleep .01\ndone\n").unwrap();
        std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let a = dir.path().join("a.sqlite");
    let b = dir.path().join("b.sqlite");
    record(&a, "a");
    record(&b, "b");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for data in [&a, &b] {
            let repo = &repo;
            let barrier = &barrier;
            scope.spawn(move || {
                let remote = Remote::new(repo.to_str().unwrap()).unwrap();
                barrier.wait();
                remote.publish(data, None).unwrap();
            });
        }
    });
    let result = dir.path().join("read.sqlite");
    Remote::new(repo.to_str().unwrap())
        .unwrap()
        .restore(&result, None)
        .unwrap();
    assert_eq!(count(&result), 2);
}

#[test]
fn restore_refuses_foreign_database_and_live_wal_without_touching_them() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let canonical = dir.path().join("canonical.sqlite");
    record(&canonical, "canonical");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    remote.publish(&canonical, None).unwrap();
    let foreign = dir.path().join("foreign.sqlite");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch(
            "CREATE TABLE important(value TEXT); INSERT INTO important VALUES('preserve');",
        )
        .unwrap();
    let before = std::fs::read(&foreign).unwrap();
    assert!(remote.restore(&foreign, None).is_err());
    assert_eq!(std::fs::read(&foreign).unwrap(), before);
    let live = dir.path().join("live.sqlite");
    record(&live, "existing");
    let connection = Connection::open(&live).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO runs(source,history_id) VALUES('wal-stale',lower(hex(randomblob(16))));").unwrap();
    assert!(remote.restore(&live, None).is_err());
    let sources: i64 = Connection::open(&live)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runs WHERE source IN ('existing','wal-stale')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sources, 2);
}

#[cfg(unix)]
#[test]
fn healthy_authenticated_cache_restores_when_fetch_is_disabled_and_special_cache_falls_back() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let db = dir.path().join("db.sqlite");
    record(&db, "real");
    let cache = dir.path().join("cache");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let oid = remote.publish(&db, Some(&cache)).unwrap().oid().to_owned();
    let wrapper = dir.path().join("bin");
    std::fs::create_dir(&wrapper).unwrap();
    std::fs::write(wrapper.join("git"),"#!/bin/sh\nfor arg in \"$@\"; do if [ \"$arg\" = fetch ]; then exit 99; fi; done\nexec \"$REAL_GIT\" \"$@\"\n").unwrap();
    std::fs::set_permissions(wrapper.join("git"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let restored = dir.path().join("read.sqlite");
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .env(
            "PATH",
            format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap()),
        )
        .env(
            "REAL_GIT",
            String::from_utf8(real_git.stdout).unwrap().trim(),
        )
        .args(["history", "restore", "--remote"])
        .arg(&repo)
        .arg("--db")
        .arg(&restored)
        .arg("--cache-dir")
        .arg(&cache)
        .assert()
        .success()
        .stderr("");
    assert_eq!(count(&restored), 1);
    std::fs::remove_file(&restored).unwrap();
    let commit = cache.join(&oid).join("commit");
    std::fs::remove_file(&commit).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&commit)
        .status()
        .unwrap()
        .success());
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .timeout(std::time::Duration::from_secs(3))
        .args(["history", "restore", "--remote"])
        .arg(&repo)
        .arg("--db")
        .arg(&restored)
        .arg("--cache-dir")
        .arg(&cache)
        .assert()
        .success();
    assert_eq!(count(&restored), 1);
}

#[test]
fn cache_named_after_ref_cannot_substitute_another_valid_database() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let canonical = dir.path().join("canonical.sqlite");
    let other = dir.path().join("other.sqlite");
    record(&canonical, "real");
    record(&other, "substitute");
    let cache = dir.path().join("cache");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let oid = remote
        .publish(&canonical, Some(&cache))
        .unwrap()
        .oid()
        .to_owned();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&std::fs::read(&other).unwrap()).unwrap();
    std::fs::write(
        cache.join(oid).join("history.sqlite.gz"),
        gzip.finish().unwrap(),
    )
    .unwrap();
    let restored = dir.path().join("read.sqlite");
    remote.restore(&restored, Some(&cache)).unwrap();
    let source: String = Connection::open(restored)
        .unwrap()
        .query_row("SELECT source FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(source, "real");
}

#[test]
fn relative_local_remote_resolves_before_scratch_cwd_and_ci_policy_refuses_other_branches() {
    let dir = tempfile::tempdir().unwrap();
    repository(&dir.path().join("remote.git"));
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .current_dir(dir.path())
        .args(["history", "cache-key", "--remote", "remote.git"])
        .assert()
        .success()
        .stdout("greenshift-history-empty\n");
    for branch in [None, Some("main")] {
        let db = dir.path().join("valid.sqlite");
        if !db.exists() {
            record(&db, "actual-valid-artifact");
        }
        let mut command = assert_cmd::Command::cargo_bin("greenshift").unwrap();
        command
            .env("GITHUB_ACTIONS", "true")
            .env("GITHUB_EVENT_NAME", "push")
            .env("GITHUB_REF", "refs/heads/feature")
            .args(["history", "publish", "--remote"])
            .arg(dir.path().join("remote.git"))
            .arg("--db")
            .arg(&db);
        if let Some(branch) = branch {
            command.args(["--default-branch", branch]);
        }
        command
            .assert()
            .code(2)
            .stderr(predicates::str::contains(if branch.is_some() {
                "only default-branch pushes"
            } else {
                "requires --default-branch"
            }));
    }
    assert!(Remote::new(dir.path().join("remote.git").to_str().unwrap())
        .unwrap()
        .head()
        .unwrap()
        .is_none());
    let output = assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_EVENT_NAME", "push")
        .env("GITHUB_REF", "refs/heads/main")
        .args(["history", "publish", "--remote"])
        .arg(dir.path().join("remote.git"))
        .arg("--db")
        .arg(dir.path().join("valid.sqlite"))
        .args(["--default-branch", "main"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let printed = String::from_utf8(output).unwrap();
    assert_eq!(
        Remote::new(dir.path().join("remote.git").to_str().unwrap())
            .unwrap()
            .head()
            .unwrap()
            .as_deref(),
        Some(printed.trim())
    );
}

#[test]
fn token_on_plaintext_http_is_refused_before_any_network_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .env("GREENSHIFT_HISTORY_TOKEN", "invented-test-token")
        .args([
            "history",
            "cache-key",
            "--remote",
            &format!("http://{}/repo.git", listener.local_addr().unwrap()),
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("requires HTTPS"));
    assert!(matches!(listener.accept(),Err(err) if err.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn exact_history_ref_ignores_suffix_matching_branch_and_restore_keeps_local_runs() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let canonical = dir.path().join("canonical");
    record(&canonical, "canonical");
    let local = dir.path().join("local");
    record(&local, "unsaved-local");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let oid = remote.publish(&canonical, None).unwrap().oid().to_owned();
    assert!(Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["update-ref", "refs/heads/refs/greenshift/history", &oid])
        .status()
        .unwrap()
        .success());
    assert_eq!(remote.head().unwrap(), Some(oid.clone()));
    assert!(matches!(
        remote.restore(&local, None).unwrap(),
        RestoreOutcome::Restored(_)
    ));
    assert_eq!(count(&local), 2);
    assert!(matches!(
        remote.restore(&local, None).unwrap(),
        RestoreOutcome::Restored(_)
    ));
    assert_eq!(count(&local), 2);
    assert_eq!(remote.head().unwrap(), Some(oid));
}

#[test]
fn restore_updates_the_existing_sqlite_file_without_retiring_open_connections() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let canonical = dir.path().join("canonical");
    let local = dir.path().join("local");
    record(&canonical, "canonical");
    record(&local, "local-before");
    let observer = Connection::open(&local).unwrap();
    let observed_count = || {
        observer
            .query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(observed_count(), 1);
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    remote.publish(&canonical, None).unwrap();
    assert!(matches!(
        remote.restore(&local, None).unwrap(),
        RestoreOutcome::Restored(_)
    ));

    // An idle connection must observe the same committed union as a newly
    // opened connection, rather than remain attached to an unlinked old DB.
    assert_eq!(observed_count(), 2);
    assert_eq!(count(&local), 2);
    record(&local, "local-after");
    assert_eq!(observed_count(), 3);
    observer
        .execute(
            "INSERT INTO runs(source,history_id) VALUES ('observer-after',?1)",
            ["eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"],
        )
        .unwrap();
    assert_eq!(count(&local), 4);
    assert_eq!(observed_count(), 4);
}

#[test]
fn landed_push_keeps_success_oid_when_local_cache_cannot_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let db = dir.path().join("db");
    record(&db, "canonical");
    let cache = dir.path().join("not-a-directory");
    std::fs::write(&cache, b"preserve").unwrap();
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    let oid = remote.publish(&db, Some(&cache)).unwrap().oid().to_owned();
    assert_eq!(remote.head().unwrap(), Some(oid));
    assert_eq!(std::fs::read(cache).unwrap(), b"preserve");
}

#[cfg(unix)]
#[test]
fn forced_lease_collision_retries_once_and_keeps_all_run_identities() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let other_repo = dir.path().join("other.git");
    repository(&other_repo);
    let baseline = dir.path().join("baseline");
    record(&baseline, "baseline");
    let remote = Remote::new(repo.to_str().unwrap()).unwrap();
    remote.publish(&baseline, None).unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    remote.restore(&a, None).unwrap();
    remote.restore(&b, None).unwrap();
    record(&a, "writer-a");
    record(&b, "writer-b");
    let competing = Remote::new(other_repo.to_str().unwrap())
        .unwrap()
        .publish(&b, None)
        .unwrap();
    let (mut command, log) =
        history_cas::command(dir.path(), &repo, &other_repo, competing.oid(), &a);
    let output = command
        .assert()
        .success()
        .stderr("")
        .get_output()
        .stdout
        .clone();
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
    assert_eq!(
        remote.head().unwrap().as_deref(),
        Some(String::from_utf8(output).unwrap().trim())
    );
    let restored = dir.path().join("restored");
    remote.restore(&restored, None).unwrap();
    let conn = Connection::open(restored).unwrap();
    let identities: i64 = conn
        .query_row("SELECT count(DISTINCT history_id) FROM runs", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(identities, 3);
    let commits = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["rev-list", "--count", "refs/greenshift/history"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(commits.stdout).unwrap().trim(), "1");
}

#[cfg(unix)]
#[test]
fn ordinary_push_rejection_does_not_retry() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote.git");
    repository(&repo);
    let db = dir.path().join("db");
    record(&db, "canonical");
    let hook = repo.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho reject >> \"$REJECTION_LOG\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = dir.path().join("rejected");
    assert_cmd::Command::cargo_bin("greenshift")
        .unwrap()
        .env("REJECTION_LOG", &log)
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_ACTIONS")
        .args(["history", "publish", "--remote"])
        .arg(&repo)
        .arg("--db")
        .arg(&db)
        .assert()
        .success()
        .stdout("")
        .stderr(predicates::str::contains("git operation failed"));
    assert_eq!(std::fs::read_to_string(log).unwrap().lines().count(), 1);
}

#[cfg(unix)]
#[test]
fn inherited_desktop_askpass_is_not_executed_on_authentication_failure() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("prompted");
    let askpass = dir.path().join("askpass");
    std::fs::write(
        &askpass,
        "#!/bin/sh\necho invoked >> \"$PROMPT_MARKER\"\necho fixture\n",
    )
    .unwrap();
    std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/private.git", listener.local_addr().unwrap());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline=Instant::now()+Duration::from_secs(3);
            loop { match listener.accept() {
                Ok((mut stream,_))=> { stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(); break; },
                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=> { assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10)); },
                Err(error)=>panic!("fixture accept: {error}"),
            }}
        });
        assert_cmd::Command::cargo_bin("greenshift")
            .unwrap()
            .env("SSH_ASKPASS", &askpass)
            .env("PROMPT_MARKER", &marker)
            .env("NO_PROXY", "127.0.0.1")
            .env_remove("GREENSHIFT_HISTORY_TOKEN")
            .args(["history", "cache-key", "--remote", &url])
            .assert()
            .success()
            .stdout("")
            .stderr(predicates::str::contains("git operation failed"));
    });
    assert!(!marker.exists());
}

#[test]
fn sha256_canonical_identity_is_rejected_explicitly_instead_of_misinterpreted() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("sha256.git");
    assert!(Command::new("git")
        .args(["init", "--bare", "--object-format=sha256"])
        .arg(&repo)
        .output()
        .unwrap()
        .status
        .success());
    let mut tree = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    tree.stdin.take().unwrap().write_all(b"").unwrap();
    let tree = tree.wait_with_output().unwrap();
    assert!(tree.status.success());
    let commit = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args([
            "commit-tree",
            String::from_utf8(tree.stdout).unwrap().trim(),
            "-m",
            "fixture",
        ])
        .output()
        .unwrap();
    assert!(commit.status.success());
    let commit = String::from_utf8(commit.stdout).unwrap();
    assert!(Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["update-ref", "refs/greenshift/history", commit.trim()])
        .status()
        .unwrap()
        .success());
    assert_eq!(
        Remote::new(repo.to_str().unwrap())
            .unwrap()
            .head()
            .unwrap_err()
            .to_string(),
        "history: SHA-256 history remotes are not supported"
    );
}
