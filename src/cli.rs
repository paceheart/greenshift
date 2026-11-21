//! Command-line interface.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::builder::{NonEmptyStringValueParser, StyledStr};
use clap::error::{ContextKind, ContextValue};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::report_key::{ReportKey, ReportKeySet};
use crate::select::{self, Class, Empirical, Estimator, Line, Smoothed, Thresholds, Wilson};
use crate::store::{self, CommitId, ImportAcknowledgement, NewRun, ReplayGrouping, Store};
use crate::tainted::Tainted;
use crate::{junit, nextest, plan, run};

/// Per-invocation transport cancellation. The binary owns signal policy; library
/// callers can cancel operations without changing process-wide signal handlers.
#[derive(Clone, Default)]
pub struct TransportControl {
    cancelled: Arc<AtomicBool>,
    // Serializes interrupt publication with the phase-to-caller transition.
    active: Arc<Mutex<bool>>,
}

impl TransportControl {
    /// Interrupt an active transport. False leaves non-transport signal handling
    /// to the binary, so a backend child cannot make Ctrl-C disappear.
    pub fn interrupt(&self) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *active {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        *active
    }

    pub(crate) fn token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub(crate) fn phase(&self) -> TransportPhase<'_> {
        *self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        TransportPhase {
            control: self,
            finished: false,
        }
    }
}

pub(crate) struct TransportPhase<'a> {
    control: &'a TransportControl,
    finished: bool,
}

impl TransportPhase<'_> {
    /// Stop accepting transport interrupts and observe all interrupts accepted
    /// before this transition under the same lock. A later signal is handled
    /// by the binary's non-transport policy, never lost between two atomics.
    pub(crate) fn finish(mut self) -> bool {
        let mut active = self
            .control
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active = false;
        self.finished = true;
        self.control.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for TransportPhase<'_> {
    fn drop(&mut self) {
        if !self.finished {
            *self
                .control
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = false;
        }
    }
}

/// Pick which tests to run from what your tests have done before.
#[derive(Debug, Parser)]
#[command(name = "greenshift", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Import all-new JUnit reports as one aggregate run; all-seen replays
    /// no-op, and mixing seen/unseen reports fails without recording.
    Ingest(IngestArgs),
    /// Print which tests to run first (rich) and after (poor), from history.
    Select(SelectArgs),
    /// Print select's choice as a cargo-nextest filterset (`-E`).
    NextestFilter(NextestFilterArgs),
    /// Print select's choice for the current tests as JSON rich and poor
    /// groups, for any test runner.
    Plan(PlanArgs),
    /// List the tests, run the rich and then the poor ones with a test
    /// runner, and record the results as one run.
    Run(RunArgs),
    /// Restore or publish durable history, or print its immutable cache key.
    History(HistoryArgs),
    /// Set the repository up to run its tests with greenshift in CI: write
    /// the CI workflow, pinned to a greenshift release. Changes nothing
    /// that exists unless --force is given.
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[command(subcommand)]
    pub backend: InitBackend,
}

/// The test runners `init` sets a repository up for.
#[derive(Debug, Subcommand)]
pub enum InitBackend {
    /// Set up `greenshift run nextest`, with cargo-nextest.
    Nextest(InitNextestArgs),
    /// Set up `greenshift run lein`, with Leiningen: add the :greenshift
    /// test selector to project.clj, and with --ci, write a workflow.
    Lein(InitLeinArgs),
}

#[derive(Debug, Args)]
pub struct InitLeinArgs {
    /// Also write a GitHub workflow, .github/workflows/greenshift.yml,
    /// that runs `greenshift run lein` with durable history. Only GitHub
    /// is supported for now.
    #[arg(long, value_enum, value_name = "FORGE", num_args = 0..=1,
          default_missing_value = "github")]
    pub ci: Option<crate::init::Forge>,

    /// The greenshift release the workflow installs, such as v0.3.0; with
    /// --ci, required. It needs `run lein`.
    #[arg(long, value_name = "TAG", requires = "ci")]
    pub release: Option<crate::init::Release>,

    /// The release binary's SHA-256, from the release's .sha256 file,
    /// checked independently; with --ci, required.
    #[arg(long, value_name = "HEX", requires = "ci")]
    pub sha256: Option<crate::init::Sha256>,

    /// The runner label the jobs run on, an x86_64 Linux runner.
    /// Default: ubuntu-latest.
    #[arg(long, value_name = "LABEL", requires = "ci")]
    pub runs_on: Option<crate::init::RunnerLabel>,

    /// The JDK the workflow sets up (Temurin). Default: 25.
    #[arg(long, value_name = "VERSION", requires = "ci")]
    pub java_version: Option<crate::init::lein::JavaVersion>,

    /// Replace a :greenshift selector or a workflow that exists and differs.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct InitNextestArgs {
    /// The forge to write the CI workflow for. Default: the one whose
    /// directory (.github or .forgejo) the repository has.
    #[arg(long, value_enum, value_name = "FORGE")]
    pub ci: Option<crate::init::Forge>,

    /// The greenshift release the workflow installs, such as v0.2.0. It
    /// needs `run nextest` and durable history, so it is newer than v0.1.0.
    /// All writers and trusted publishers sharing history must support its
    /// schema. Tag validation alone does not establish schema compatibility.
    #[arg(long, value_name = "TAG", required = true)]
    pub release: crate::init::Release,

    /// The release binary's SHA-256, from the release's .sha256 file,
    /// checked independently. The workflow refuses a binary that does not
    /// match it.
    #[arg(long, value_name = "HEX", required = true)]
    pub sha256: crate::init::Sha256,

    /// The runner label the jobs run on, an x86_64 Linux runner: releases
    /// ship only for x86_64-unknown-linux-musl. Default: ubuntu-latest on
    /// GitHub, docker on Forgejo.
    #[arg(long, value_name = "LABEL")]
    pub runs_on: Option<crate::init::RunnerLabel>,

    /// Replace generated files that exist and differ.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub operation: HistoryCommand,
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Restore canonical history (a cache miss is harmless).
    Restore(HistoryDataOptions),
    /// Validate and publish a snapshot; run only in a fresh trusted job.
    Publish(HistoryPublishOptions),
    /// Print the canonical ref identity for stock actions/cache.
    CacheKey(HistoryRemoteOptions),
}

#[derive(Debug, Args)]
pub struct HistoryRemoteOptions {
    /// Canonical repository URL. Credentials belong in GREENSHIFT_HISTORY_TOKEN.
    #[arg(long)]
    pub remote: String,
}

#[derive(Debug, Args)]
pub struct HistoryDataOptions {
    #[command(flatten)]
    pub location: HistoryRemoteOptions,
    #[arg(long, default_value = store::DEFAULT_PATH)]
    pub db: PathBuf,
    /// Optional stock-action cache directory (not a cache service endpoint).
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct HistoryPublishOptions {
    #[command(flatten)]
    pub data: HistoryDataOptions,
    /// Required for CI publishing: the forge's configured default branch.
    #[arg(long)]
    pub default_branch: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(subcommand)]
    pub backend: Backend,
}

/// The test runners `run` drives.
#[derive(Debug, Subcommand)]
pub enum Backend {
    /// Run the tests with cargo-nextest.
    Nextest(NextestRunArgs),
    /// Run the tests with Leiningen (`lein test`).
    Lein(LeinRunArgs),
}

/// What `run` takes whatever the test runner.
#[derive(Debug, Args)]
#[command(mut_arg("db", |db| {
    db.help("History store to read and record the run in (created if missing)")
}))]
pub struct RunOptions {
    #[command(flatten)]
    pub select: SelectArgs,

    /// Commit the tests run against, as for `ingest --commit`.
    #[arg(long, value_name = "SHA")]
    pub commit: Option<CommitId>,

    /// Branch the tests run on, as for `ingest --branch`.
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
    pub branch: Option<String>,

    /// Tag the tests run on, instead of a branch, as for `ingest --tag`.
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new(),
          conflicts_with = "branch")]
    pub tag: Option<String>,

    /// Restore durable history before listing tests (opt-in).
    #[arg(long)]
    pub history_remote: Option<String>,

    /// Write a validated SQLite data artifact after recording, for a fresh publisher.
    #[arg(long)]
    pub history_artifact: Option<PathBuf>,

    /// Optional directory restored by stock actions/cache.
    #[arg(long)]
    pub history_cache_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(mut_arg("source", |source| {
    source
        .action(clap::ArgAction::Set)
        .default_value("nextest")
        .help("Read only runs from this source, and record the run as from it")
}))]
pub struct NextestRunArgs {
    #[command(flatten)]
    pub options: RunOptions,

    /// The longest filterset to give one `cargo nextest run`, in bytes;
    /// a longer one is split over several runs.
    #[arg(long, value_name = "BYTES", default_value_t = run::nextest::MAX_FILTERSET_BYTES,
          value_parser = clap::value_parser!(usize))]
    pub max_filterset_bytes: usize,

    /// Arguments for `cargo nextest run` and, where it takes them,
    /// `cargo nextest list`.
    #[arg(last = true, value_name = "NEXTEST_ARGS")]
    pub args: Vec<std::ffi::OsString>,
}

#[derive(Debug, Args)]
#[command(mut_arg("source", |source| {
    source
        .action(clap::ArgAction::Set)
        .default_value("lein")
        .help("Read only runs from this source, and record the run as from it")
}))]
pub struct LeinRunArgs {
    #[command(flatten)]
    pub options: RunOptions,

    /// The most bytes of test names to give one `lein test :only`, when
    /// project.clj has no :greenshift selector; more are split over
    /// several runs.
    #[arg(long, value_name = "BYTES", default_value_t = run::lein::MAX_ONLY_BYTES,
          value_parser = clap::value_parser!(usize))]
    pub max_only_bytes: usize,

    /// Arguments for the listing's `lein test`: selectors and namespaces,
    /// as `lein test` takes them. The runs get only greenshift's own.
    #[arg(last = true, value_name = "LEIN_TEST_ARGS")]
    pub args: Vec<std::ffi::OsString>,
}

#[derive(Debug, Args)]
pub struct IngestArgs {
    /// JUnit XML files to import as one run. Exact report bytes at the same
    /// commit are recorded once, regardless of path/order. Replays retain the
    /// first grouping and metadata; partial overlap with unseen reports fails.
    /// Without --commit, imports share one unknown-commit namespace. Use
    /// `run nextest` to record distinct executions, including identical reports.
    #[arg(required = true, value_name = "JUNIT_XML")]
    pub files: Vec<PathBuf>,

    /// History store to write to (created if missing).
    #[arg(long, value_name = "PATH", default_value = store::DEFAULT_PATH)]
    pub db: PathBuf,

    /// Commit the tests ran against: 7 to 64 hex digits, as
    /// `git rev-parse HEAD` prints it. Stored in lower case.
    #[arg(long, value_name = "SHA")]
    pub commit: Option<CommitId>,

    /// Where the results came from, e.g. the test runner or CI job.
    #[arg(
        long,
        value_name = "NAME",
        default_value = "junit",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub source: String,

    /// The files hold only part of the suite (one pass of several, say,
    /// when another wrote no JUnit). Such a run never counts as full, but
    /// its results still count towards each test's results.
    #[arg(long)]
    pub partial: bool,

    /// Branch the tests ran on. Default: from the CI environment,
    /// GITHUB_HEAD_REF if nonempty (a pull request), else GITHUB_REF_NAME,
    /// unless GITHUB_REF_TYPE is tag; an empty variable counts as unset,
    /// and with neither, none.
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
    pub branch: Option<String>,

    /// Tag the tests ran on, instead of a branch. Default: from the CI
    /// environment, GITHUB_REF_NAME when GITHUB_REF_TYPE is tag.
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new(),
          conflicts_with = "branch")]
    pub tag: Option<String>,
}

#[derive(Debug, Args)]
pub struct SelectArgs {
    /// History store to read (never written).
    #[arg(long, value_name = "PATH", default_value = store::DEFAULT_PATH)]
    pub db: PathBuf,

    /// Use only runs from this source; repeat for several. Default: all.
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
    pub source: Vec<String>,

    /// Results a test needs to be listed as poor; with fewer it is new and
    /// not listed.
    #[arg(long, value_name = "N", default_value_t = 120)]
    pub min_runs: u64,

    /// End rich at the first test whose coverage reaches this percentage
    /// (0 to 100). Default: 100, the whole cover, unless --confidence is
    /// given.
    #[arg(long, value_name = "N", value_parser = percentage)]
    pub coverage: Option<f64>,

    /// End rich at the first test whose confidence reaches this percentage
    /// (0 to 100). With --coverage too, rich ends where both are reached.
    #[arg(long, value_name = "N", value_parser = percentage)]
    pub confidence: Option<f64>,

    /// How confidence is estimated.
    #[arg(long, value_enum, default_value_t = EstimatorKind::Wilson)]
    pub estimator: EstimatorKind,
}

#[derive(Debug, Args)]
pub struct NextestFilterArgs {
    #[command(flatten)]
    pub select: SelectArgs,

    /// The current tests: `cargo nextest list --message-format json`
    /// output (full or `--list-type binaries-only`), or `-` for stdin.
    /// Tests of binaries not in it are left out of the filterset.
    #[arg(long, value_name = "FILE", required = true)]
    pub inventory: PathBuf,

    /// Match the rich tests: every test but the poor ones, so tests new to
    /// the store run too. The default.
    #[arg(long, conflicts_with = "poor")]
    pub rich: bool,

    /// Match the poor tests.
    #[arg(long)]
    pub poor: bool,
}

#[derive(Debug, Args)]
#[command(mut_arg("db", |db| {
    db.help("History store to read (created empty if missing, else never written)")
}))]
pub struct PlanArgs {
    #[command(flatten)]
    pub select: SelectArgs,

    /// The current tests, one `{"suite": ..., "name": ...}` JSON object
    /// per line, or `-` for stdin. Each lands in exactly one group.
    #[arg(long, value_name = "FILE", required = true)]
    pub inventory: PathBuf,
}

impl NextestFilterArgs {
    /// Which tests the filterset is to match.
    pub fn class(&self) -> Class {
        if self.poor {
            Class::Poor
        } else {
            Class::Rich
        }
    }
}

/// The confidence estimators `select` offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EstimatorKind {
    /// The lower bound of the Wilson score interval (z = 1.96) for the
    /// share of green runs.
    Wilson,
    /// green / (green + rows left).
    Empirical,
    /// (green + 1) / (green + rows left + 2).
    Smoothed,
}

impl EstimatorKind {
    fn estimator(self) -> &'static dyn Estimator {
        match self {
            EstimatorKind::Wilson => &Wilson,
            EstimatorKind::Empirical => &Empirical,
            EstimatorKind::Smoothed => &Smoothed,
        }
    }
}

fn percentage(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(n) if (0.0..=100.0).contains(&n) => Ok(n),
        _ => Err("expected a percentage from 0 to 100".into()),
    }
}

/// `select`'s exit status when there is no full run to select from.
pub const NO_HISTORY: u8 = 2;

/// Parse a command line, `args[0]` being the program name.
///
/// clap's usage errors quote what was given on the command line as it was
/// given: a rejected value, an unknown argument or subcommand. Those
/// quotes are escaped here, as [`Tainted`] shows text, so a terminal
/// escape sequence or newline in argv prints harmlessly. The rest of the
/// error, styling included, is clap's.
pub fn parse<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args).map_err(escape_argv)
}

/// The clap error, with the parts it quotes from argv escaped.
fn escape_argv(mut err: clap::Error) -> clap::Error {
    let mut quoted = Vec::new();
    for kind in [
        ContextKind::InvalidArg,
        ContextKind::InvalidValue,
        ContextKind::InvalidSubcommand,
    ] {
        if let Some(ContextValue::String(raw)) = err.get(kind) {
            let shown = Tainted::raw(raw.as_str()).to_string();
            if shown != *raw {
                quoted.push((raw.clone(), shown.clone()));
                err.insert(kind, ContextValue::String(shown));
            }
        }
    }
    if quoted.is_empty() {
        return err;
    }
    // clap's tips ("to pass '--x' as a value, use '-- --x'") quote argv
    // too, inside styled text.
    if let Some(ContextValue::StyledStrs(tips)) = err.get(ContextKind::Suggested) {
        let tips: Option<Vec<StyledStr>> =
            tips.iter().map(|tip| escape_tip(tip, &quoted)).collect();
        match tips {
            Some(tips) => err.insert(ContextKind::Suggested, ContextValue::StyledStrs(tips)),
            None => err.remove(ContextKind::Suggested),
        };
    }
    err
}

/// The tip with each quote from argv replaced by its escaped form,
/// keeping clap's styling, or `None` if a quote's occurrences in the
/// styled text overlap, so that which are argv cannot be told.
fn escape_tip(tip: &StyledStr, quoted: &[(String, String)]) -> Option<StyledStr> {
    let mut text = tip.ansi().to_string();
    for (raw, shown) in quoted {
        let starts: Vec<usize> = text
            .char_indices()
            .map(|(at, _)| at)
            .filter(|&at| text[at..].starts_with(raw.as_str()))
            .collect();
        if starts.windows(2).any(|pair| pair[1] - pair[0] < raw.len()) {
            return None;
        }
        text = text.replace(raw.as_str(), shown);
    }
    Some(StyledStr::from(text))
}

/// Parse `std::env::args_os` and run the requested command.
pub fn run() -> anyhow::Result<ExitCode> {
    run_with_control(&TransportControl::default())
}

/// CLI entry for a binary that supplies its own transport cancellation policy.
pub fn run_with_control(control: &TransportControl) -> anyhow::Result<ExitCode> {
    let cli = parse(std::env::args_os()).unwrap_or_else(|err| err.exit());
    match cli.command {
        Command::Ingest(args) => ingest(&args).map(|()| ExitCode::SUCCESS),
        Command::Select(args) => select(&args),
        Command::NextestFilter(args) => nextest_filter(&args),
        Command::Plan(args) => plan(&args),
        Command::Run(args) => match args.backend {
            Backend::Nextest(args) => run_nextest_with_control(&args, control),
            Backend::Lein(args) => run_lein_with_control(&args, control),
        },
        Command::History(args) => history(&args, control),
        Command::Init(args) => match args.backend {
            InitBackend::Nextest(args) => init_nextest(&args, std::path::Path::new(".")),
            InitBackend::Lein(args) => init_lein(&args, std::path::Path::new(".")),
        },
    }
}

/// Write what `greenshift run nextest` needs in CI into the repository at
/// `root`; see [`crate::init`].
pub fn init_nextest(args: &InitNextestArgs, root: &std::path::Path) -> anyhow::Result<ExitCode> {
    use crate::init::{self, Forge};
    let forge = match args.ci {
        Some(forge) => forge,
        None => match init::detect_forge(root) {
            Ok(forge) => {
                println!("greenshift: found {}, so writing for {forge}", forge.dir());
                forge
            }
            Err(why) => {
                eprintln!("greenshift: usage error: {why}");
                return Ok(ExitCode::from(2));
            }
        },
    };
    if forge == Forge::Forgejo {
        println!(
            "greenshift: note: the Forgejo workflow is not isolated: every step, the tests \
             included, gets a token that can write the repository (see the top of the file)"
        );
        if let Some(hidden) = init::forgejo_would_hide(root) {
            println!(
                "greenshift: note: Forgejo runs the workflows of only the first of \
                 .forgejo/workflows, .gitea/workflows and .github/workflows there is, so \
                 committing .forgejo/workflows stops running the workflows in {hidden}; \
                 move them into .forgejo/workflows to keep them"
            );
        }
    }
    let runs_on = args
        .runs_on
        .clone()
        .unwrap_or_else(|| forge.default_runner());
    let settings = init::nextest::Settings {
        forge,
        release: &args.release,
        sha256: &args.sha256,
        runs_on: &runs_on,
    };
    let wants = init::nextest::wants(&settings);
    let actions = init::decide(root, wants, args.force)?;
    init::apply(root, &actions)
}

/// Set the Leiningen project at `root` up for `greenshift run lein`; see
/// [`crate::init::lein`].
pub fn init_lein(args: &InitLeinArgs, root: &std::path::Path) -> anyhow::Result<ExitCode> {
    use crate::init::{self, Forge};
    let workflow = match (args.ci, &args.release, &args.sha256) {
        (None, ..) => None,
        (Some(Forge::Forgejo), ..) => {
            eprintln!(
                "greenshift: usage error: init lein writes a GitHub workflow only, for now; \
                 use --ci github"
            );
            return Ok(ExitCode::from(2));
        }
        (Some(Forge::Github), Some(release), _)
            if release.version() <= init::LAST_RELEASE_WITHOUT_LEIN =>
        {
            let (major, minor, patch) = init::LAST_RELEASE_WITHOUT_LEIN;
            eprintln!(
                "greenshift: usage error: --release {}: v{major}.{minor}.{patch} and earlier \
                 have no `run lein`; pin a later release",
                release.as_str()
            );
            return Ok(ExitCode::from(2));
        }
        (Some(Forge::Github), Some(release), Some(sha256)) => Some((release, sha256)),
        (Some(Forge::Github), ..) => {
            eprintln!("greenshift: usage error: --ci needs --release and --sha256");
            return Ok(ExitCode::from(2));
        }
    };
    let mut actions = vec![init::lein::project_action(root, args.force)?];
    if let Some((release, sha256)) = workflow {
        let runs_on = args
            .runs_on
            .clone()
            .unwrap_or_else(|| Forge::Github.default_runner());
        let java = args.java_version.clone().unwrap_or_default();
        let settings = init::lein::Settings {
            release,
            sha256,
            runs_on: &runs_on,
            java: &java,
        };
        actions.extend(init::decide(
            root,
            init::lein::wants(&settings),
            args.force,
        )?);
    }
    init::apply(root, &actions)
}

fn history(args: &HistoryArgs, control: &TransportControl) -> anyhow::Result<ExitCode> {
    let location = match &args.operation {
        HistoryCommand::Restore(options) => &options.location,
        HistoryCommand::Publish(options) => &options.data.location,
        HistoryCommand::CacheKey(options) => options,
    };
    if let Err(error) = crate::history::validate_remote_argument(&location.remote) {
        eprintln!("greenshift: usage error: {error}");
        return Ok(ExitCode::from(2));
    }
    // Policy mistakes are configuration errors, not unavailable storage. Check
    // them before constructing a transport or touching its remote.
    if let HistoryCommand::Publish(options) = &args.operation {
        if std::env::var("GITHUB_EVENT_NAME")
            .is_ok_and(|event| event == "pull_request" || event == "pull_request_target")
        {
            eprintln!("greenshift: usage error: publishing history from pull requests is refused");
            return Ok(ExitCode::from(2));
        }
        if std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true") {
            let Some(branch) = options
                .default_branch
                .as_deref()
                .filter(|branch| !branch.is_empty())
            else {
                eprintln!("greenshift: usage error: CI publishing requires --default-branch");
                return Ok(ExitCode::from(2));
            };
            if std::env::var("GITHUB_EVENT_NAME").ok().as_deref() != Some("push")
                || std::env::var("GITHUB_REF").ok().as_deref()
                    != Some(&format!("refs/heads/{branch}"))
            {
                eprintln!(
                    "greenshift: usage error: only default-branch pushes may publish history"
                );
                return Ok(ExitCode::from(2));
            }
        }
    }
    let phase = control.phase();
    let remote = match crate::history::Remote::new_cancellable(&location.remote, control.token()) {
        Ok(remote) => remote,
        Err(error) => {
            if phase.finish() || error.is_cancelled() {
                return Ok(ExitCode::from(130));
            }
            if error.is_usage() {
                eprintln!("greenshift: usage error: {error}");
                return Ok(ExitCode::from(2));
            }
            eprintln!("greenshift: ⚠ {error}");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let result = (|| -> anyhow::Result<()> {
        match &args.operation {
            HistoryCommand::Restore(options) => {
                match remote.restore(&options.db, options.cache_dir.as_deref())? {
                    crate::history::RestoreOutcome::Missing => {
                        eprintln!("greenshift: ⚠ no durable history; running without it");
                    }
                    crate::history::RestoreOutcome::Restored(report) => {
                        show_restore_report(&report);
                    }
                }
            }
            HistoryCommand::Publish(options) => {
                let published =
                    remote.publish(&options.data.db, options.data.cache_dir.as_deref())?;
                println!("{}", published.oid());
                show_publish_report(published.report());
            }
            HistoryCommand::CacheKey(_) => {
                println!(
                    "greenshift-history-{}",
                    remote.head()?.as_deref().unwrap_or("empty")
                );
            }
        }
        Ok(())
    })();
    if phase.finish() {
        return Ok(ExitCode::from(130));
    }
    if let Err(err) = result {
        if err
            .downcast_ref::<crate::history::HistoryError>()
            .is_some_and(|error| error.is_cancelled())
        {
            return Ok(ExitCode::from(130));
        }
        eprintln!("greenshift: ⚠ {err:#}");
    }
    // History storage must never turn a successful test job into a failed one.
    Ok(ExitCode::SUCCESS)
}

fn show_publish_report(report: &crate::history::MergeReport) {
    show_merge_replays(report);
    for group in report.excluded_groups() {
        eprintln!(
            "greenshift: ⚠ history group {} was not published: partial report overlap; {} unseen report memberships remain only in the source artifact, with no automatic recovery. Nonconflicting history was published",
            group.history_id(), group.unseen_memberships(),
        );
    }
}

pub(crate) fn show_restore_report(report: &crate::history::MergeReport) {
    show_merge_replays(report);
    for group in report.excluded_groups() {
        eprintln!(
            "greenshift: ⚠ history group {} was not restored locally: partial report overlap; {} unseen report memberships remain in the incoming source, not the local destination",
            group.history_id(), group.unseen_memberships(),
        );
    }
    for group in report.retained_groups() {
        eprintln!(
            "greenshift: ⚠ local history group {} retains {} report memberships absent from the source; publishing back may exclude this group. Restore did not reconcile the grouping conflict; no automatic recovery",
            group.history_id(), group.unseen_memberships(),
        );
    }
}

fn show_merge_replays(report: &crate::history::MergeReport) {
    if report.metadata_replays() != 0 {
        eprintln!("greenshift: ⚠ report replay retains destination metadata, including source, partial coverage and branch/tag ({} replay groups)", report.metadata_replays());
    }
    if report.nonexact_replays() != 0 {
        eprintln!("greenshift: ⚠ existing report grouping retained; subset/spanning aggregate equivalence is not independently verified ({} replay groups)", report.nonexact_replays());
    }
}

/// Name `err` on stderr as greenshift names every error, whether it ends
/// the run (from `main`) or is planned past (by [`plan()`]).
pub fn show_error(err: &anyhow::Error) {
    eprintln!("greenshift: error: {err:#}");
}

/// Parse every file first, so a bad file leaves the store untouched, then
/// record them all in one transaction.
pub fn ingest(args: &IngestArgs) -> anyhow::Result<()> {
    record_reports(args, true)
}

/// A real execution is a new observation even when its reports are identical.
pub(crate) fn record_execution(args: &IngestArgs) -> anyhow::Result<()> {
    record_reports(args, false)
}

fn record_reports(args: &IngestArgs, keyed: bool) -> anyhow::Result<()> {
    let mut cases = Vec::new();
    let mut files = Vec::new();
    let mut keys = HashSet::new();
    for file in &args.files {
        let report = if keyed {
            let bytes = std::fs::read(file)
                .map_err(|source| junit::JunitError::Io {
                    path: file.clone(),
                    source,
                })
                .with_context(|| Tainted::from(file.as_path()))?;
            let key = ReportKey::from_bytes(args.commit.as_ref(), &bytes);
            if !keys.insert(key) {
                continue;
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| junit::JunitError::NotUtf8)
                .with_context(|| Tainted::from(file.as_path()))?;
            junit::parse_str(text).with_context(|| Tainted::from(file.as_path()))?
        } else {
            junit::parse_file(file).with_context(|| Tainted::from(file.as_path()))?
        };
        files.push(file.clone());
        cases.extend(report.into_cases());
    }
    let git_ref = resolve_git_ref(args.branch.as_deref(), args.tag.as_deref(), |var| {
        std::env::var_os(var)
    });
    let keys = if keyed {
        Some(ReportKeySet::new(keys)?)
    } else {
        None
    };
    let mut store = Store::open(&args.db)?;
    let run = NewRun {
        commit: args.commit.as_ref(),
        source: &args.source,
        files: &files,
        partial: args.partial,
        branch: git_ref.as_ref().and_then(GitRef::branch),
        tag: git_ref.as_ref().and_then(GitRef::tag),
    };
    let recorded = if let Some(keys) = &keys {
        match store.record_import(&run, keys, &cases)? {
            ImportAcknowledgement::Recorded(recorded) => recorded,
            ImportAcknowledgement::AlreadyPresent {
                grouping,
                metadata_retained,
            } => {
                println!(
                    "greenshift: reports already recorded in {}; no new observation",
                    Tainted::from(args.db.as_path())
                );
                if metadata_retained {
                    eprintln!("greenshift: warning: existing metadata retained, including source, partial coverage and branch/tag");
                }
                if grouping == ReplayGrouping::MembershipOnly {
                    eprintln!("greenshift: warning: existing grouping retained; report membership is already present, subset aggregate equivalence is not independently verified");
                }
                return Ok(());
            }
        }
    } else {
        store.record_run(&run, &cases)?
    };
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    println!(
        "greenshift: recorded {}run {} with {} test{} from {} file{} into {}{}",
        if args.partial { "partial " } else { "" },
        recorded.run_id,
        recorded.results,
        plural(recorded.results),
        files.len(),
        plural(files.len()),
        Tainted::from(args.db.as_path()),
        match &git_ref {
            Some(GitRef::Branch(name)) => format!(" on branch {}", Tainted::raw(name.as_str())),
            Some(GitRef::Tag(name)) => format!(" on tag {}", Tainted::raw(name.as_str())),
            None => String::new(),
        }
    );
    Ok(())
}

/// On a pull request, GitHub Actions sets this to the PR's branch.
const GITHUB_HEAD_REF: &str = "GITHUB_HEAD_REF";
/// The branch or tag pushed, or a pull request's merge ref (`7/merge`).
const GITHUB_REF_NAME: &str = "GITHUB_REF_NAME";
/// `branch` or `tag`, saying what [`GITHUB_REF_NAME`] names.
const GITHUB_REF_TYPE: &str = "GITHUB_REF_TYPE";

/// The CI variables `ingest` reads a branch or tag from. On a pull
/// request, GitHub Actions sets `GITHUB_HEAD_REF` to the PR's branch and
/// `GITHUB_REF_NAME` to its merge ref (`7/merge`); otherwise only
/// `GITHUB_REF_NAME`, to the branch or tag pushed, which
/// `GITHUB_REF_TYPE` says (`branch` or `tag`). Forgejo and Gitea Actions
/// set the same variables.
pub const CI_REF_VARS: [&str; 3] = [GITHUB_HEAD_REF, GITHUB_REF_NAME, GITHUB_REF_TYPE];

/// What a run's tests ran on.
#[derive(Debug, PartialEq, Eq)]
enum GitRef {
    Branch(String),
    Tag(String),
}

impl GitRef {
    fn branch(&self) -> Option<&str> {
        match self {
            GitRef::Branch(name) => Some(name),
            GitRef::Tag(_) => None,
        }
    }

    fn tag(&self) -> Option<&str> {
        match self {
            GitRef::Tag(name) => Some(name),
            GitRef::Branch(_) => None,
        }
    }
}

/// What to record a run on: the `branch` or `tag` flag when given (clap
/// allows one at most), else from [`CI_REF_VARS`] in `env`. There,
/// when `GITHUB_REF_TYPE` is `tag`, the tag is `GITHUB_REF_NAME`;
/// otherwise the branch is `GITHUB_HEAD_REF`, else `GITHUB_REF_NAME`. An
/// empty variable counts as unset, and a value that is not UTF-8 is still
/// recorded, with U+FFFD for each invalid sequence.
fn resolve_git_ref(
    branch: Option<&str>,
    tag: Option<&str>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<GitRef> {
    if let Some(branch) = branch {
        return Some(GitRef::Branch(branch.to_owned()));
    }
    if let Some(tag) = tag {
        return Some(GitRef::Tag(tag.to_owned()));
    }
    let var = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string_lossy().into_owned())
    };
    if env(GITHUB_REF_TYPE).is_some_and(|kind| kind == "tag") {
        return var(GITHUB_REF_NAME).map(GitRef::Tag);
    }
    var(GITHUB_HEAD_REF)
        .or_else(|| var(GITHUB_REF_NAME))
        .map(GitRef::Branch)
}

/// Print the selection. Reads the store only; with no full run from the
/// chosen sources (or no store at all), says so and exits [`NO_HISTORY`].
pub fn select(args: &SelectArgs) -> anyhow::Result<ExitCode> {
    let every_test = |_: &Tainted, _: &Tainted| true;
    with_lines(args, every_test, |lines| {
        let Some(lines) = lines else {
            return Ok(ExitCode::from(NO_HISTORY));
        };
        let mut out = std::io::stdout().lock();
        lines
            .iter()
            .try_for_each(|line| writeln!(out, "{line}"))
            .and_then(|()| out.flush())
            .map(|()| ExitCode::SUCCESS)
    })
}

/// Print the selection as one nextest filterset, from lines made as
/// [`select()`] makes them but chosen from the inventory's tests only
/// ([`nextest::Listing::has`]), so rich is never a test the inventory
/// shows is gone.
/// With no history there are no lines, so every test is rich: it prints
/// `all()` or `none()` and exits 0. The inventory is read first, so a bad
/// one fails before the store is read.
pub fn nextest_filter(args: &NextestFilterArgs) -> anyhow::Result<ExitCode> {
    let listing = read_inventory(&args.inventory, nextest::listing)?;
    let eligible = |suite: &Tainted, name: &Tainted| listing.has(suite.as_raw(), name.as_raw());
    with_lines(&args.select, eligible, |lines| {
        let mut out = std::io::stdout().lock();
        let lines = lines.unwrap_or_default();
        let filter = nextest::filter(lines, args.class(), listing.binaries());
        writeln!(out, "{filter}")
            .and_then(|()| out.flush())
            .map(|()| ExitCode::SUCCESS)
    })
}

/// Print the plan: the inventory split into rich and poor, from lines made
/// as [`select()`] makes them but chosen from the inventory's tests only,
/// by suite and name, as one line of JSON. A missing store is created
/// empty, through [`Store::open`]; an existing one is only read. With no
/// history there are no lines, so every test is rich. A store that cannot
/// be created or read is no history too: the plan's `history` is `error`,
/// stderr names the error, and it still exits 0, so a broken store never
/// stops the tests from running. The inventory is read first, so a bad
/// one fails, with nothing printed, before the store is touched.
pub fn plan(args: &PlanArgs) -> anyhow::Result<ExitCode> {
    let tests = read_inventory(&args.inventory, plan::parse_inventory)?;
    let (history, groups) = plan_groups(&args.select, &tests)?;
    let mut out = std::io::stdout().lock();
    written(
        writeln!(out, "{}", groups.to_json(history))
            .and_then(|()| out.flush())
            .map(|()| ExitCode::SUCCESS),
    )
}

/// `tests` split into rich and poor as [`plan()`] splits them, from lines
/// made as [`select()`] makes them but chosen from `tests` alone, by
/// suite and name, and whether the store could be used. A missing store
/// is created empty; an existing one is only read. With no history, or a
/// store that cannot be created or read, every test is rich; stderr says
/// which, naming the error.
pub fn plan_groups<'t>(
    args: &SelectArgs,
    tests: &'t [plan::InventoryTest],
) -> anyhow::Result<(plan::HistoryState, plan::Plan<'t>)> {
    let history = match plan_history(args) {
        Ok(history) => history,
        Err(err) => {
            show_error(&err);
            return Ok((plan::HistoryState::Error, plan::plan(&[], tests)));
        }
    };
    let known: HashSet<(&Tainted, &Tainted)> = tests
        .iter()
        .map(|test| (test.suite(), test.name()))
        .collect();
    let eligible = |suite: &Tainted, name: &Tainted| known.contains(&(suite, name));
    let mut groups = None;
    with_history(&history, args, eligible, |lines| {
        groups = Some(plan::plan(lines.unwrap_or_default(), tests));
        Ok(ExitCode::SUCCESS)
    })?;
    let groups = groups.expect("with_history hands the lines on");
    Ok((plan::HistoryState::Ok, groups))
}

/// Run the whole cycle with cargo-nextest; see [`run`].
pub fn run_nextest(args: &NextestRunArgs) -> anyhow::Result<ExitCode> {
    run_nextest_with_control(args, &TransportControl::default())
}

fn run_nextest_with_control(
    args: &NextestRunArgs,
    control: &TransportControl,
) -> anyhow::Result<ExitCode> {
    let backend = run::nextest::Nextest::new(
        &args.args,
        std::env::var_os("NEXTEST_PROFILE").as_deref(),
        args.max_filterset_bytes,
    )?;
    run::run_with_control(&backend, &args.options, control)
}

/// Run the whole cycle with Leiningen; see [`run`] and [`run::lein`].
pub fn run_lein(args: &LeinRunArgs) -> anyhow::Result<ExitCode> {
    run_lein_with_control(args, &TransportControl::default())
}

fn run_lein_with_control(
    args: &LeinRunArgs,
    control: &TransportControl,
) -> anyhow::Result<ExitCode> {
    let (selector, note) = run::lein::Selector::for_project(std::path::Path::new("project.clj"));
    if let Some(note) = note {
        eprintln!("greenshift: {note}");
    }
    let backend = run::lein::Lein::new(args.args.clone(), selector, args.max_only_bytes);
    run::run_with_control(&backend, &args.options, control)
}

/// The history `plan` reads. A missing store is created, and its history
/// read through the same connection: none, unless another process wrote
/// runs to it after the check found it missing.
fn plan_history(args: &SelectArgs) -> anyhow::Result<store::History> {
    // Only a store known to be missing is created; any other trouble
    // finding it is left to the read below to name.
    if matches!(args.db.try_exists(), Ok(false)) {
        return Ok(Store::open(&args.db)?.history(&args.source)?);
    }
    read_history(args)
}

/// Read the inventory at `path` (`-` for stdin) and `parse` it, with any
/// error naming the inventory.
fn read_inventory<T, E>(
    path: &std::path::Path,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> anyhow::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let context = || InventoryContext(Tainted::from(path));
    let text = if path.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin().lock()).context("inventory on stdin")?
    } else {
        std::fs::read_to_string(path).with_context(context)?
    };
    parse(&text).with_context(context)
}

/// The error context naming the inventory file: the path is kept raw and
/// escaped only when the error is shown.
#[derive(Debug)]
struct InventoryContext(Tainted);

impl std::fmt::Display for InventoryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "inventory {}", self.0)
    }
}

/// Compute `select`'s lines from the store and hand them to `write`, which
/// decides the exit status, then name any threshold not reached on stderr.
/// With no full run from the chosen sources (or no store at all), says so
/// on stderr and hands `write` `None`. The lines are chosen from the
/// tests `eligible` keeps, by suite and name; see [`select::analyse_among`].
fn with_lines(
    args: &SelectArgs,
    eligible: impl Fn(&Tainted, &Tainted) -> bool,
    write: impl FnOnce(Option<&[Line<'_>]>) -> std::io::Result<ExitCode>,
) -> anyhow::Result<ExitCode> {
    with_history(&read_history(args)?, args, eligible, write)
}

/// The history of the chosen sources in the store, read only: none if
/// there is no store.
fn read_history(args: &SelectArgs) -> anyhow::Result<store::History> {
    Ok(match Store::open_read_only(&args.db)? {
        Some(store) => store.history(&args.source)?,
        None => Default::default(),
    })
}

/// [`with_lines`] from `history`, already read.
fn with_history(
    history: &store::History,
    args: &SelectArgs,
    eligible: impl Fn(&Tainted, &Tainted) -> bool,
    write: impl FnOnce(Option<&[Line<'_>]>) -> std::io::Result<ExitCode>,
) -> anyhow::Result<ExitCode> {
    let Some(analysis) = select::analyse_among(history, eligible) else {
        eprintln!("no history: run the full suite");
        return written(write(None));
    };
    let estimator = args.estimator.estimator();
    let decided = select::split(
        &select::cover_coverages(&analysis),
        &select::cover_confidences(&analysis, estimator),
        Thresholds {
            coverage: args.coverage,
            confidence: args.confidence,
        },
    );
    let lines = select::lines(&analysis, estimator, decided.rich(), args.min_runs);
    let code = written(write(Some(lines.as_slice())))?;
    for missed in decided.unreached() {
        eprintln!("greenshift: {missed}");
    }
    Ok(code)
}

/// The exit status from writing output. A reader that stops early
/// (`| head`) has all the lines it wants, so that is a success.
fn written(result: std::io::Result<ExitCode>) -> anyhow::Result<ExitCode> {
    match result {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        result => Ok(result?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_ingest_programmatic_empty_import_does_not_create_store() {
        let directory = tempfile::tempdir().unwrap();
        let args = IngestArgs {
            files: vec![],
            db: directory.path().join("history.sqlite"),
            commit: None,
            source: "fixture".to_owned(),
            partial: false,
            branch: None,
            tag: None,
        };
        assert!(ingest(&args).is_err());
        assert!(!args.db.exists());
    }

    #[test]
    fn test_record_execution_identical_reports_are_distinct_from_import_retries() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("report.xml");
        std::fs::write(
            &file,
            r#"<testsuite name="gears"><testcase name="turns"/></testsuite>"#,
        )
        .unwrap();
        let args = IngestArgs {
            files: vec![file],
            db: directory.path().join("history.sqlite"),
            commit: Some("aaaaaaa".parse().unwrap()),
            source: "fixture".to_owned(),
            partial: false,
            branch: Some("fixture".to_owned()),
            tag: None,
        };
        record_execution(&args).unwrap();
        record_execution(&args).unwrap();
        ingest(&args).unwrap();
        ingest(&args).unwrap();
        let conn = rusqlite::Connection::open(&args.db).unwrap();
        let (runs, identities, members): (i64, i64, i64) = conn.query_row(
            "SELECT count(*),count(DISTINCT history_id),(SELECT count(*) FROM report_memberships) FROM runs", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).unwrap();
        assert_eq!((runs, identities, members), (3, 3, 1));
    }

    #[test]
    fn test_transport_finish_observes_latched_interrupt_and_disarms_phase() {
        let control = TransportControl::default();
        let phase = control.phase();
        assert!(control.interrupt());
        assert!(
            phase.finish(),
            "an accepted interrupt must stop caller continuation"
        );
        assert!(
            !control.interrupt(),
            "later signals use non-transport policy"
        );
    }

    #[test]
    fn test_transport_finish_and_interrupt_have_one_ordered_boundary() {
        for _ in 0..100 {
            let control = TransportControl::default();
            let phase = control.phase();
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let interrupt = scope.spawn(|| {
                    start.wait();
                    control.interrupt()
                });
                start.wait();
                let cancelled = phase.finish();
                assert_eq!(
                    cancelled,
                    interrupt.join().unwrap(),
                    "finish must observe every interrupt accepted before disarming"
                );
            });
            assert!(!control.interrupt());
        }
    }

    #[test]
    fn test_transport_phase_drop_disarms_on_early_return() {
        let control = TransportControl::default();
        drop(control.phase());
        assert!(!control.interrupt());
        assert!(!control.token().load(Ordering::SeqCst));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ingest_defaults() {
        let cli = Cli::try_parse_from(["greenshift", "ingest", "a.xml", "b.xml"]).unwrap();
        let Command::Ingest(args) = cli.command else {
            panic!("not ingest");
        };
        assert_eq!(args.files, [PathBuf::from("a.xml"), PathBuf::from("b.xml")]);
        assert_eq!(args.db, PathBuf::from(".greenshift/history.sqlite"));
        assert_eq!(args.commit, None);
        assert_eq!(args.source, "junit");
        assert!(!args.partial);
        assert_eq!(args.branch, None);
        assert_eq!(args.tag, None);
    }

    #[test]
    fn ingest_options() {
        let cli = Cli::try_parse_from([
            "greenshift",
            "ingest",
            "--db",
            "h.sqlite",
            "--commit",
            "abc1234",
            "--source",
            "nextest",
            "--partial",
            "--branch",
            "topic",
            "a.xml",
        ])
        .unwrap();
        let Command::Ingest(args) = cli.command else {
            panic!("not ingest");
        };
        assert_eq!(args.db, PathBuf::from("h.sqlite"));
        assert_eq!(args.commit.as_ref().map(CommitId::as_str), Some("abc1234"));
        assert_eq!(args.source, "nextest");
        assert!(args.partial);
        assert_eq!(args.branch.as_deref(), Some("topic"));
    }

    /// An environment holding only `vars`.
    fn env_of<'v>(vars: &'v [(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'v {
        move |var| {
            vars.iter()
                .find(|(name, _)| *name == var)
                .map(|(_, value)| value.into())
        }
    }

    /// The branch [`resolve_git_ref`] finds with no tag flag; panics on a tag.
    fn run_branch(
        flag: Option<&str>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Option<String> {
        resolve_git_ref(flag, None, env).map(|found| match found {
            GitRef::Branch(name) => name,
            GitRef::Tag(name) => panic!("a tag, {name:?}, not a branch"),
        })
    }

    #[test]
    fn a_tag_push_is_its_tag_and_the_flags_replace_the_env() {
        let tag_push = [
            ("GITHUB_HEAD_REF", ""),
            ("GITHUB_REF_NAME", "v1.2.0"),
            ("GITHUB_REF_TYPE", "tag"),
        ];
        let tag = |name: &str| Some(GitRef::Tag(name.to_owned()));
        let branch = |name: &str| Some(GitRef::Branch(name.to_owned()));
        assert_eq!(
            resolve_git_ref(None, None, env_of(&tag_push)),
            tag("v1.2.0")
        );
        assert_eq!(
            resolve_git_ref(Some("x"), None, env_of(&tag_push)),
            branch("x")
        );
        assert_eq!(
            resolve_git_ref(None, Some("v9"), env_of(&tag_push)),
            tag("v9")
        );
        let empty_tag = [("GITHUB_REF_NAME", ""), ("GITHUB_REF_TYPE", "tag")];
        assert_eq!(resolve_git_ref(None, None, env_of(&empty_tag)), None);
        // On a tag push the head ref is never the branch, even when set.
        let tag_and_head_ref = [
            ("GITHUB_HEAD_REF", "feature"),
            ("GITHUB_REF_NAME", "v1.2.0"),
            ("GITHUB_REF_TYPE", "tag"),
        ];
        assert_eq!(
            resolve_git_ref(None, None, env_of(&tag_and_head_ref)),
            tag("v1.2.0")
        );
        let empty_tag_and_head_ref = [
            ("GITHUB_HEAD_REF", "feature"),
            ("GITHUB_REF_NAME", ""),
            ("GITHUB_REF_TYPE", "tag"),
        ];
        assert_eq!(
            resolve_git_ref(None, None, env_of(&empty_tag_and_head_ref)),
            None
        );
        let branch_push = [("GITHUB_REF_NAME", "main"), ("GITHUB_REF_TYPE", "branch")];
        assert_eq!(
            resolve_git_ref(None, None, env_of(&branch_push)),
            branch("main")
        );
        // An unrecognized GITHUB_REF_TYPE is not "tag", so it falls through
        // to the branch precedence chain exactly as an unset one does.
        let unknown_type = [("GITHUB_REF_NAME", "main"), ("GITHUB_REF_TYPE", "unknown")];
        assert_eq!(
            resolve_git_ref(None, None, env_of(&unknown_type)),
            branch("main")
        );
    }

    #[test]
    fn the_branch_is_the_flag_then_the_head_ref_then_the_ref_name() {
        let both = [
            ("GITHUB_HEAD_REF", "feature"),
            ("GITHUB_REF_NAME", "7/merge"),
        ];
        assert_eq!(
            run_branch(Some("mine"), env_of(&both)).as_deref(),
            Some("mine")
        );
        assert_eq!(run_branch(None, env_of(&both)).as_deref(), Some("feature"));
        let push = [("GITHUB_HEAD_REF", ""), ("GITHUB_REF_NAME", "main")];
        assert_eq!(run_branch(None, env_of(&push)).as_deref(), Some("main"));
        assert_eq!(
            run_branch(None, env_of(&[("GITHUB_REF_NAME", "main")])).as_deref(),
            Some("main")
        );
        let empty = [("GITHUB_HEAD_REF", ""), ("GITHUB_REF_NAME", "")];
        assert_eq!(run_branch(None, env_of(&empty)), None);
        assert_eq!(run_branch(None, env_of(&[])), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_branch_variable_that_is_not_utf8_is_still_the_branch() {
        use std::os::unix::ffi::OsStringExt;
        let env = |var: &str| match var {
            "GITHUB_HEAD_REF" => Some(std::ffi::OsString::from_vec(b"bad\xffname".to_vec())),
            "GITHUB_REF_NAME" => Some("7/merge".into()),
            _ => None,
        };
        assert_eq!(run_branch(None, env).as_deref(), Some("bad\u{fffd}name"));
    }

    #[test]
    fn ingest_needs_a_file_and_non_empty_options() {
        assert!(Cli::try_parse_from(["greenshift", "ingest"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--commit", "", "a.xml"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--source", "", "a.xml"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--branch", "", "a.xml"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--tag", "", "a.xml"]).is_err());
        let both = ["--branch", "main", "--tag", "v1", "a.xml"];
        assert!(Cli::try_parse_from(["greenshift", "ingest"].iter().chain(&both)).is_err());
    }

    fn select_args(args: &[&str]) -> Result<SelectArgs, clap::Error> {
        let cli = Cli::try_parse_from(["greenshift", "select"].iter().chain(args))?;
        let Command::Select(args) = cli.command else {
            panic!("not select");
        };
        Ok(args)
    }

    #[test]
    fn select_defaults() {
        let args = select_args(&[]).unwrap();
        assert_eq!(args.db, PathBuf::from(".greenshift/history.sqlite"));
        assert_eq!(args.source, Vec::<String>::new());
        assert_eq!(args.min_runs, 120);
        assert_eq!(args.coverage, None);
        assert_eq!(args.confidence, None);
        assert_eq!(args.estimator, EstimatorKind::Wilson);
    }

    #[test]
    fn select_options() {
        let args = select_args(&[
            "--db",
            "h.sqlite",
            "--source",
            "ci",
            "--source",
            "nightly",
            "--min-runs",
            "7",
            "--coverage",
            "90",
            "--confidence",
            "99.5",
            "--estimator",
            "smoothed",
        ])
        .unwrap();
        assert_eq!(args.db, PathBuf::from("h.sqlite"));
        assert_eq!(args.source, ["ci", "nightly"]);
        assert_eq!(args.min_runs, 7);
        assert_eq!(args.coverage, Some(90.0));
        assert_eq!(args.confidence, Some(99.5));
        assert_eq!(args.estimator, EstimatorKind::Smoothed);
        for (name, kind) in [
            ("wilson", EstimatorKind::Wilson),
            ("empirical", EstimatorKind::Empirical),
        ] {
            assert_eq!(select_args(&["--estimator", name]).unwrap().estimator, kind);
        }
    }

    #[test]
    fn select_thresholds_are_percentages() {
        for flag in ["--coverage", "--confidence"] {
            for ok in ["0", "100", "50.25"] {
                assert!(select_args(&[flag, ok]).is_ok(), "{flag} {ok}");
            }
            for bad in ["-1", "100.1", "NaN", "inf", "high", ""] {
                assert!(select_args(&[flag, bad]).is_err(), "{flag} {bad}");
            }
        }
    }

    fn nextest_filter_args(args: &[&str]) -> Result<NextestFilterArgs, clap::Error> {
        let cli = Cli::try_parse_from(
            ["greenshift", "nextest-filter", "--inventory", "inv.json"]
                .iter()
                .chain(args),
        )?;
        let Command::NextestFilter(args) = cli.command else {
            panic!("not nextest-filter");
        };
        Ok(args)
    }

    #[test]
    fn nextest_filter_takes_select_options_and_a_class() {
        let args = nextest_filter_args(&[]).unwrap();
        assert_eq!(args.class(), Class::Rich);
        assert_eq!(args.inventory, PathBuf::from("inv.json"));
        assert!(Cli::try_parse_from(["greenshift", "nextest-filter"]).is_err());
        assert_eq!(args.select.db, PathBuf::from(".greenshift/history.sqlite"));
        assert_eq!(args.select.min_runs, 120);
        let args = nextest_filter_args(&[
            "--db",
            "h.sqlite",
            "--source",
            "nextest",
            "--min-runs",
            "3",
            "--coverage",
            "90",
            "--confidence",
            "80",
            "--estimator",
            "empirical",
            "--poor",
        ])
        .unwrap();
        assert_eq!(args.class(), Class::Poor);
        assert_eq!(args.select.db, PathBuf::from("h.sqlite"));
        assert_eq!(args.select.source, ["nextest"]);
        assert_eq!(args.select.min_runs, 3);
        assert_eq!(args.select.coverage, Some(90.0));
        assert_eq!(args.select.confidence, Some(80.0));
        assert_eq!(args.select.estimator, EstimatorKind::Empirical);
        assert_eq!(
            nextest_filter_args(&["--rich"]).unwrap().class(),
            Class::Rich
        );
        assert!(nextest_filter_args(&["--rich", "--poor"]).is_err());
        assert!(nextest_filter_args(&["--coverage", "101"]).is_err());
    }

    #[test]
    fn plan_takes_select_options_and_an_inventory() {
        assert!(Cli::try_parse_from(["greenshift", "plan"]).is_err());
        let cli = Cli::try_parse_from([
            "greenshift",
            "plan",
            "--inventory",
            "-",
            "--db",
            "h.sqlite",
            "--source",
            "lein",
            "--min-runs",
            "3",
            "--coverage",
            "90",
            "--estimator",
            "smoothed",
        ])
        .unwrap();
        let Command::Plan(args) = cli.command else {
            panic!("not plan");
        };
        assert_eq!(args.inventory, PathBuf::from("-"));
        assert_eq!(args.select.db, PathBuf::from("h.sqlite"));
        assert_eq!(args.select.source, ["lein"]);
        assert_eq!(args.select.min_runs, 3);
        assert_eq!(args.select.coverage, Some(90.0));
        assert_eq!(args.select.estimator, EstimatorKind::Smoothed);
        assert!(Cli::try_parse_from(["greenshift", "plan", "--inventory", "-", "--poor"]).is_err());
    }

    #[test]
    fn a_tip_has_each_quote_from_argv_escaped() {
        let quoted = [("\u{1b}x".to_owned(), r"\u{1b}x".to_owned())];
        let tip = StyledStr::from("pass '\u{1b}x', use '-- \u{1b}x'".to_owned());
        let tip = escape_tip(&tip, &quoted).unwrap();
        assert_eq!(tip.to_string(), r"pass '\u{1b}x', use '-- \u{1b}x'");
    }

    #[test]
    fn a_tip_whose_quotes_overlap_is_dropped() {
        // "aXa" occurs at 0 and 2: which one is argv cannot be told.
        let quoted = [("a\u{1b}a".to_owned(), r"a\u{1b}a".to_owned())];
        let tip = StyledStr::from("a\u{1b}a\u{1b}a".to_owned());
        assert!(escape_tip(&tip, &quoted).is_none());
    }

    #[test]
    fn parse_escapes_a_rejected_value_once() {
        let err = parse(["greenshift", "ingest", "--commit", "\u{1b}[31m", "a.xml"]).unwrap_err();
        assert_eq!(
            err.get(ContextKind::InvalidValue),
            Some(&ContextValue::String(r"\u{1b}[31m".to_owned()))
        );
    }

    #[test]
    fn parse_drops_a_tip_whose_argv_quotes_overlap() {
        // clap's tip reads "to pass '{arg}' as a value, use '-- {arg}'",
        // with styling after each quote. This argument ends with the text
        // between its two quotes, so its occurrences in the tip overlap.
        let arg = "--\u{1b}[0m' as a value, use '\u{1b}[32m-- --";
        let err = parse(["greenshift", "ingest", "a.xml", arg]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(err.get(ContextKind::Suggested).is_none(), "{err:?}");
        let shown = err.render().ansi().to_string();
        assert!(!shown.contains(arg), "{shown:?}");
    }

    #[test]
    fn an_inventory_error_keeps_the_path_raw_and_shows_it_escaped_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("\u{1b}[31mevil.json");
        let mut args = nextest_filter_args(&[]).unwrap();
        args.inventory = path.clone();
        let err = nextest_filter(&args).unwrap_err();
        let context = err
            .downcast_ref::<InventoryContext>()
            .unwrap_or_else(|| panic!("not an inventory error: {err:?}"));
        assert_eq!(context.0.as_raw(), path.to_string_lossy());
        let shown = format!("{err:#}");
        assert!(shown.contains(r"\u{1b}[31mevil.json"), "{shown:?}");
        assert!(!shown.contains(r"\\u{1b}"), "escaped twice: {shown:?}");
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
    }

    #[test]
    fn select_refuses_bad_options() {
        assert!(select_args(&["--source", ""]).is_err());
        assert!(select_args(&["--min-runs", "-1"]).is_err());
        assert!(select_args(&["--estimator", "bayes"]).is_err());
    }
}
