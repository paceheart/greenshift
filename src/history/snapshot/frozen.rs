//! Concrete immutable completed snapshot payloads, distinct from load builders.

use std::sync::Arc;

use super::dictionary::TestId;
use super::text::FrozenText;
use crate::junit::Outcome;
use crate::report_key::ReportKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ResultRow {
    test: TestId,
    outcome: Outcome,
    duration: Option<i64>,
}

impl ResultRow {
    pub(super) fn new(test: TestId, outcome: Outcome, duration: Option<i64>) -> Self {
        Self {
            test,
            outcome,
            duration,
        }
    }
    pub(super) fn test(&self) -> TestId {
        self.test
    }
    pub(super) fn outcome(&self) -> Outcome {
        self.outcome
    }
    pub(super) fn duration(&self) -> Option<i64> {
        self.duration
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RunMetadata {
    history_id: FrozenText,
    commit: Option<FrozenText>,
    source: FrozenText,
    at: FrozenText,
    partial: i64,
    branch: Option<FrozenText>,
    tag: Option<FrozenText>,
}

impl RunMetadata {
    pub(super) fn new(
        history_id: FrozenText,
        commit: Option<FrozenText>,
        source: FrozenText,
        at: FrozenText,
        partial: i64,
        branch: Option<FrozenText>,
        tag: Option<FrozenText>,
    ) -> Self {
        Self {
            history_id,
            commit,
            source,
            at,
            partial,
            branch,
            tag,
        }
    }
    pub(super) fn history_id(&self) -> &FrozenText {
        &self.history_id
    }
    pub(super) fn commit(&self) -> Option<&FrozenText> {
        self.commit.as_ref()
    }
    pub(super) fn source(&self) -> &FrozenText {
        &self.source
    }
    pub(super) fn at(&self) -> &FrozenText {
        &self.at
    }
    pub(super) fn partial(&self) -> i64 {
        self.partial
    }
    pub(super) fn branch(&self) -> Option<&FrozenText> {
        self.branch.as_ref()
    }
    pub(super) fn tag(&self) -> Option<&FrozenText> {
        self.tag.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Run {
    metadata: RunMetadata,
    files: FrozenFiles,
    results: FrozenResults,
    reports: FrozenReports,
}

impl Run {
    pub(super) fn new(
        metadata: RunMetadata,
        files: FrozenFiles,
        results: FrozenResults,
        reports: FrozenReports,
    ) -> Self {
        Self {
            metadata,
            files,
            results,
            reports,
        }
    }
    pub(super) fn metadata(&self) -> &RunMetadata {
        &self.metadata
    }
    pub(super) fn files(&self) -> &FrozenFiles {
        &self.files
    }
    pub(super) fn results(&self) -> &FrozenResults {
        &self.results
    }
    pub(super) fn reports(&self) -> &FrozenReports {
        &self.reports
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FrozenReports(Box<[ReportKey]>);

impl FrozenReports {
    pub(super) fn from_vec(mut reports: Vec<ReportKey>) -> Self {
        reports.sort();
        Self(reports.into_boxed_slice())
    }
    pub(super) fn as_slice(&self) -> &[ReportKey] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FrozenBytes(Arc<[u8]>);

impl FrozenBytes {
    pub(super) fn copy_from(bytes: &[u8]) -> Self {
        Self(Arc::from(bytes))
    }
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FrozenFiles(Box<[(i64, FrozenBytes)]>);

impl FrozenFiles {
    pub(super) fn from_vec(files: Vec<(i64, FrozenBytes)>) -> Self {
        Self(files.into_boxed_slice())
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &(i64, FrozenBytes)> {
        self.0.iter()
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FrozenResults(Box<[ResultRow]>);

impl FrozenResults {
    pub(super) fn from_vec(results: Vec<ResultRow>) -> Self {
        Self(results.into_boxed_slice())
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &ResultRow> {
        self.0.iter()
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

pub(super) struct FrozenRuns(Box<[Run]>);

impl FrozenRuns {
    pub(super) fn from_vec(runs: Vec<Run>) -> Self {
        Self(runs.into_boxed_slice())
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &Run> {
        self.0.iter()
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}
