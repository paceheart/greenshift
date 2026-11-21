//! `greenshift init`: set a repository up to run its tests with greenshift
//! in CI, once, with output meant to be committed as is.
//!
//! A backend ([`nextest`] or [`lein`]) says which files it wants and with
//! what content ([`Want`]), or for a file it edits, what to do with it
//! ([`Action::Edit`]); this module decides what to do with each
//! against what is already on disk ([`decide`]), and then writes them: a
//! file that exists and differs is left alone unless `--force` is given,
//! and while one is left alone, none is written. A path through a symlink
//! that is there when `init` looks is never read or written. Every file is
//! staged beside its place before any is renamed into it, so none is left
//! half written and one that cannot be staged stops all of them; renaming
//! is not undone, so were a later rename to fail, earlier ones would stay.
//! A second run with the same inputs finds every file as it would write it
//! and changes nothing.
//!
//! What gets pinned is the greenshift release the workflow downloads,
//! by tag and SHA-256 ([`Release`], [`Sha256`]). Neither has a default: a
//! release is the repository owner's to make and its checksum is to be
//! taken from it, never made up here.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::ValueEnum;

use crate::tainted::Tainted;

pub mod lein;
pub mod nextest;

/// The last published release without `run nextest` and durable history:
/// a workflow pinned to it, or to anything older, could not work.
pub const LAST_RELEASE_WITHOUT_HISTORY: (u64, u64, u64) = (0, 1, 0);

/// The forges `init` writes a CI workflow for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Forge {
    /// GitHub Actions: a read-only test job and a separate publisher.
    Github,
    /// Forgejo Actions. Not isolated: the workflow says so at its top.
    Forgejo,
}

impl Forge {
    /// The forge's directory in a repository, whose presence says the
    /// repository is on it.
    pub fn dir(self) -> &'static str {
        match self {
            Forge::Github => ".github",
            Forge::Forgejo => ".forgejo",
        }
    }

    /// The runner label jobs use unless `--runs-on` is given: GitHub's
    /// hosted Ubuntu runner, and the label a Forgejo runner registers by
    /// default.
    pub fn default_runner(self) -> RunnerLabel {
        RunnerLabel(
            match self {
                Forge::Github => "ubuntu-latest",
                Forge::Forgejo => "docker",
            }
            .to_owned(),
        )
    }
}

impl fmt::Display for Forge {
    /// The name `--ci` takes for it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self
            .to_possible_value()
            .expect("no forge is skipped from --ci");
        f.write_str(value.get_name())
    }
}

/// Why [`detect_forge`] could not tell which forge a repository uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DetectForgeError {
    #[error(
        "found neither .github nor .forgejo; say which forge with --ci github or --ci forgejo"
    )]
    Neither,
    #[error("found both .github and .forgejo; say which forge with --ci github or --ci forgejo")]
    Both,
}

/// The forge whose directory ([`Forge::dir`]) `root` has, when exactly
/// one does.
pub fn detect_forge(root: &Path) -> Result<Forge, DetectForgeError> {
    let found: Vec<Forge> = [Forge::Github, Forge::Forgejo]
        .into_iter()
        .filter(|forge| root.join(forge.dir()).is_dir())
        .collect();
    match found[..] {
        [forge] => Ok(forge),
        [] => Err(DetectForgeError::Neither),
        _ => Err(DetectForgeError::Both),
    }
}

/// The directory of workflows that Forgejo runs for `root` now and stops
/// running once `.forgejo/workflows` is there, if it has any. Forgejo runs
/// the workflows of only the first of `.forgejo/workflows`,
/// `.gitea/workflows` and `.github/workflows` that a commit has.
pub fn forgejo_would_hide(root: &Path) -> Option<&'static str> {
    if root.join(".forgejo/workflows").exists() {
        return None;
    }
    let now = [".gitea/workflows", ".github/workflows"]
        .into_iter()
        .find(|dir| root.join(dir).exists())?;
    let has_any =
        std::fs::read_dir(root.join(now)).is_ok_and(|mut entries| entries.next().is_some());
    has_any.then_some(now)
}

/// A greenshift release tag, `vMAJOR.MINOR.PATCH`, as the release
/// workflow publishes and the generated workflow checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    tag: String,
}

impl Release {
    pub fn as_str(&self) -> &str {
        &self.tag
    }
}

/// Why a `--release` was refused. Neither message repeats the value: clap
/// names it already.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseError {
    #[error("expected a release tag such as v1.2.3")]
    Shape,
    #[error(
        "too old: v{}.{}.{} and earlier have neither `run nextest` nor durable \
         history; pin a later release",
        LAST_RELEASE_WITHOUT_HISTORY.0,
        LAST_RELEASE_WITHOUT_HISTORY.1,
        LAST_RELEASE_WITHOUT_HISTORY.2
    )]
    TooOld,
}

impl std::str::FromStr for Release {
    type Err = ReleaseError;

    /// `vMAJOR.MINOR.PATCH`, each a decimal number with no leading zero,
    /// newer than [`LAST_RELEASE_WITHOUT_HISTORY`]. Pre-releases are
    /// refused, as the generated workflow refuses them.
    fn from_str(text: &str) -> Result<Release, ReleaseError> {
        let numbers = text.strip_prefix('v').ok_or(ReleaseError::Shape)?;
        let parts: Vec<&str> = numbers.split('.').collect();
        let [major, minor, patch] = parts[..] else {
            return Err(ReleaseError::Shape);
        };
        let number = |part: &str| -> Result<u64, ReleaseError> {
            let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            if !digits || (part.len() > 1 && part.starts_with('0')) {
                return Err(ReleaseError::Shape);
            }
            part.parse().map_err(|_| ReleaseError::Shape)
        };
        let version = (number(major)?, number(minor)?, number(patch)?);
        if version <= LAST_RELEASE_WITHOUT_HISTORY {
            return Err(ReleaseError::TooOld);
        }
        Ok(Release {
            tag: text.to_owned(),
        })
    }
}

/// A SHA-256 checksum: 64 hex digits, kept in lower case, as
/// `sha256sum` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256(String);

impl Sha256 {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a `--sha256` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "expected a SHA-256 checksum: 64 hex digits, the first field of the release's .sha256 file"
)]
pub struct Sha256Error;

impl std::str::FromStr for Sha256 {
    type Err = Sha256Error;

    fn from_str(text: &str) -> Result<Sha256, Sha256Error> {
        if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Sha256(text.to_ascii_lowercase()))
        } else {
            Err(Sha256Error)
        }
    }
}

/// A runner label for `runs-on`: letters, digits and `.`, `_`, `-`, `:`,
/// so that it goes into a single-quoted YAML string as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerLabel(String);

impl RunnerLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a `--runs-on` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected a runner label of at most 256 letters, digits, '.', '_', '-' and ':'")]
pub struct RunnerLabelError;

impl std::str::FromStr for RunnerLabel {
    type Err = RunnerLabelError;

    fn from_str(text: &str) -> Result<RunnerLabel, RunnerLabelError> {
        let ok = !text.is_empty()
            && text.len() <= 256
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b));
        if ok {
            Ok(RunnerLabel(text.to_owned()))
        } else {
            Err(RunnerLabelError)
        }
    }
}

/// A file a backend wants, relative to the repository root, with exactly
/// this content: written if missing; if present and different, replaced
/// only with `--force`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub path: PathBuf,
    pub content: String,
}

/// What `init` does with one wanted file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write `content`, in place of a file that is there when `replaces`.
    Write {
        path: PathBuf,
        content: String,
        replaces: bool,
    },
    /// Replace the file that is there with `content`, an edit of it that
    /// adds `what`.
    Edit {
        path: PathBuf,
        content: String,
        what: String,
    },
    /// Nothing to do: the file is already as wanted.
    Unchanged { path: PathBuf },
    /// The file exists and differs, and `--force` was not given; `why`
    /// says so in place of the general message, when given.
    Conflict { path: PathBuf, why: Option<String> },
}

/// What to do with each of `wants` under `root`, given `force`.
pub fn decide(root: &Path, wants: Vec<Want>, force: bool) -> anyhow::Result<Vec<Action>> {
    wants
        .into_iter()
        .map(|Want { path, content }| {
            let existing = read_existing(root, &path)?;
            Ok(match existing {
                None => Action::Write {
                    path,
                    content,
                    replaces: false,
                },
                Some(bytes) if bytes == content.as_bytes() => Action::Unchanged { path },
                Some(_) if force => Action::Write {
                    path,
                    content,
                    replaces: true,
                },
                Some(_) => Action::Conflict { path, why: None },
            })
        })
        .collect()
}

/// The bytes of the file at `path` under `root`, or `None` if there is
/// none. A symlink at `path`, or on the way to it from `root`, is an
/// error: `init` neither reads what it points to into the repository nor
/// replaces it.
pub fn read_existing(root: &Path, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let could_not_read = || format!("could not read {}", Tainted::from(path));
    let mut at = root.to_owned();
    for part in path.components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                "{} is a symlink; left alone, and nothing written",
                Tainted::from(at.strip_prefix(root).unwrap_or(&at))
            ),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => return Err(err).with_context(could_not_read),
        }
    }
    match std::fs::read(root.join(path)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(could_not_read),
    }
}

/// A wanted file ready to put in place, or already there.
enum Ready<'a> {
    Write {
        path: &'a Path,
        replaces: bool,
        /// What an edit adds, for [`Action::Edit`].
        adds: Option<&'a str>,
        file: tempfile::NamedTempFile,
    },
    Unchanged {
        path: &'a Path,
    },
}

/// Carry out `actions` under `root`, saying what happened to each on
/// stdout, and return the exit status: success, or 1 with nothing written
/// when any is a [`Action::Conflict`], each named on stderr.
pub fn apply(root: &Path, actions: &[Action]) -> anyhow::Result<ExitCode> {
    let conflicts: Vec<(&Path, &Option<String>)> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Conflict { path, why } => Some((path.as_path(), why)),
            _ => None,
        })
        .collect();
    if !conflicts.is_empty() {
        for (path, why) in &conflicts {
            match why {
                Some(why) => eprintln!("greenshift: {why}"),
                None => eprintln!(
                    "greenshift: {} exists and differs from what init writes; \
                     left alone (--force overwrites it)",
                    Tainted::from(*path)
                ),
            }
        }
        eprintln!("greenshift: nothing written");
        return Ok(ExitCode::from(1));
    }
    // Stage every file first, so that one that cannot be written leaves
    // every other one as it was.
    let ready = actions
        .iter()
        .filter_map(|action| match action {
            Action::Write {
                path,
                content,
                replaces,
            } => Some(staged(root, path, content).map(|file| Ready::Write {
                path,
                replaces: *replaces,
                adds: None,
                file,
            })),
            Action::Edit {
                path,
                content,
                what,
            } => Some(staged(root, path, content).map(|file| Ready::Write {
                path,
                replaces: true,
                adds: Some(what),
                file,
            })),
            Action::Unchanged { path } => Some(Ok(Ready::Unchanged { path })),
            Action::Conflict { .. } => None,
        })
        .collect::<anyhow::Result<Vec<Ready>>>()?;
    let mut changed = false;
    for item in ready {
        match item {
            Ready::Write {
                path,
                replaces,
                adds,
                file,
            } => {
                file.persist(root.join(path))
                    .map_err(|err| err.error)
                    .with_context(|| format!("could not write {}", Tainted::from(path)))?;
                match adds {
                    Some(what) => println!("greenshift: added {what} to {}", Tainted::from(path)),
                    None => {
                        let verb = if replaces { "overwrote" } else { "wrote" };
                        println!("greenshift: {verb} {}", Tainted::from(path));
                    }
                }
                changed = true;
            }
            Ready::Unchanged { path } => {
                println!("greenshift: {} is already up to date", Tainted::from(path));
            }
        }
    }
    if !changed {
        println!("greenshift: nothing to change; the repository is already set up");
    }
    Ok(ExitCode::SUCCESS)
}

/// [`stage`] for `path` under `root`, with an error naming it.
fn staged(root: &Path, path: &Path, content: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    stage(&root.join(path), content)
        .with_context(|| format!("could not write {}; nothing written", Tainted::from(path)))
}

/// `content` in a temporary file beside `path`, creating its directory,
/// for renaming into place, so the file is never left half written. It
/// has the mode of the file it replaces, or else the mode any new file
/// gets.
fn stage(path: &Path, content: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".greenshift-init-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Subject to the umask, as for any new file.
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut file = builder.tempfile_in(dir)?;
    match std::fs::metadata(path) {
        Ok(meta) => file.as_file().set_permissions(meta.permissions())?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    std::io::Write::write_all(&mut file, content.as_bytes())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forge_is_detected_only_when_exactly_one_directory_is_there() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_forge(dir.path()), Err(DetectForgeError::Neither));
        std::fs::create_dir(dir.path().join(".forgejo")).unwrap();
        assert_eq!(detect_forge(dir.path()), Ok(Forge::Forgejo));
        std::fs::create_dir(dir.path().join(".github")).unwrap();
        assert_eq!(detect_forge(dir.path()), Err(DetectForgeError::Both));
    }

    #[test]
    fn releases_are_plain_tags_newer_than_the_last_without_history() {
        for ok in ["v0.1.1", "v0.2.0", "v1.0.0", "v10.20.30"] {
            assert_eq!(ok.parse::<Release>().unwrap().as_str(), ok);
        }
        for bad in [
            "",
            "v",
            "0.2.0",
            "v0.2",
            "v0.2.0.1",
            "v0.2.0-rc.1",
            "v01.2.0",
            "v0.02.0",
            "v+1.2.3",
            "V0.2.0",
            "v0.2.0 ",
            // Too big for a u64.
            "v18446744073709551616.0.0",
        ] {
            assert_eq!(bad.parse::<Release>(), Err(ReleaseError::Shape), "{bad:?}");
        }
        for old in ["v0.1.0", "v0.0.1", "v0.0.0"] {
            assert_eq!(old.parse::<Release>(), Err(ReleaseError::TooOld), "{old:?}");
        }
    }

    #[test]
    fn checksums_are_64_hex_digits_kept_in_lower_case() {
        let upper = "ABCDEF0123456789".repeat(4);
        assert_eq!(
            upper.parse::<Sha256>().unwrap().as_str(),
            upper.to_ascii_lowercase()
        );
        for bad in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "z".repeat(64),
        ] {
            assert!(bad.parse::<Sha256>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn runner_labels_go_into_yaml_as_they_are() {
        for ok in ["ubuntu-24.04", "self-hosted:docker_x", "-", "null"] {
            assert_eq!(ok.parse::<RunnerLabel>().unwrap().as_str(), ok);
        }
        let long = "a".repeat(257);
        for bad in ["", "a b", "a\nb", "'x'", "x#y", "[x]", &long] {
            assert!(bad.parse::<RunnerLabel>().is_err(), "{bad:?}");
        }
        assert!("a".repeat(256).parse::<RunnerLabel>().is_ok());
    }
}
