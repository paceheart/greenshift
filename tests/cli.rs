use assert_cmd::Command;
use predicates::prelude::*;

fn greenshift() -> Command {
    Command::cargo_bin("greenshift").expect("binary builds")
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
fn help_describes_the_tool() {
    greenshift()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: greenshift"));
}
