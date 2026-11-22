//! The Leiningen backend: `greenshift run lein`.
//!
//! It lists in one `lein test` JVM with greenshift's listing injected
//! ([`LIST_CLJ`]): `lein update-in :injections conj '(load-file "…")' --
//! test <args>`, which makes `clojure.test/run-tests` write the tests
//! lein selected, by its own selectors, as an inventory, and run none.
//! The arguments after `--` (selectors and namespaces) go to the listing
//! only, as `-E` goes only to `cargo nextest list`: lein ORs selectors, so
//! given to a run as well they would add to the group's tests.
//!
//! Each group runs in one more JVM, `lein … -- test :greenshift`, the
//! `:greenshift` selector in project.clj (`greenshift init lein` adds it)
//! reading the group's names from a file of its own, one `ns/var` per
//! line, and loading only the namespaces it names. A project without
//! the selector runs `lein test :only ns/var …` instead, split into
//! batches that fit on a command line ([`only_batches`]).
//!
//! Every run JVM has greenshift's JUnit reporter injected ([`JUNIT_CLJ`]),
//! which writes the group's results to a file of its own, and runs the
//! namespaces in the order the group first names them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::Context;

use super::{Backend, Invocation, Listed};
use crate::lein::clojure_string;
use crate::plan::{Inventory, InventoryTest};
use crate::select::Class;
use crate::tainted::Tainted;

/// The listing injection: see the file's own comments.
pub const LIST_CLJ: &str = include_str!("lein/list.clj");

/// The JUnit reporter injection, ported from vaelii-lacuna#81's run-vars
/// reporter: see the file's own comments.
pub const JUNIT_CLJ: &str = include_str!("lein/junit.clj");

/// The default limit on the test names one `lein test :only` is given,
/// in bytes, a separator each included: well under Linux's limit on a
/// command line (ARG_MAX, 2 MiB with the default stack), the environment
/// and lein's own arguments with it.
pub const MAX_ONLY_BYTES: usize = 100_000;

/// The program run.
const LEIN: &str = "lein";

/// How a run JVM is told which tests to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// project.clj's `:greenshift` selector, reading a names file.
    Names,
    /// lein's own `:only`, with the names as arguments.
    Only,
}

impl Selector {
    /// The selector a project can run with: [`Selector::Names`] when
    /// `project_clj`'s literal `:test-selectors` has the `:greenshift`
    /// entry, else [`Selector::Only`], with a note saying why: the entry
    /// is missing, the map computed, or the file unreadable. A missing
    /// file is noted too: lein, run in a subdirectory, finds the project
    /// in a parent and runs, so without a note the fallback would pass
    /// unseen.
    pub fn for_project(project_clj: &Path) -> (Selector, Option<String>) {
        let text = match std::fs::read_to_string(project_clj) {
            Ok(text) => text,
            Err(err) => {
                return (
                    Selector::Only,
                    Some(format!(
                        "could not read {} ({err}), so each group runs as \
                         `lein test :only ns/var ...`; run greenshift in the project's root",
                        Tainted::from(project_clj)
                    )),
                );
            }
        };
        match crate::lein::has_selector(&text) {
            Ok(true) => (Selector::Names, None),
            Ok(false) => (
                Selector::Only,
                Some(
                    if matches!(
                        crate::lein::selectors(&text),
                        Ok(crate::lein::Selectors::Computed)
                    ) {
                        "project.clj's :test-selectors is computed, so greenshift cannot \
                         find a :greenshift selector in it, and each group runs as \
                         `lein test :only ns/var ...`"
                    } else {
                        "project.clj has no :greenshift test selector, so each group runs \
                         as `lein test :only ns/var ...`; `greenshift init lein` adds it"
                    }
                    .to_owned(),
                ),
            ),
            Err(err) => (
                Selector::Only,
                Some(format!(
                    "{err}; each group runs as `lein test :only ns/var ...`"
                )),
            ),
        }
    }
}

/// The Leiningen backend, set up from the arguments to pass on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lein {
    /// The program to run.
    pub program: OsString,
    /// The arguments `lein test` lists with: selectors and namespaces.
    pub test_args: Vec<OsString>,
    /// How each group's run is told its tests.
    pub selector: Selector,
    /// The most bytes of names one `lein test :only` is given.
    pub max_only_bytes: usize,
}

/// `(load-file "<script>")`, the injection that loads `script`.
pub fn injection(script: &Path) -> anyhow::Result<OsString> {
    Ok(format!("(load-file {})", clojure_path(script)?).into())
}

/// `path` as a Clojure string literal.
fn clojure_path(path: &Path) -> anyhow::Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", Tainted::from(path)))?;
    Ok(clojure_string(text))
}

/// The listing injection, writing the tests to `out`.
pub fn list_script(out: &Path) -> anyhow::Result<String> {
    Ok(format!(
        "{LIST_CLJ}\n(greenshift.lein.list/install! {})\n",
        clojure_path(out)?
    ))
}

/// The reporter injection for one run: it sets the property the
/// `:greenshift` selector reads to `names`, and writes the run's JUnit
/// to `junit`, or, when `names` names a namespace lein did not require
/// itself, those namespaces to [`unloaded_file`] instead.
pub fn run_script(names: &Path, junit: &Path) -> anyhow::Result<String> {
    let unloaded = clojure_path(&unloaded_file(junit))?;
    let names = clojure_path(names)?;
    let junit = clojure_path(junit)?;
    Ok(format!(
        "{JUNIT_CLJ}\n(System/setProperty {} {names})\n\
         (greenshift.lein.junit/install! {{:names {names} :junit {junit} :unloaded {unloaded}}})\n",
        clojure_string(crate::lein::NAMES_PROPERTY)
    ))
}

/// Where the reporter writing `junit` lists the namespaces it was given
/// that lein did not require itself, one a line: those outside
/// `:test-paths`.
pub fn unloaded_file(junit: &Path) -> PathBuf {
    junit.with_extension("unloaded")
}

/// Why a run whose reporter wrote `junit` is refused: the namespaces in
/// its [`unloaded_file`], which `lein test <ns>` loads and greenshift
/// does not.
pub fn refusal(junit: &Path) -> Option<String> {
    let text = std::fs::read_to_string(unloaded_file(junit)).ok()?;
    let namespaces: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| Tainted::raw(line).to_string())
        .collect();
    let (list, verb) = match namespaces.as_slice() {
        [] => return None,
        [one] => (one.clone(), "is"),
        many => (many.join(", "), "are"),
    };
    Some(format!(
        "{list} {verb} not under :test-paths: `lein test` loads a namespace it is given \
         from anywhere on the classpath, but greenshift does not yet"
    ))
}

/// A test as lein names it, `ns/var`: its suite, the namespace, and its
/// name, the var.
pub fn test_name(test: &InventoryTest) -> String {
    format!("{}/{}", test.suite().as_raw(), test.name().as_raw())
}

/// `names` in batches of at most `max` bytes each, counting a separator
/// after every name, in order: every name in exactly one. A name longer
/// than `max` is a batch alone. No names give no batches.
pub fn only_batches(names: &[String], max: usize) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut bytes = 0;
    for name in names {
        let size = name.len() + 1;
        match out.last_mut() {
            Some(batch) if bytes + size <= max => batch.push(name.clone()),
            _ => {
                out.push(vec![name.clone()]);
                bytes = 0;
            }
        }
        bytes += size;
    }
    out
}

/// The tests in the listing file at `path`, which the listing injection
/// wrote.
pub fn read_listing(path: &Path) -> anyhow::Result<Inventory> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "lein test wrote no listing at {}: was the injection run?",
            Tainted::from(path)
        )
    })?;
    crate::plan::parse_inventory(&text).context("lein test's listing")
}

impl Lein {
    /// The backend for the arguments that follow `--` on greenshift's
    /// command line, all of which go to the listing's `lein test`.
    pub fn new(test_args: Vec<OsString>, selector: Selector, max_only_bytes: usize) -> Lein {
        Lein {
            program: LEIN.into(),
            test_args,
            selector,
            max_only_bytes,
        }
    }

    /// `lein update-in :injections conj (load-file "<script>") -- test`.
    fn with_injection(script: &Path) -> anyhow::Result<Vec<OsString>> {
        let mut args: Vec<OsString> = ["update-in", ":injections", "conj"]
            .map(OsString::from)
            .to_vec();
        args.push(injection(script)?);
        args.extend(["--", "test"].map(OsString::from));
        Ok(args)
    }

    /// The listing's arguments, its injection at `script`.
    pub fn list_command(&self, script: &Path) -> anyhow::Result<Vec<OsString>> {
        let mut args = Lein::with_injection(script)?;
        args.extend(self.test_args.iter().cloned());
        Ok(args)
    }

    /// A run's arguments, its injection at `script`, running `names`:
    /// `:greenshift` alone, or `:only` and the names.
    pub fn run_command(&self, script: &Path, names: &[String]) -> anyhow::Result<Vec<OsString>> {
        let mut args = Lein::with_injection(script)?;
        match self.selector {
            Selector::Names => args.push(":greenshift".into()),
            Selector::Only => {
                args.push(":only".into());
                args.extend(names.iter().map(OsString::from));
            }
        }
        Ok(args)
    }
}

impl Backend for Lein {
    fn name(&self) -> &'static str {
        "lein"
    }

    fn list(&self, dir: &Path) -> anyhow::Result<Listed> {
        let script = dir.join("list.clj");
        let out = dir.join("list.jsonl");
        std::fs::write(&script, list_script(&out)?)
            .context("could not write the listing injection")?;
        let started = Instant::now();
        // lein's output goes to stderr: greenshift's stdout carries only
        // the test runs' own.
        let status = std::process::Command::new(&self.program)
            .env_remove("GREENSHIFT_HISTORY_TOKEN")
            .args(self.list_command(&script)?)
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::io::stderr()))
            .status()
            .with_context(|| {
                format!("could not run {}", Tainted::from(Path::new(&self.program)))
            })?;
        if !status.success() {
            return Ok(Listed::Failed(super::exit_code(status).max(1)));
        }
        let tests = read_listing(&out)?;
        eprintln!(
            "greenshift: lein listed {} tests in {:.1} s",
            tests.iter().count(),
            started.elapsed().as_secs_f64()
        );
        Ok(Listed::Tests(tests))
    }

    fn invocations(
        &self,
        class: Class,
        tests: &[&InventoryTest],
        dir: &Path,
    ) -> anyhow::Result<Vec<Invocation>> {
        let class = match class {
            Class::Rich => "rich",
            Class::Poor => "poor",
        };
        let names: Vec<String> = tests.iter().map(|test| test_name(test)).collect();
        let mut batches = match self.selector {
            Selector::Names => vec![names],
            Selector::Only => only_batches(&names, self.max_only_bytes),
        };
        if batches.is_empty() {
            batches.push(Vec::new());
        }
        batches
            .iter()
            .enumerate()
            .map(|(at, batch)| {
                let stem = format!("{class}-{}", at + 1);
                let names_file = dir.join(format!("{stem}.names"));
                let script = dir.join(format!("{stem}.clj"));
                let junit: PathBuf = dir.join(format!("{stem}.xml"));
                let listed: String = batch.iter().map(|name| format!("{name}\n")).collect();
                Ok(Invocation {
                    program: self.program.clone(),
                    args: self.run_command(&script, batch)?,
                    setup: vec![
                        (names_file.clone(), listed),
                        (script, run_script(&names_file, &junit)?),
                    ],
                    junit,
                })
            })
            .collect()
    }

    fn refused(&self, invocation: &Invocation) -> Option<String> {
        refusal(&invocation.junit)
    }
}

#[cfg(test)]
mod tests;
