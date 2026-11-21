//! greenshift: test selection from test history.
//!
//! This crate is the core: [`junit`] reads JUnit XML, [`store`] keeps the
//! per-repo SQLite history, [`select`] chooses tests from that history,
//! [`nextest`] writes that choice as a cargo-nextest filterset, [`plan`]
//! as rich and poor groups of the current tests for any runner, [`run`]
//! lists, runs and records the tests with a test runner, [`init`] sets a
//! repository up to do that in CI, [`lein`] reads and edits a Leiningen
//! project.clj for them, and [`cli`]
//! parses the command line and dispatches. [`tainted`] holds text from
//! outside the program so that it prints escaped.

pub mod cli;
pub mod history;
pub mod init;
pub mod junit;
pub mod lein;
pub mod nextest;
pub mod plan;
pub mod report_key;
pub mod run;
pub mod select;
pub mod store;
pub mod tainted;
