//! Shared local Git lease-collision fixture for transport integration tests.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Inject a competing local snapshot before the CLI's first push, then forward
/// both normal pushes to the actual Git executable. No global environment is changed.
pub fn command(
    directory: &Path,
    target_repo: &Path,
    competing_repo: &Path,
    competing_oid: &str,
    db: &Path,
) -> (assert_cmd::Command, PathBuf) {
    let wrapper = directory.join("bin");
    std::fs::create_dir(&wrapper).unwrap();
    std::fs::write(
        wrapper.join("git"),
        r#"#!/bin/sh
for arg in "$@"; do
 if [ "$arg" = push ]; then
  echo push >> "$PUSH_LOG"
  if mkdir "$ONCE" 2>/dev/null; then
   "$REAL_GIT" --git-dir="$COMPETING_REPO" push --force "$TARGET_REPO" "$COMPETING_OID:refs/greenshift/history" >/dev/null 2>&1 || exit 98
  fi
 fi
done
exec "$REAL_GIT" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(wrapper.join("git"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(
        real.status.success(),
        "resolve actual Git for local CAS fixture"
    );
    let real_git = String::from_utf8(real.stdout).unwrap();
    assert!(!real_git.trim().is_empty());
    let log = directory.join("push.log");
    let mut command = assert_cmd::Command::cargo_bin("greenshift").unwrap();
    command
        .env(
            "PATH",
            format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap()),
        )
        .env("REAL_GIT", real_git.trim())
        .env("PUSH_LOG", &log)
        .env("ONCE", directory.join("once"))
        .env("COMPETING_REPO", competing_repo)
        .env("TARGET_REPO", target_repo)
        .env("COMPETING_OID", competing_oid)
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GREENSHIFT_HISTORY_TOKEN")
        .args(["history", "publish", "--remote"])
        .arg(target_repo)
        .arg("--db")
        .arg(db);
    (command, log)
}
