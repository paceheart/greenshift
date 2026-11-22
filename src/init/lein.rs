//! `greenshift init lein`: the `:greenshift` test selector merged into a
//! Leiningen project's `project.clj` ([`crate::lein::merge`]), which
//! `greenshift run lein` runs each group with, and with `--ci`, a GitHub
//! or Forgejo workflow of its own that runs `greenshift run lein` with
//! durable history, in the shape [`super::nextest`] writes for that
//! forge. Nothing else: the JUnit reporter comes with each run, so there
//! is no reporter in the repository, no profile and nothing in `~/.lein`.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{Action, Forge, Release, RunnerLabel, Sha256, Want};
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

/// Where the Leiningen project is, relative to the repository root, as
/// `--project-dir` gives it: `/`-separated parts of letters, digits and
/// `.`, `_`, `-`, so that it goes into YAML and a `hashFiles` pattern as
/// it is. `.` parts, empty ones and a trailing `/` are dropped, so
/// `./vaelii/` is `vaelii`, and `.` is the root itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectDir(String);

impl ProjectDir {
    /// The directory as the workflow names it, with no trailing `/`;
    /// empty for the root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the repository root itself.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// `name` in the directory, relative to the repository root.
    pub fn join(&self, name: &str) -> String {
        if self.is_root() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.0)
        }
    }
}

/// Why a `--project-dir` was refused. No message repeats the value: clap
/// names it already.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProjectDirError {
    #[error("expected a directory relative to the repository root, such as upstream/app")]
    Empty,
    #[error("expected a directory relative to the repository root, not an absolute path")]
    Absolute,
    #[error("expected a directory inside the repository: no `..` parts")]
    Parent,
    #[error(
        "expected a directory of at most 1024 letters, digits, '.', '_', '-' and '/' separators"
    )]
    Chars,
}

impl std::str::FromStr for ProjectDir {
    type Err = ProjectDirError;

    fn from_str(text: &str) -> Result<ProjectDir, ProjectDirError> {
        if text.is_empty() {
            return Err(ProjectDirError::Empty);
        }
        if text.starts_with('/') {
            return Err(ProjectDirError::Absolute);
        }
        let parts: Vec<&str> = text
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        if parts.contains(&"..") {
            return Err(ProjectDirError::Parent);
        }
        let ok = text.len() <= 1024
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
        if !ok {
            return Err(ProjectDirError::Chars);
        }
        Ok(ProjectDir(parts.join("/")))
    }
}

/// A shell command for `--setup`, which the workflow runs in bash, in
/// the repository root, as a step of its own before the tests: kept as
/// given, but for leading and trailing whitespace, and with nothing a
/// YAML block scalar cannot hold as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCommand(String);

impl SetupCommand {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a `--setup` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SetupCommandError {
    #[error("expected a shell command, not an empty one")]
    Empty,
    #[error(
        "expected a shell command of text, newlines and tabs: no carriage returns or \
         other control characters"
    )]
    Control,
}

impl std::str::FromStr for SetupCommand {
    type Err = SetupCommandError;

    fn from_str(text: &str) -> Result<SetupCommand, SetupCommandError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SetupCommandError::Empty);
        }
        // YAML takes U+2028, U+2029 and U+0085 (a control) as line breaks,
        // and U+FEFF as a byte order mark.
        let bad = |c: char| {
            (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{feff}')
        };
        if text.chars().any(bad) {
            return Err(SetupCommandError::Control);
        }
        Ok(SetupCommand(text.to_owned()))
    }
}

/// What the workflow is made from.
#[derive(Debug, Clone)]
pub struct Settings<'a> {
    pub forge: Forge,
    pub release: &'a Release,
    pub sha256: &'a Sha256,
    pub runs_on: &'a RunnerLabel,
    pub java: &'a JavaVersion,
    /// Where the project is: the tests run there.
    pub project: &'a ProjectDir,
    /// What to run before the tests, if anything.
    pub setup: Option<&'a SetupCommand>,
}

/// Where the workflow goes for `forge`: where `init nextest` writes its.
pub fn workflow_path(forge: Forge) -> PathBuf {
    forge.workflow_path()
}

/// The workflow `init lein --ci` wants.
pub fn wants(settings: &Settings<'_>) -> Vec<Want> {
    vec![Want {
        path: workflow_path(settings.forge),
        content: workflow(settings),
    }]
}

/// The CI workflow for `settings`.
pub fn workflow(settings: &Settings<'_>) -> String {
    let template = match settings.forge {
        Forge::Github => include_str!("lein/github.yml"),
        Forge::Forgejo => include_str!("lein/forgejo.yml"),
    };
    let install = super::nextest::indent(include_str!("nextest/install.sh"), 10);
    let project = settings.project;
    let (working_directory, history_files) = if project.is_root() {
        (
            String::new(),
            "            --history-cache-dir .greenshift/cache \
             --history-artifact .greenshift/publish.sqlite \\\n",
        )
    } else {
        (
            format!("        working-directory: '{}'\n", project.as_str()),
            // greenshift's own files stay in the repository root, where
            // the cache and artifact steps and the publisher find them.
            concat!(
                "            --history-cache-dir \"$GITHUB_WORKSPACE/.greenshift/cache\" \\\n",
                "            --history-artifact \"$GITHUB_WORKSPACE/.greenshift/publish.sqlite\" \\\n",
            ),
        )
    };
    let again = match (settings.setup.is_some(), !project.is_root()) {
        (false, false) => "",
        (false, true) => {
            "# Give it the same --project-dir too, or the new workflow goes without it.\n"
        }
        (true, false) => "# Give it the same --setup too, or the new workflow goes without it.\n",
        (true, true) => {
            "# Give it the same --setup and --project-dir too, or the new workflow goes\n\
             # without them.\n"
        }
    };
    let setup = settings.setup.map_or_else(String::new, |setup| {
        format!(
            "      # From --setup: run in the repository root, before the tests.\n      \
             - name: Set up the project\n        shell: bash\n        run: |\n{}",
            super::nextest::indent(setup.as_str(), 10)
        )
    });
    super::forgejo_paragraphs(template)
        .replace("@AGAIN@\n", again)
        .replace("@WORKING_DIRECTORY@\n", &working_directory)
        .replace("@HISTORY_FILES@\n", history_files)
        .replace("@PROJECT_CLJ@", &project.join(PROJECT_CLJ))
        .replace("@INSTALL@\n", &install)
        .replace("@RELEASE@", settings.release.as_str())
        .replace("@SHA256@", settings.sha256.as_str())
        .replace("@RUNS_ON@", settings.runs_on.as_str())
        .replace("@JAVA@", settings.java.as_str())
        .replace("@LEIN@", LEIN_VERSION)
        // Last, so that nothing in the command is taken for a placeholder.
        .replace("@SETUP@\n", &setup)
}

/// What to do with `project.clj` in `project` under `root`: add the
/// selector, nothing when it is there already, or a conflict when a
/// different one is, unless `force`. A missing file is nothing to do
/// (`None`) when the project is `assembled` in CI, in a directory of its
/// own, and otherwise an error, as is an unreadable or symlinked file,
/// one reached through a symlinked directory, or a computed
/// `:test-selectors`; and then nothing is written.
pub fn project_action(
    root: &Path,
    project: &ProjectDir,
    assembled: bool,
    force: bool,
) -> anyhow::Result<Option<Action>> {
    let path = PathBuf::from(project.join(PROJECT_CLJ));
    let Some(bytes) = super::read_existing(root, &path)? else {
        if assembled && !project.is_root() {
            return Ok(None);
        }
        if project.is_root() {
            anyhow::bail!(
                "no project.clj here; run `greenshift init lein` in the root of a Leiningen \
                 project. Nothing written"
            );
        }
        anyhow::bail!(
            "no {} here; give --project-dir the Leiningen project's directory, relative to \
             the repository root, and run `greenshift init lein` in that root. Nothing written",
            Tainted::from(&*path)
        );
    };
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not UTF-8; nothing written", Tainted::from(&*path)))?;
    let merged = lein::merge(&text, force).context("nothing written")?;
    Ok(Some(match merged {
        Merge::Unchanged => Action::Unchanged { path },
        Merge::Changed(content) => Action::Edit {
            path,
            content,
            what: "the :greenshift test selector".to_owned(),
        },
        Merge::Conflict => Action::Conflict {
            why: Some(format!(
                "{} has a :greenshift test selector that differs from what init writes; \
                 left alone (--force replaces it)",
                Tainted::from(&*path)
            )),
            path,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_placeholder_is_filled() {
        let release: Release = "v0.3.0".parse().unwrap();
        let sha256: Sha256 = "a".repeat(64).parse().unwrap();
        let java = JavaVersion::default();
        let nested: ProjectDir = "upstream/app".parse().unwrap();
        let setup: SetupCommand = "./assemble".parse().unwrap();
        for (forge, project) in [Forge::Github, Forge::Forgejo]
            .into_iter()
            .flat_map(|forge| [(forge, ProjectDir::default()), (forge, nested.clone())])
        {
            let project = &project;
            let runs_on = forge.default_runner();
            let text = workflow(&Settings {
                forge,
                release: &release,
                sha256: &sha256,
                runs_on: &runs_on,
                java: &java,
                project,
                setup: Some(&setup),
            });
            // Any `@NAME@` left, whether or not `workflow` knows the name.
            let parts: Vec<&str> = text.split('@').collect();
            // Every piece with an `@` on both sides; `split` yields at
            // least one piece, so `len() - 1` cannot underflow.
            let between = parts.get(1..parts.len() - 1).unwrap_or(&[]);
            let left: Vec<&&str> = between
                .iter()
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                })
                .collect();
            assert!(left.is_empty(), "{forge:?}: {left:?}");
        }
    }

    #[test]
    fn project_dirs_are_relative_and_inside_the_repository() {
        for (given, kept) in [
            ("vaelii", "vaelii"),
            ("upstream/app", "upstream/app"),
            ("./vaelii/", "vaelii"),
            ("a//b/./c", "a/b/c"),
            (".hidden/x-1_2", ".hidden/x-1_2"),
            (".", ""),
            ("./", ""),
        ] {
            let dir: ProjectDir = given.parse().unwrap();
            assert_eq!(dir.as_str(), kept, "{given:?}");
            assert_eq!(dir.is_root(), kept.is_empty(), "{given:?}");
        }
        assert_eq!(
            "vaelii".parse::<ProjectDir>().unwrap().join("project.clj"),
            "vaelii/project.clj"
        );
        assert_eq!(ProjectDir::default().join("project.clj"), "project.clj");
        for (bad, why) in [
            ("", ProjectDirError::Empty),
            ("/vaelii", ProjectDirError::Absolute),
            ("/", ProjectDirError::Absolute),
            ("..", ProjectDirError::Parent),
            ("../vaelii", ProjectDirError::Parent),
            ("a/../b", ProjectDirError::Parent),
            ("a/..", ProjectDirError::Parent),
            ("a b", ProjectDirError::Chars),
            ("a\\b", ProjectDirError::Chars),
            ("'a'", ProjectDirError::Chars),
            ("a*", ProjectDirError::Chars),
            ("a\nb", ProjectDirError::Chars),
            ("é", ProjectDirError::Chars),
        ] {
            assert_eq!(bad.parse::<ProjectDir>(), Err(why), "{bad:?}");
        }
        assert!("a".repeat(1024).parse::<ProjectDir>().is_ok());
        assert_eq!(
            "a".repeat(1025).parse::<ProjectDir>(),
            Err(ProjectDirError::Chars)
        );
    }

    #[test]
    fn setup_commands_are_kept_as_given_but_for_outer_whitespace() {
        for (given, kept) in [
            ("./overlay.sh", "./overlay.sh"),
            ("  make\n\n", "make"),
            (
                "a &&\n\tb # c: 'd' \"e\" ${{ f }}",
                "a &&\n\tb # c: 'd' \"e\" ${{ f }}",
            ),
        ] {
            assert_eq!(given.parse::<SetupCommand>().unwrap().as_str(), kept);
        }
        for bad in ["", " ", "\n\t "] {
            assert_eq!(
                bad.parse::<SetupCommand>(),
                Err(SetupCommandError::Empty),
                "{bad:?}"
            );
        }
        for bad in [
            "a\r\nb",
            "a\u{0}b",
            "a\u{1b}b",
            "a\u{85}b",
            "a\u{2028}b",
            "\u{feff}a",
        ] {
            assert_eq!(
                bad.parse::<SetupCommand>(),
                Err(SetupCommandError::Control),
                "{bad:?}"
            );
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
