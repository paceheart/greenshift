//! Choosing tests from history: what `greenshift select` prints.
//!
//! Everything is computed from [`History`] first ([`analyse`]); deciding
//! how much of the cover counts as rich ([`split`]) and rendering lines
//! ([`lines`]) are separate steps over those numbers.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;

use crate::junit::Outcome;
use crate::junit::Tainted;
use crate::store::{History, StoredRun};

/// A run is full when it has at least this share of the largest result
/// count among the [`FULL_WINDOW`] full runs before it, as a fraction
/// `numerator / denominator`.
const FULL_SHARE: (u64, u64) = (9, 10);

/// How many of the latest full runs a run is compared with.
const FULL_WINDOW: usize = 3;

/// How many of a test's latest durations its median is taken over.
const MEDIAN_OF: usize = 5;

/// Estimates P(suite green | the tests so far passed).
pub trait Estimator {
    /// `green` is the number of full runs in which no universe test failed;
    /// `rows_left` the number of matrix rows (failing full runs) that none
    /// of the tests so far fails in. Returns a probability in `[0, 1]`.
    fn p_green(&self, green: u64, rows_left: u64) -> f64;
}

/// `green / (green + rows_left)`: the share of history runs in which the
/// tests so far passed that were green. With no such run at all (no green
/// run and no row left) nothing in history contradicts a green suite, so
/// it is 1.
#[derive(Debug, Clone, Copy, Default)]
pub struct Empirical;

impl Estimator for Empirical {
    fn p_green(&self, green: u64, rows_left: u64) -> f64 {
        let runs = green + rows_left;
        if runs == 0 {
            1.0
        } else {
            green as f64 / runs as f64
        }
    }
}

/// `(green + 1) / (green + rows_left + 2)`: [`Empirical`] with one green
/// and one red run added, so it never reaches 0 or 1.
#[derive(Debug, Clone, Copy, Default)]
pub struct Smoothed;

impl Estimator for Smoothed {
    fn p_green(&self, green: u64, rows_left: u64) -> f64 {
        (green as f64 + 1.0) / ((green + rows_left) as f64 + 2.0)
    }
}

/// One universe test and what history says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestInfo {
    pub suite: Tainted,
    pub name: Tainted,
    /// Median duration in eighths of a millisecond, so medians of medians
    /// stay exact; see [`TestInfo::median_ms`].
    pub median_eighths: u128,
    /// Passed results in full runs.
    pub passes: u64,
    /// Results of any outcome in any run read.
    pub results: u64,
}

impl TestInfo {
    /// The median duration in milliseconds.
    pub fn median_ms(&self) -> f64 {
        self.median_eighths as f64 / 8.0
    }
}

/// One step of the greedy cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    /// Index into [`Analysis::tests`].
    pub test: usize,
    /// Matrix rows that no test picked so far, this one included, fails in.
    pub rows_left: u64,
}

/// Everything [`lines`] needs, computed from history before anything is
/// decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// The universe, by suite and then name.
    pub tests: Vec<TestInfo>,
    /// The greedy cover, in pick order. It always ends with no rows left.
    pub cover: Vec<Pick>,
    /// Matrix rows: full runs in which a universe test failed.
    pub rows: u64,
    /// Full runs in which no universe test failed.
    pub green: u64,
}

/// Analyse `history`, which holds only the runs from the chosen sources.
/// `None` when it has no full run.
pub fn analyse(history: &History) -> Option<Analysis> {
    let full = full_runs(&history.runs);
    let newest = full.last()?;

    // The universe, by suite and name; `index` maps a test id into it.
    let mut ids: Vec<i64> = newest.results.iter().map(|r| r.test_id).collect();
    let no_name = (Tainted::default(), Tainted::default());
    let name_of = |id: &i64| history.tests.get(id).unwrap_or(&no_name);
    let raw = |id: &i64| {
        let (suite, name) = name_of(id);
        (suite.as_raw(), name.as_raw())
    };
    ids.sort_by(|a, b| raw(a).cmp(&raw(b)));
    let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // The matrix: for each failing full run, the universe tests failing in it.
    let mut passes = vec![0u64; ids.len()];
    let mut matrix: Vec<Vec<usize>> = Vec::new();
    let mut green = 0;
    for run in &full {
        let mut failing = Vec::new();
        for r in &run.results {
            let Some(&i) = index.get(&r.test_id) else {
                continue;
            };
            match r.outcome {
                Outcome::Passed => passes[i] += 1,
                Outcome::Failed | Outcome::Error => failing.push(i),
                Outcome::Skipped => {}
            }
        }
        if failing.is_empty() {
            green += 1;
        } else {
            matrix.push(failing);
        }
    }

    // Result counts and the latest durations, newest first, over every run.
    let mut results = vec![0u64; ids.len()];
    let mut latest: Vec<Vec<u64>> = vec![Vec::new(); ids.len()];
    for run in history.runs.iter().rev() {
        for r in &run.results {
            let Some(&i) = index.get(&r.test_id) else {
                continue;
            };
            results[i] += 1;
            if let Some(ms) = r.duration_ms {
                if latest[i].len() < MEDIAN_OF {
                    // 0 ms counts as 1 ms, so nothing divides by zero.
                    latest[i].push(ms.max(1) as u64);
                }
            }
        }
    }
    let medians: Vec<Option<u128>> = latest
        .into_iter()
        .map(|ms| median_eighths(ms.into_iter().map(|m| 8 * u128::from(m)).collect()))
        .collect();
    let fallback = median_eighths(medians.iter().flatten().copied().collect()).unwrap_or(8);

    let tests: Vec<TestInfo> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let (suite, name) = name_of(id).clone();
            TestInfo {
                suite,
                name,
                median_eighths: medians[i].unwrap_or(fallback),
                passes: passes[i],
                results: results[i],
            }
        })
        .collect();
    let cover = greedy_cover(&tests, &matrix);
    Some(Analysis {
        tests,
        cover,
        rows: matrix.len() as u64,
        green,
    })
}

/// The full runs among `runs`, oldest first. The first run is full; after
/// that a run is full when it has at least [`FULL_SHARE`] of the largest
/// result count among the [`FULL_WINDOW`] full runs before it.
fn full_runs(runs: &[StoredRun]) -> Vec<&StoredRun> {
    let mut full: Vec<&StoredRun> = Vec::new();
    for run in runs {
        let count = run.results.len() as u64;
        let largest = full
            .iter()
            .rev()
            .take(FULL_WINDOW)
            .map(|r| r.results.len() as u64)
            .max();
        let (num, den) = FULL_SHARE;
        if largest.is_none_or(|largest| count * den >= largest * num) {
            full.push(run);
        }
    }
    full
}

/// The median of `values`; the mean of the middle two for an even count.
/// The values are in eighths of a millisecond, and medians of them always
/// are whole eighths: per test they are multiples of 8, so a mean of two
/// is a multiple of 4, and a mean of two of those is whole.
fn median_eighths(mut values: Vec<u128>) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    })
}

/// Pick universe tests until every row of `matrix` has a picked test
/// failing in it; see [`better`] for which comes first.
fn greedy_cover(tests: &[TestInfo], matrix: &[Vec<usize>]) -> Vec<Pick> {
    let mut rows_of: Vec<Vec<usize>> = vec![Vec::new(); tests.len()];
    for (row, failing) in matrix.iter().enumerate() {
        for &t in failing {
            rows_of[t].push(row);
        }
    }
    // Failures in the rows still left.
    let mut fails: Vec<u64> = rows_of.iter().map(|rows| rows.len() as u64).collect();
    let mut left = vec![true; matrix.len()];
    let mut rows_left = matrix.len() as u64;
    let mut picked = vec![false; tests.len()];
    let mut cover = Vec::new();
    while rows_left > 0 {
        let best = (0..tests.len())
            .filter(|&t| !picked[t] && fails[t] > 0)
            .min_by(|&a, &b| better(tests, &fails, a, b))
            .expect("every row left has a failing test that is not picked");
        picked[best] = true;
        for &row in &rows_of[best] {
            if std::mem::replace(&mut left[row], false) {
                rows_left -= 1;
                for &t in &matrix[row] {
                    fails[t] -= 1;
                }
            }
        }
        cover.push(Pick {
            test: best,
            rows_left,
        });
    }
    cover
}

/// `Less` when test `a` is to be picked before `b`: the higher failure
/// chance per second, f / (f + p) / median, where f is its failures in the
/// rows left and p its passes in full runs; then the shorter median, then
/// suite, then name. Chances are compared exactly, by cross-multiplying.
fn better(tests: &[TestInfo], fails: &[u64], a: usize, b: usize) -> Ordering {
    let (ta, tb) = (&tests[a], &tests[b]);
    let (fa, fb) = (u128::from(fails[a]), u128::from(fails[b]));
    let (ra, rb) = (fa + u128::from(ta.passes), fb + u128::from(tb.passes));
    // a's chance is fa / (ra * ma), b's is fb / (rb * mb).
    let lhs = fa
        .checked_mul(rb)
        .and_then(|x| x.checked_mul(tb.median_eighths));
    let rhs = fb
        .checked_mul(ra)
        .and_then(|x| x.checked_mul(ta.median_eighths));
    let chance = match (lhs, rhs) {
        (Some(l), Some(r)) => r.cmp(&l),
        // Only for absurd counts and durations: compare approximately.
        _ => {
            let c = |f: u128, r: u128, m: u128| f as f64 / r as f64 / m as f64;
            c(fb, rb, tb.median_eighths).total_cmp(&c(fa, ra, ta.median_eighths))
        }
    };
    chance
        .then(ta.median_eighths.cmp(&tb.median_eighths))
        .then_with(|| ta.suite.as_raw().cmp(tb.suite.as_raw()))
        .then_with(|| ta.name.as_raw().cmp(tb.name.as_raw()))
}

/// The confidence after each pick of the cover, as a percentage.
pub fn cover_confidences(analysis: &Analysis, estimator: &dyn Estimator) -> Vec<f64> {
    analysis
        .cover
        .iter()
        .map(|pick| 100.0 * estimator.p_green(analysis.green, pick.rows_left))
        .collect()
}

/// How much of the cover is rich.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    /// The length of the rich prefix of the cover.
    pub rich: usize,
    /// When the threshold was given and no prefix reached it, the highest
    /// confidence (a percentage) that one did.
    pub unreached: Option<f64>,
}

/// Decide the rich prefix of a cover whose confidences, as percentages in
/// pick order, are `confidences`. With no `threshold` the whole cover is
/// rich. Otherwise rich ends at the first pick whose confidence is at
/// least `threshold`; when none is, the whole cover is rich and
/// [`Split::unreached`] says the highest confidence. An empty cover has
/// nothing to split.
pub fn split(confidences: &[f64], threshold: Option<f64>) -> Split {
    let whole = Split {
        rich: confidences.len(),
        unreached: None,
    };
    let Some(threshold) = threshold else {
        return whole;
    };
    match confidences.iter().position(|&c| c >= threshold) {
        Some(i) => Split {
            rich: i + 1,
            unreached: None,
        },
        None => Split {
            unreached: confidences.iter().copied().reduce(f64::max),
            ..whole
        },
    }
}

/// Whether a line's test is to run first or after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Rich,
    Poor,
}

/// One line of `select` output.
#[derive(Debug, Clone, PartialEq)]
pub struct Line<'a> {
    pub class: Class,
    /// Percentage of the matrix rows caught once this test and every line
    /// before it pass.
    pub coverage: f64,
    /// [`Estimator::p_green`] once this test and every line before it pass,
    /// as a percentage.
    pub confidence: f64,
    pub suite: &'a Tainted,
    pub name: &'a Tainted,
}

impl fmt::Display for Line<'_> {
    /// `class<TAB>coverage<TAB>confidence<TAB>suite<TAB>name`. Suite and
    /// name are escaped as [`Tainted`] displays them, so a tab or newline
    /// in either cannot break the line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self.class {
            Class::Rich => "rich",
            Class::Poor => "poor",
        };
        write!(
            f,
            "{class}\t{:.1}\t{:.1}\t{}\t{}",
            self.coverage, self.confidence, self.suite, self.name
        )
    }
}

/// The output lines: the first `rich` picks of the cover in pick order,
/// then poor tests (every other universe test with at least `min_runs`
/// results) by median duration, suite and name. The rest are new and not
/// listed.
pub fn lines<'a>(
    analysis: &'a Analysis,
    estimator: &dyn Estimator,
    rich: usize,
    min_runs: u64,
) -> Vec<Line<'a>> {
    let rows = analysis.rows;
    let line = |class, coverage, confidence, test: &'a TestInfo| Line {
        class,
        coverage,
        confidence,
        suite: &test.suite,
        name: &test.name,
    };
    let mut out: Vec<Line<'a>> = analysis.cover[..rich]
        .iter()
        .map(|pick| {
            let caught = 100.0 * (rows - pick.rows_left) as f64 / rows as f64;
            // Only a cover with no row left prints as 100.0.
            let coverage = if pick.rows_left > 0 {
                caught.min(99.9)
            } else {
                100.0
            };
            let confidence = 100.0 * estimator.p_green(analysis.green, pick.rows_left);
            line(
                Class::Rich,
                coverage,
                confidence,
                &analysis.tests[pick.test],
            )
        })
        .collect();
    let mut is_rich = vec![false; analysis.tests.len()];
    for pick in &analysis.cover[..rich] {
        is_rich[pick.test] = true;
    }
    let mut poor: Vec<&TestInfo> = analysis
        .tests
        .iter()
        .zip(&is_rich)
        .filter(|(t, &rich)| !rich && t.results >= min_runs)
        .map(|(t, _)| t)
        .collect();
    poor.sort_by(|a, b| {
        a.median_eighths
            .cmp(&b.median_eighths)
            .then_with(|| a.suite.as_raw().cmp(b.suite.as_raw()))
            .then_with(|| a.name.as_raw().cmp(b.name.as_raw()))
    });
    // Poor lines come after the whole cover, which leaves no row.
    let confidence = 100.0 * estimator.p_green(analysis.green, 0);
    out.extend(
        poor.into_iter()
            .map(|t| line(Class::Poor, 100.0, confidence, t)),
    );
    out
}

#[cfg(test)]
mod tests;
