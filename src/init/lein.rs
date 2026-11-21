//! `greenshift init lein`: the `:greenshift` test selector merged into a
//! Leiningen project's `project.clj` ([`crate::lein::merge`]), which
//! `greenshift run lein` runs each group with, and with `--ci`, a GitHub
//! workflow of its own that runs `greenshift run lein` with durable
//! history. Nothing else: the JUnit reporter comes with each run, so
//! there is no reporter in the repository, no profile and nothing in
//! `~/.lein`.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Action, Release, RunnerLabel, Sha256, Want};
use crate::lein::{self, Merge};
use crate::tainted::Tainted;

/// The Leiningen version the workflow installs (2.13.0's launcher
/// self-installs a snapshot jar that was never published).
const LEIN_VERSION: &str = "2.12.0";

/// The project file `init lein` edits.
pub const PROJECT_CLJ: &str = "project.clj";

/// A JDK version for `actions/setup-java`: a major version such as `25`,
/// or a fuller one such as `21.0.5`, so that it goes into YAML as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaVersion(String);

impl JavaVersion {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JavaVersion {
    fn default() -> JavaVersion {
        JavaVersion("25".to_owned())
    }
}

/// Why a `--java-version` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected a JDK version such as 25 or 21.0.5: digits and dots")]
pub struct JavaVersionError;

impl std::str::FromStr for JavaVersion {
    type Err = JavaVersionError;

    fn from_str(text: &str) -> Result<JavaVersion, JavaVersionError> {
        let ok = !text.is_empty()
            && text.len() <= 32
            && text.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && !text.starts_with('.')
            && !text.ends_with('.');
        if ok {
            Ok(JavaVersion(text.to_owned()))
        } else {
            Err(JavaVersionError)
        }
    }
}

/// What the workflow is made from.
#[derive(Debug, Clone)]
pub struct Settings<'a> {
    pub release: &'a Release,
    pub sha256: &'a Sha256,
    pub runs_on: &'a RunnerLabel,
    pub java: &'a JavaVersion,
}

/// Where the workflow goes.
pub fn workflow_path() -> PathBuf {
    Path::new(".github/workflows/greenshift.yml").to_owned()
}

/// The workflow `init lein --ci` wants.
pub fn wants(settings: &Settings<'_>) -> Vec<Want> {
    vec![Want {
        path: workflow_path(),
        content: workflow(settings),
    }]
}

/// The GitHub workflow for `settings`.
pub fn workflow(settings: &Settings<'_>) -> String {
    let install = super::nextest::indent(include_str!("nextest/install.sh"), 10);
    include_str!("lein/github.yml")
        .replace("@INSTALL@\n", &install)
        .replace("@RELEASE@", settings.release.as_str())
        .replace("@SHA256@", settings.sha256.as_str())
        .replace("@RUNS_ON@", settings.runs_on.as_str())
        .replace("@JAVA@", settings.java.as_str())
        .replace("@LEIN@", LEIN_VERSION)
}

/// What to do with `project.clj` under `root`: add the selector, nothing
/// when it is there already, or a conflict when a different one is,
/// unless `force`. A missing, unreadable or symlinked file, or a computed
/// `:test-selectors`, is an error, and then nothing is written.
pub fn project_action(root: &Path, force: bool) -> anyhow::Result<Action> {
    let path = PathBuf::from(PROJECT_CLJ);
    let Some(bytes) = super::read_existing(root, &path)? else {
        anyhow::bail!(
            "no project.clj here; run `greenshift init lein` in the root of a Leiningen \
             project. Nothing written"
        );
    };
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not UTF-8; nothing written", Tainted::from(&*path)))?;
    let merged = lein::merge(&text, force).context("nothing written")?;
    Ok(match merged {
        Merge::Unchanged => Action::Unchanged { path },
        Merge::Changed(content) => Action::Edit {
            path,
            content,
            what: "the :greenshift test selector".to_owned(),
        },
        Merge::Conflict => Action::Conflict {
            path,
            why: Some(
                "project.clj has a :greenshift test selector that differs from what init \
                 writes; left alone (--force replaces it)"
                    .to_owned(),
            ),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_placeholder_is_filled() {
        let release: Release = "v0.3.0".parse().unwrap();
        let sha256: Sha256 = "a".repeat(64).parse().unwrap();
        let runs_on = super::super::Forge::Github.default_runner();
        let java = JavaVersion::default();
        let text = workflow(&Settings {
            release: &release,
            sha256: &sha256,
            runs_on: &runs_on,
            java: &java,
        });
        for name in ["INSTALL", "RELEASE", "SHA256", "RUNS_ON", "JAVA", "LEIN"] {
            assert!(!text.contains(&format!("@{name}@")), "{name}");
        }
    }

    #[test]
    fn java_versions_are_digits_and_dots() {
        for ok in ["25", "21", "21.0.5", "8"] {
            assert_eq!(ok.parse::<JavaVersion>().unwrap().as_str(), ok);
        }
        for bad in ["", "v25", "25-ea", ".25", "25.", "2 5", "'25'"] {
            assert!(bad.parse::<JavaVersion>().is_err(), "{bad:?}");
        }
    }
}
