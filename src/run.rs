//! `greenshift run`: one command for the whole cycle, with a test runner
//! (a [`Backend`]) doing the listing and the running.
//!
//! The cycle: the backend lists the tests that would run now; they are
//! split into rich and poor as `greenshift plan` splits them
//! ([`cli::plan_groups`]); the backend runs the rich group and then the
//! poor group, each as one or more [`Invocation`]s, every one writing
//! JUnit to a file of its own; the execution's JUnit files are recorded
//! together as one distinct run, even when their bytes match a previous
//! execution. `greenshift ingest` instead imports identical reports once
//! per textual commit namespace.
//! The exit status is that of the first invocation that failed, or 0.
//!
//! Each backend lives in a module of its own, [`nextest`] and [`lein`]; this
//! module knows nothing of any one runner.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};

use anyhow::Context;

use crate::cli::{self, IngestArgs, RunOptions};
use crate::plan::{Inventory, InventoryTest};
use crate::select::Class;
use crate::tainted::Tainted;

pub mod lein;
pub mod nextest;

/// One run of a test runner's program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Files to write before it starts, such as the runner's config: each
    /// path and its content.
    pub setup: Vec<(PathBuf, String)>,
    /// Where it is to write its JUnit.
    pub junit: PathBuf,
}

/// What a backend's listing found.
///
/// ```
/// use greenshift::run::Listed;
/// let listed = Listed::Tests(Default::default());
/// if let Listed::Tests(inventory) = &listed {
///     assert_eq!(inventory.iter().count(), 0);
/// }
/// ```
#[derive(Debug)]
pub enum Listed {
    /// The tests that would run now, in the runner's order.
    Tests(Inventory),
    /// The listing failed, with this exit status; the runner has said why
    /// on stderr.
    Failed(u8),
}

/// A test runner `greenshift run` drives.
pub trait Backend {
    /// The runner's name, as `greenshift run` takes it.
    fn name(&self) -> &'static str;

    /// List the tests that would run now. `dir` is a scratch directory
    /// for the run, removed when it ends.
    fn list(&self, dir: &Path) -> anyhow::Result<Listed>;

    /// The invocations that run exactly `tests`, the `class` group, in
    /// order: together they run every one of them, each once. With no
    /// tests, one invocation that runs none, so the runner decides what
    /// running nothing means. Each writes its JUnit to a file of its own
    /// in `dir`.
    fn invocations(
        &self,
        class: Class,
        tests: &[&InventoryTest],
        dir: &Path,
    ) -> anyhow::Result<Vec<Invocation>>;
}

/// Run the whole cycle with `backend`; see the module docs.
pub fn run(backend: &dyn Backend, options: &RunOptions) -> anyhow::Result<ExitCode> {
    run_with_control(backend, options, &cli::TransportControl::default())
}

pub(crate) fn run_with_control(
    backend: &dyn Backend,
    options: &RunOptions,
    control: &cli::TransportControl,
) -> anyhow::Result<ExitCode> {
    let mut history_unavailable = false;
    if let Some(url) = &options.history_remote {
        let phase = control.phase();
        let restored =
            crate::history::Remote::new_cancellable(url, control.token()).and_then(|remote| {
                remote.restore(&options.select.db, options.history_cache_dir.as_deref())
            });
        if phase.finish() {
            return Ok(ExitCode::from(130));
        }
        match restored {
            Ok(crate::history::RestoreOutcome::Restored(report)) => {
                cli::show_restore_report(&report);
            }
            Ok(crate::history::RestoreOutcome::Missing) => {
                history_unavailable = true;
                eprintln!("greenshift: ⚠ no durable history; running without it");
            }
            Err(err) => {
                if err.is_cancelled() {
                    return Ok(ExitCode::from(130));
                }
                if err.is_usage() {
                    eprintln!("greenshift: usage error: {err}");
                    return Ok(ExitCode::from(2));
                }
                history_unavailable = true;
                eprintln!("greenshift: ⚠ {err}");
            }
        }
    }
    // Removed when dropped, at the end of the run.
    let scratch = tempfile::Builder::new()
        .prefix("greenshift-run-")
        .tempdir()
        .context("could not create a scratch directory")?;
    let dir =
        std::path::absolute(scratch.path()).context("could not find the scratch directory")?;
    let tests = match backend.list(&dir)? {
        Listed::Tests(tests) => tests,
        Listed::Failed(code) => return Ok(ExitCode::from(code)),
    };
    let (history, groups) = if history_unavailable {
        (
            crate::plan::HistoryState::Error,
            crate::plan::plan(&[], &tests),
        )
    } else {
        cli::plan_groups(&options.select, &tests)?
    };
    if history == crate::plan::HistoryState::Error {
        eprintln!("greenshift: running every test as rich");
    }
    let mut invocations = Vec::new();
    for (class, group) in planned_groups(groups.rich(), groups.poor()) {
        invocations.extend(backend.invocations(class, group, &dir)?);
    }
    let codes: Vec<u8> = invocations.iter().map(execute).collect();
    if tests.is_empty() {
        eprintln!("greenshift: no tests to run, so nothing to record");
    } else {
        let junit: Vec<PathBuf> = invocations.into_iter().map(|inv| inv.junit).collect();
        record(options, backend.name(), &junit);
    }
    if let Some(path) = &options.history_artifact {
        if let Err(err) = crate::history::snapshot::snapshot(&options.select.db, path) {
            eprintln!("greenshift: ⚠ could not export history artifact: {err}");
        }
    }
    Ok(ExitCode::from(combined(codes)))
}

/// The groups to run, in order: rich, then poor, each only if it has a
/// test, or rich alone, empty, when neither has one.
///
/// ```
/// let mut groups = greenshift::run::planned_groups(&[], &[]);
/// assert_eq!(groups.len(), 1);
/// assert_eq!(groups.next_back().unwrap().0, greenshift::select::Class::Rich);
/// assert!(groups.next().is_none());
/// ```
pub fn planned_groups<'g, 't>(
    rich: &'g [&'t InventoryTest],
    poor: &'g [&'t InventoryTest],
) -> impl DoubleEndedIterator<Item = (Class, &'g [&'t InventoryTest])> + ExactSizeIterator {
    let mut groups = [(Class::Rich, rich), (Class::Poor, poor)];
    if rich.is_empty() && !poor.is_empty() {
        groups.swap(0, 1);
    }
    let count = if !rich.is_empty() && !poor.is_empty() {
        2
    } else {
        1
    };
    groups.into_iter().take(count)
}

/// Write the invocation's setup files and run it, its output going to
/// greenshift's own, and return its exit status. One that cannot start
/// is named on stderr and counts as exit status 1.
fn execute(invocation: &Invocation) -> u8 {
    let started = (|| -> anyhow::Result<ExitStatus> {
        for (path, content) in &invocation.setup {
            std::fs::write(path, content).with_context(|| Tainted::from(path.as_path()))?;
        }
        std::process::Command::new(&invocation.program)
            .env_remove("GREENSHIFT_HISTORY_TOKEN")
            .args(&invocation.args)
            .status()
            .with_context(|| {
                format!(
                    "could not run {}",
                    Tainted::from(Path::new(&invocation.program))
                )
            })
    })();
    match started {
        Ok(status) => exit_code(status),
        Err(err) => {
            cli::show_error(&err);
            1
        }
    }
}

/// A finished process's exit status as greenshift exits with it: its
/// code, or for one a signal ended, 128 plus the signal, as a shell
/// gives it.
pub fn exit_code(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return u8::try_from(128 + signal).unwrap_or(1);
        }
    }
    1
}

/// The exit status of a run of several invocations, `codes` in the order
/// they ran: the first that is not 0, or 0 when all are.
pub fn combined(codes: impl IntoIterator<Item = u8>) -> u8 {
    codes.into_iter().find(|&code| code != 0).unwrap_or(0)
}

/// Record this execution's existing JUnit files as one distinct run:
/// with some missing, as a partial run; with none, not at all. The branch
/// or tag is `--branch` or `--tag`, or as `ingest` finds it in the CI
/// environment. Any error is named on stderr and does not change the
/// exit status: the tests have run.
fn record(options: &RunOptions, backend: &str, junit: &[PathBuf]) {
    let mut files = Vec::new();
    for file in junit {
        if file.exists() {
            files.push(file.clone());
        } else {
            eprintln!("greenshift: no JUnit at {}", Tainted::from(file.as_path()));
        }
    }
    if files.is_empty() {
        eprintln!("greenshift: no JUnit to record");
        return;
    }
    let partial = files.len() < junit.len();
    if partial {
        eprintln!(
            "greenshift: recording {} of {} JUnit files, as a partial run",
            files.len(),
            junit.len()
        );
    }
    let args = IngestArgs {
        files,
        db: options.select.db.clone(),
        commit: options.commit.clone(),
        source: record_source(options, backend).to_owned(),
        partial,
        branch: options.branch.clone(),
        tag: options.tag.clone(),
    };
    if let Err(err) = cli::record_execution(&args) {
        cli::show_error(&err);
    }
}

/// The source the run is recorded as from: the one `--source` names,
/// which is the backend's name unless given.
pub fn record_source<'a>(options: &'a RunOptions, backend: &'a str) -> &'a str {
    options
        .select
        .source
        .last()
        .map(String::as_str)
        .unwrap_or(backend)
}

#[cfg(test)]
mod tests;
