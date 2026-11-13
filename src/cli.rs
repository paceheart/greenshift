//! Command-line interface.

use std::path::PathBuf;

use anyhow::Context;
use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand};

use crate::junit;
use crate::store::{self, NewRun, Store};

/// Pick which tests to run from what your tests have done before.
#[derive(Debug, Parser)]
#[command(name = "greenshift", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record JUnit XML results in the history store, all as one run.
    Ingest(IngestArgs),
}

#[derive(Debug, Args)]
pub struct IngestArgs {
    /// JUnit XML files to record. All of them make up one run.
    #[arg(required = true, value_name = "JUNIT_XML")]
    pub files: Vec<PathBuf>,

    /// History store to write to (created if missing).
    #[arg(long, value_name = "PATH", default_value = store::DEFAULT_PATH)]
    pub db: PathBuf,

    /// Commit the tests ran against.
    #[arg(long, value_name = "SHA", value_parser = NonEmptyStringValueParser::new())]
    pub commit: Option<String>,

    /// Where the results came from, e.g. the test runner or CI job.
    #[arg(
        long,
        value_name = "NAME",
        default_value = "junit",
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub source: String,
}

/// Parse `std::env::args` and run the requested command.
pub fn run() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Ingest(args) => ingest(&args),
    }
}

/// Parse every file first, so a bad file leaves the store untouched, then
/// record them all in one transaction.
pub fn ingest(args: &IngestArgs) -> anyhow::Result<()> {
    let mut cases = Vec::new();
    for file in &args.files {
        let report = junit::parse_file(file).with_context(|| format!("{}", file.display()))?;
        cases.extend(report.cases);
    }
    let file_name = args
        .files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut store = Store::open(&args.db)?;
    let recorded = store.record_run(
        &NewRun {
            commit: args.commit.as_deref(),
            source: &args.source,
            file_name: &file_name,
        },
        &cases,
    )?;
    println!(
        "greenshift: recorded run {} with {} tests from {} file{} into {}",
        recorded.run_id,
        recorded.results,
        args.files.len(),
        if args.files.len() == 1 { "" } else { "s" },
        args.db.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ingest_defaults() {
        let cli = Cli::try_parse_from(["greenshift", "ingest", "a.xml", "b.xml"]).unwrap();
        let Command::Ingest(args) = cli.command;
        assert_eq!(args.files, [PathBuf::from("a.xml"), PathBuf::from("b.xml")]);
        assert_eq!(args.db, PathBuf::from(".greenshift/history.sqlite"));
        assert_eq!(args.commit, None);
        assert_eq!(args.source, "junit");
    }

    #[test]
    fn ingest_options() {
        let cli = Cli::try_parse_from([
            "greenshift",
            "ingest",
            "--db",
            "h.sqlite",
            "--commit",
            "abc123",
            "--source",
            "nextest",
            "a.xml",
        ])
        .unwrap();
        let Command::Ingest(args) = cli.command;
        assert_eq!(args.db, PathBuf::from("h.sqlite"));
        assert_eq!(args.commit.as_deref(), Some("abc123"));
        assert_eq!(args.source, "nextest");
    }

    #[test]
    fn ingest_needs_a_file_and_non_empty_options() {
        assert!(Cli::try_parse_from(["greenshift", "ingest"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--commit", "", "a.xml"]).is_err());
        assert!(Cli::try_parse_from(["greenshift", "ingest", "--source", "", "a.xml"]).is_err());
    }
}
