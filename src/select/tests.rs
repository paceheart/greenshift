use super::*;

/// `p` as the percentage `select` prints.
fn pct(p: f64) -> String {
    format!("{:.1}", 100.0 * p)
}

#[test]
fn empirical_at_all_rows_left_and_at_none() {
    // 3 green runs, 1 matrix row.
    assert_eq!(Empirical.p_green(3, 1), 0.75);
    assert_eq!(Empirical.p_green(3, 0), 1.0);
    // 1 green run, 2 rows: 1/3.
    assert_eq!(pct(Empirical.p_green(1, 2)), "33.3");
    assert_eq!(Empirical.p_green(1, 0), 1.0);
}

#[test]
fn empirical_with_no_green_run() {
    assert_eq!(Empirical.p_green(0, 4), 0.0);
    // No run at all in which the tests so far passed: 0/0 is taken as 1.
    assert_eq!(Empirical.p_green(0, 0), 1.0);
}

#[test]
fn smoothed_at_all_rows_left_and_at_none() {
    assert_eq!(Smoothed.p_green(3, 1), 4.0 / 6.0);
    assert_eq!(Smoothed.p_green(3, 0), 0.8);
    assert_eq!(Smoothed.p_green(0, 4), 1.0 / 6.0);
    assert_eq!(Smoothed.p_green(0, 0), 0.5);
    assert_eq!(pct(Smoothed.p_green(98, 0)), "99.0");
}

use crate::junit::{Outcome, TestCase};
use crate::store::{NewRun, Store};
use Outcome::{Error as E, Failed as F, Passed as P, Skipped as S};

fn case(suite: &str, name: &str, outcome: Outcome, ms: Option<i64>) -> TestCase {
    TestCase {
        suite: suite.into(),
        name: name.into(),
        outcome,
        duration_ms: ms,
    }
}

/// A test in suite `s` that took `ms` milliseconds.
fn t(name: &str, outcome: Outcome, ms: i64) -> TestCase {
    case("s", name, outcome, Some(ms))
}

/// `n` always-passing tests in suite `filler`, to make runs long enough
/// that dropping a test keeps them full.
fn fillers(n: usize) -> Vec<TestCase> {
    (0..n)
        .map(|i| case("filler", &format!("f{i:02}"), P, Some(1)))
        .collect()
}

fn with(mut cases: Vec<TestCase>, more: &[TestCase]) -> Vec<TestCase> {
    cases.extend_from_slice(more);
    cases
}

fn record(store: &mut Store, source: &str, cases: &[TestCase]) {
    store
        .record_run(
            &NewRun {
                commit: None,
                source,
                files: &[],
            },
            cases,
        )
        .unwrap();
}

fn store_of(runs: &[Vec<TestCase>]) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for cases in runs {
        record(&mut store, "ci", cases);
    }
    store
}

fn analysis(store: &Store, sources: &[&str]) -> Option<Analysis> {
    let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
    analyse(&store.history(&sources).unwrap())
}

/// What `select` prints, and the highest confidence when `threshold` was
/// not reached; `None` for no history.
fn select(
    store: &Store,
    sources: &[&str],
    estimator: &dyn Estimator,
    threshold: Option<f64>,
    min_runs: u64,
) -> Option<(Vec<String>, Option<f64>)> {
    let analysis = analysis(store, sources)?;
    let decided = split(&cover_confidences(&analysis, estimator), threshold);
    let lines = lines(&analysis, estimator, decided.rich, min_runs)
        .iter()
        .map(|l| l.to_string())
        .collect();
    Some((lines, decided.unreached))
}

/// `select` with the defaults but `min_runs`.
fn out(store: &Store, min_runs: u64) -> Vec<String> {
    select(store, &[], &Empirical, None, min_runs).unwrap().0
}

/// The names on rich lines, in order.
fn rich(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.starts_with("rich\t"))
        .map(|l| l.rsplit('\t').next().unwrap())
        .collect()
}

fn median(analysis: &Analysis, name: &str) -> f64 {
    analysis
        .tests
        .iter()
        .find(|t| t.name.as_raw() == name)
        .unwrap()
        .median_ms()
}

#[test]
fn prints_rich_in_pick_order_then_poor_and_leaves_new_out() {
    let store = store_of(&[
        vec![t("a", P, 10), t("b", F, 20), t("c", P, 30)],
        vec![t("a", F, 10), t("b", P, 20), t("c", P, 30)],
        vec![t("a", P, 10), t("b", P, 20), t("c", P, 30)],
    ]);
    // Rows: run 1 (b fails), run 2 (a fails); run 3 is green. a and b both
    // fail once and pass twice; a is quicker, so it goes first.
    assert_eq!(
        out(&store, 3),
        [
            "rich\t50.0\t50.0\ts\ta",
            "rich\t100.0\t100.0\ts\tb",
            "poor\t100.0\t100.0\ts\tc",
        ]
    );
    // Exactly min_runs results is poor; fewer is new, and not listed.
    assert_eq!(out(&store, 4), out(&store, 3)[..2]);
}

#[test]
fn every_universe_test_not_rich_is_still_poor_once_the_cover_is_done() {
    // The one row is covered by the first pick; the rest are poor, though
    // no rows are left to pick them for.
    let store = store_of(&[
        vec![t("a", F, 10), t("b", F, 50), t("c", P, 5)],
        vec![t("a", P, 10), t("b", P, 50), t("c", P, 5)],
    ]);
    assert_eq!(
        out(&store, 2),
        [
            "rich\t100.0\t100.0\ts\ta",
            "poor\t100.0\t100.0\ts\tc",
            "poor\t100.0\t100.0\ts\tb",
        ]
    );
}

#[test]
fn a_pick_drops_every_row_it_fails_in_and_failures_count_only_in_rows_left() {
    let store = store_of(&[
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 10)],
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 10)],
        vec![t("x", F, 10), t("y", P, 10), t("z", P, 10)],
        vec![t("x", P, 10), t("y", P, 10), t("z", F, 10)],
        vec![t("x", P, 10), t("y", P, 10), t("z", P, 10)],
    ]);
    // x: 3 of 5, y: 2 of 5, z: 1 of 5. x takes rows 1-3; y then fails in
    // no row left, so it is never picked, and z covers row 4.
    assert_eq!(
        out(&store, 5),
        [
            "rich\t75.0\t50.0\ts\tx",
            "rich\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ty",
        ]
    );
    let a = analysis(&store, &[]).unwrap();
    assert_eq!((a.rows, a.green), (4, 1));
    assert_eq!(
        a.cover.iter().map(|p| p.rows_left).collect::<Vec<_>>(),
        [1, 0]
    );
}

#[test]
fn failure_chance_is_divided_by_the_median_duration() {
    let store = store_of(&[
        vec![t("a", F, 10), t("b", P, 40), t("c", F, 100)],
        vec![t("a", P, 10), t("b", F, 40), t("c", F, 100)],
    ]);
    // c fails in both rows but is slow: a (1/2 per 10 ms) and then b
    // (1/2 per 40 ms, against c's 1/1 per 100 ms) cover both.
    assert_eq!(rich(&out(&store, 0)), ["a", "b"]);
}

#[test]
fn a_skip_is_neither_a_pass_nor_a_failure() {
    let store = store_of(&[
        vec![t("z", F, 10), t("a", F, 10)],
        vec![t("z", S, 10), t("a", P, 10)],
        vec![t("z", P, 10), t("a", P, 10)],
    ]);
    // z has one pass to a's two, so it fails more often per run and is
    // picked though "a" sorts first; that covers the only row.
    let (lines, _) = select(&store, &[], &Smoothed, None, 0).unwrap();
    // The run where z was skipped is green: G = 2, so (2 + 1) / (2 + 2).
    assert_eq!(
        lines,
        ["rich\t100.0\t75.0\ts\tz", "poor\t100.0\t75.0\ts\ta"]
    );
}

#[test]
fn an_error_counts_as_a_failure() {
    let store = store_of(&[
        vec![t("a", E, 10), t("b", P, 10)],
        vec![t("a", P, 10), t("b", F, 10)],
        vec![t("a", P, 10), t("b", P, 10)],
    ]);
    assert_eq!(
        out(&store, 0),
        ["rich\t50.0\t50.0\ts\ta", "rich\t100.0\t100.0\ts\tb"]
    );
}

#[test]
fn equal_chance_per_second_goes_to_the_shorter_median_first() {
    // z_short: fails 1, passes 1, 10 ms; a_long: fails 1, passes 0 (its
    // first result is a skip), 20 ms. Both are 1/20 per ms.
    let store = store_of(&[
        vec![t("z_short", F, 10), case("s", "a_long", S, None)],
        vec![t("z_short", P, 10), t("a_long", F, 20)],
    ]);
    assert_eq!(rich(&out(&store, 0)), ["z_short", "a_long"]);
}

#[test]
fn equal_chance_and_median_goes_to_the_suite_first() {
    let store = store_of(&[
        vec![case("b", "a", F, Some(10)), case("a", "z", P, Some(10))],
        vec![case("b", "a", P, Some(10)), case("a", "z", F, Some(10))],
    ]);
    assert_eq!(
        out(&store, 0),
        ["rich\t50.0\t0.0\ta\tz", "rich\t100.0\t100.0\tb\ta"]
    );
}

#[test]
fn equal_chance_median_and_suite_goes_to_the_name_in_byte_order() {
    let store = store_of(&[
        vec![t("a", F, 10), t("B", P, 10)],
        vec![t("a", P, 10), t("B", F, 10)],
    ]);
    assert_eq!(rich(&out(&store, 0)), ["B", "a"]);
}

#[test]
fn poor_is_by_median_then_suite_then_name() {
    let store = store_of(&[vec![
        t("a", P, 30),
        t("z", P, 10),
        t("m", P, 10),
        case("a", "z", P, Some(10)),
    ]]);
    assert_eq!(
        out(&store, 1),
        [
            "poor\t100.0\t100.0\ta\tz",
            "poor\t100.0\t100.0\ts\tm",
            "poor\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ta",
        ]
    );
}

#[test]
fn the_cover_stops_at_exact_coverage_not_a_rounded_percentage() {
    // a fails in 2000 rows and b in 1. After a, 2000/2001 = 99.95% of the
    // rows are covered, which would round to 100.0; b is still picked, and
    // only the line that leaves no row says 100.0.
    let mut runs = vec![vec![t("a", F, 10), t("b", P, 10)]; 2000];
    runs.push(vec![t("a", P, 10), t("b", F, 10)]);
    let store = store_of(&runs);
    assert_eq!(
        out(&store, 0),
        ["rich\t99.9\t0.0\ts\ta", "rich\t100.0\t100.0\ts\tb"]
    );
}

#[test]
fn the_first_run_is_full() {
    let store = store_of(&[vec![t("a", F, 10)]]);
    assert_eq!(out(&store, 0), ["rich\t100.0\t100.0\ts\ta"]);
}

#[test]
fn the_universe_is_the_newest_full_run() {
    let store = store_of(&[
        with(fillers(10), &[t("old", F, 10)]),
        with(fillers(10), &[t("new", P, 10)]),
    ]);
    let lines = out(&store, 0);
    assert!(lines.iter().any(|l| l.ends_with("\ts\tnew")), "{lines:?}");
    // old is not in the universe, so its failure makes no row: run 1 is
    // green.
    assert!(!lines.iter().any(|l| l.contains("old")), "{lines:?}");
    assert_eq!(rich(&lines), Vec::<&str>::new());
    let a = analysis(&store, &[]).unwrap();
    assert_eq!((a.rows, a.green, a.tests.len()), (0, 2, 11));
}

#[test]
fn a_short_run_is_not_full() {
    let store = store_of(&[
        with(fillers(10), &[t("a", P, 10)]),
        vec![t("a", F, 10), t("only_short", F, 10)],
    ]);
    // Run 2 has 2 results, under 90% of 11: not a row, not the universe.
    let lines = out(&store, 0);
    assert_eq!(rich(&lines), Vec::<&str>::new());
    assert!(!lines.iter().any(|l| l.contains("only_short")), "{lines:?}");
    let a = analysis(&store, &[]).unwrap();
    assert_eq!((a.rows, a.green, a.tests.len()), (0, 1, 11));
    // Its results still count towards min-runs: a has 2, the fillers 1.
    assert!(out(&store, 2).iter().all(|l| l.ends_with("\ts\ta")));
}

#[test]
fn ninety_percent_of_the_largest_is_full_and_less_is_not() {
    // 9 of 10 is full.
    let store = store_of(&[fillers(10), with(fillers(8), &[t("a", F, 10)])]);
    assert_eq!(rich(&out(&store, 0)), ["a"]);
    // 8 of 10 is not.
    let store = store_of(&[fillers(10), with(fillers(7), &[t("a", F, 10)])]);
    assert_eq!(rich(&out(&store, 0)), Vec::<&str>::new());
}

#[test]
fn fullness_compares_with_only_the_three_full_runs_before() {
    // 17 is under 90% of the first run's 20 but not of the 18s after it.
    let store = store_of(&[
        fillers(20),
        fillers(18),
        fillers(18),
        fillers(18),
        with(fillers(16), &[t("a", F, 10)]),
    ]);
    assert_eq!(rich(&out(&store, 0)), ["a"]);
}

#[test]
fn short_runs_are_not_among_the_runs_compared_with() {
    // Runs 2-5 are all short against run 1. Were short runs compared
    // with, run 5 would be full against runs 2-4.
    let store = store_of(&[
        fillers(20),
        fillers(10),
        fillers(10),
        fillers(10),
        with(fillers(9), &[t("a", F, 10)]),
    ]);
    assert_eq!(rich(&out(&store, 0)), Vec::<&str>::new());
    assert_eq!(analysis(&store, &[]).unwrap().tests.len(), 20);
}

#[test]
fn median_is_of_the_last_five_results_with_a_duration() {
    let mut runs: Vec<Vec<TestCase>> = [1000, 1000, 1000, 1, 2, 3, 4, 5]
        .iter()
        .map(|&ms| vec![t("a", P, ms), t("b", P, 10)])
        .collect();
    // A result without a duration does not count as one of the five.
    runs.push(vec![case("s", "a", P, None), t("b", P, 20)]);
    let store = store_of(&runs);
    let a = analysis(&store, &[]).unwrap();
    assert_eq!(median(&a, "a"), 3.0);
    // b's last five: 10 four times, then 20.
    assert_eq!(median(&a, "b"), 10.0);
    // Even count: the mean of the middle two.
    let store = store_of(&[vec![t("c", P, 10)], vec![t("c", P, 25)]]);
    assert_eq!(median(&analysis(&store, &[]).unwrap(), "c"), 17.5);
}

#[test]
fn a_test_with_no_duration_gets_the_median_of_the_medians() {
    let none = |name| case("s", name, P, None);
    let store = store_of(&[vec![t("p", P, 10), t("q", P, 30), none("r")]]);
    assert_eq!(median(&analysis(&store, &[]).unwrap(), "r"), 20.0);
    let store = store_of(&[vec![t("p", P, 10), t("q", P, 30), t("s", P, 90), none("r")]]);
    assert_eq!(median(&analysis(&store, &[]).unwrap(), "r"), 30.0);
    // With no duration anywhere, everything is 1 ms.
    let store = store_of(&[vec![none("r"), none("s")]]);
    assert_eq!(median(&analysis(&store, &[]).unwrap(), "r"), 1.0);
}

#[test]
fn zero_milliseconds_counts_as_one() {
    let store = store_of(&[vec![t("a", P, 0)], vec![t("a", P, 4)]]);
    // (1 + 4) / 2, not (0 + 4) / 2.
    assert_eq!(median(&analysis(&store, &[]).unwrap(), "a"), 2.5);
    // b0 at 0 ms ties a1 at 1 ms on everything but the name.
    let store = store_of(&[
        vec![t("b0", F, 0), t("a1", P, 1)],
        vec![t("b0", P, 0), t("a1", F, 1)],
    ]);
    assert_eq!(rich(&out(&store, 0)), ["a1", "b0"]);
}

#[test]
fn no_full_run_is_no_history() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(analysis(&store, &[]), None);
    let store = store_of(&[vec![t("a", P, 1)]]);
    assert_eq!(analysis(&store, &["elsewhere"]), None);
}

#[test]
fn source_keeps_only_the_given_sources_runs() {
    let mut store = Store::open_in_memory().unwrap();
    record(&mut store, "ci", &[t("a", F, 10), t("b", P, 10)]);
    record(&mut store, "local", &[t("a", P, 10), t("b", F, 10)]);
    record(&mut store, "ci", &[t("a", P, 10), t("b", P, 10)]);
    record(&mut store, "local", &[t("a", P, 10), t("b", P, 10)]);
    let rich_from = |sources: &[&str]| {
        let (lines, _) = select(&store, sources, &Empirical, None, 0).unwrap();
        rich(&lines)
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(rich_from(&["ci"]), ["a"]);
    assert_eq!(rich_from(&["local"]), ["b"]);
    assert_eq!(rich_from(&["ci", "local"]), ["a", "b"]);
    assert_eq!(rich_from(&[]), ["a", "b"]);
}

#[test]
fn fullness_is_judged_among_the_chosen_sources_runs() {
    let mut store = Store::open_in_memory().unwrap();
    record(&mut store, "big", &fillers(20));
    record(&mut store, "small", &[t("a", F, 10), t("b", P, 10)]);
    // Against the big run the small one is short.
    assert_eq!(rich(&out(&store, 0)), Vec::<&str>::new());
    let (lines, _) = select(&store, &["small"], &Empirical, None, 0).unwrap();
    assert_eq!(rich(&lines), ["a"]);
}

#[test]
fn suite_and_name_are_printed_escaped() {
    let store = store_of(&[vec![case("s\tx", "n\n\u{1b}[31m", P, Some(1))]]);
    assert_eq!(
        out(&store, 1),
        ["poor\t100.0\t100.0\ts\\tx\tn\\n\\u{1b}[31m"]
    );
}

#[test]
fn split_without_a_threshold_is_the_whole_cover() {
    assert_eq!(
        split(&[50.0, 80.0, 100.0], None),
        Split {
            rich: 3,
            unreached: None
        }
    );
}

#[test]
fn split_ends_rich_at_the_first_pick_at_or_over_the_threshold() {
    let c = [50.0, 80.0, 95.0, 100.0];
    let rich_at = |n| split(&c, Some(n));
    assert_eq!(
        rich_at(80.0),
        Split {
            rich: 2,
            unreached: None
        }
    );
    assert_eq!(
        rich_at(79.9),
        Split {
            rich: 2,
            unreached: None
        }
    );
    assert_eq!(
        rich_at(80.1),
        Split {
            rich: 3,
            unreached: None
        }
    );
    assert_eq!(
        rich_at(0.0),
        Split {
            rich: 1,
            unreached: None
        }
    );
    assert_eq!(
        rich_at(100.0),
        Split {
            rich: 4,
            unreached: None
        }
    );
}

#[test]
fn split_that_never_reaches_the_threshold_is_the_whole_cover() {
    assert_eq!(
        split(&[40.0, 60.0, 75.0], Some(90.0)),
        Split {
            rich: 3,
            unreached: Some(75.0)
        }
    );
    assert_eq!(
        split(&[], Some(90.0)),
        Split {
            rich: 0,
            unreached: None
        }
    );
}

#[test]
fn smoothed_confidence_can_leave_a_threshold_unreached() {
    let store = store_of(&[vec![t("a", F, 10)], vec![t("a", P, 10)]]);
    // G = 1, so the most smoothed reaches is (1 + 1) / (1 + 0 + 2).
    let (lines, unreached) = select(&store, &[], &Smoothed, Some(99.0), 0).unwrap();
    assert_eq!(lines, ["rich\t100.0\t66.7\ts\ta"]);
    assert_eq!(
        unreached.map(|c| format!("{c:.1}")).as_deref(),
        Some("66.7")
    );
}

#[test]
fn the_cover_after_the_split_is_poor_or_new() {
    // As in a_pick_drops_every_row_it_fails_in..., but z is quicker, and
    // a short last run gives y one more result than the others.
    let store = store_of(&[
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 5)],
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 5)],
        vec![t("x", F, 10), t("y", P, 10), t("z", P, 5)],
        vec![t("x", P, 10), t("y", P, 10), t("z", F, 5)],
        vec![t("x", P, 10), t("y", P, 10), t("z", P, 5)],
        vec![t("y", P, 10)],
    ]);
    let run = |threshold, min_runs| {
        select(&store, &[], &Empirical, threshold, min_runs)
            .unwrap()
            .0
    };
    assert_eq!(
        run(None, 5),
        [
            "rich\t75.0\t50.0\ts\tx",
            "rich\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ty",
        ]
    );
    // x alone reaches 50%: z joins the poor, by median.
    assert_eq!(
        run(Some(50.0), 5),
        [
            "rich\t75.0\t50.0\ts\tx",
            "poor\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ty",
        ]
    );
    // With 6 results needed, z (5) is new and y (6) still poor.
    assert_eq!(
        run(Some(50.0), 6),
        ["rich\t75.0\t50.0\ts\tx", "poor\t100.0\t100.0\ts\ty"]
    );
}
