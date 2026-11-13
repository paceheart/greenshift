use super::*;

/// `p` as the percentage `select` prints.
fn pct(p: f64) -> String {
    format!("{:.1}", 100.0 * p)
}

#[test]
fn wilson_with_every_run_green() {
    // R = 0: n green runs and no row left.
    for (n, want) in [
        (1, "20.7"),
        (5, "56.6"),
        (10, "72.2"),
        (20, "83.9"),
        (50, "92.9"),
        (100, "96.3"),
        (200, "98.1"),
    ] {
        assert_eq!(pct(Wilson.p_green(n, 0)), want, "n = {n}");
    }
}

#[test]
fn wilson_with_three_rows_left() {
    // R = 3 of n runs, G = n - 3.
    for (n, want) in [
        (5, "11.8"),
        (10, "39.7"),
        (20, "64.0"),
        (50, "83.8"),
        (100, "91.5"),
        (200, "95.7"),
    ] {
        assert_eq!(pct(Wilson.p_green(n - 3, 3)), want, "n = {n}");
    }
}

#[test]
fn wilson_with_no_green_run_is_zero() {
    assert_eq!(Wilson.p_green(0, 4), 0.0);
    assert_eq!(Wilson.p_green(0, 0), 0.0);
}

#[test]
fn wilson_at_all_rows_left_and_at_none() {
    // 3 green runs, 1 matrix row: from n = 4 with a red to n = 3 all green.
    assert_eq!(pct(Wilson.p_green(3, 1)), "30.1");
    assert_eq!(pct(Wilson.p_green(3, 0)), "43.8");
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

/// What `select` prints, and the thresholds not reached; `None` for no
/// history.
fn select(
    store: &Store,
    sources: &[&str],
    estimator: &dyn Estimator,
    thresholds: Thresholds,
    min_runs: u64,
) -> Option<(Vec<String>, Vec<Unreached>)> {
    let analysis = analysis(store, sources)?;
    let decided = split(
        &cover_coverages(&analysis),
        &cover_confidences(&analysis, estimator),
        thresholds,
    );
    let lines = lines(&analysis, estimator, decided.rich, min_runs)
        .iter()
        .map(|l| l.to_string())
        .collect();
    Some((lines, decided.unreached))
}

const DEFAULT: Thresholds = Thresholds {
    coverage: None,
    confidence: None,
};

fn confidence(n: f64) -> Thresholds {
    Thresholds {
        confidence: Some(n),
        ..DEFAULT
    }
}

fn coverage(n: f64) -> Thresholds {
    Thresholds {
        coverage: Some(n),
        ..DEFAULT
    }
}

fn both(coverage: f64, confidence: f64) -> Thresholds {
    Thresholds {
        coverage: Some(coverage),
        confidence: Some(confidence),
    }
}

/// `select` with the defaults but `min_runs`.
fn out(store: &Store, min_runs: u64) -> Vec<String> {
    select(store, &[], &Empirical, DEFAULT, min_runs).unwrap().0
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
    let (lines, _) = select(&store, &[], &Smoothed, DEFAULT, 0).unwrap();
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
        let (lines, _) = select(&store, sources, &Empirical, DEFAULT, 0).unwrap();
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
    let (lines, _) = select(&store, &["small"], &Empirical, DEFAULT, 0).unwrap();
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

/// A cover of four picks: coverage and confidence after each.
const COV: [f64; 4] = [40.0, 70.0, 90.0, 100.0];
const CONF: [f64; 4] = [50.0, 80.0, 85.0, 95.0];

fn rich_at(thresholds: Thresholds) -> usize {
    let decided = split(&COV, &CONF, thresholds);
    assert_eq!(decided.unreached, [], "{thresholds:?}");
    decided.rich
}

#[test]
fn split_without_a_threshold_is_the_whole_cover() {
    assert_eq!(rich_at(DEFAULT), 4);
    assert_eq!(rich_at(DEFAULT), rich_at(coverage(100.0)));
}

#[test]
fn split_ends_rich_at_the_first_pick_at_or_over_the_coverage() {
    assert_eq!(rich_at(coverage(70.0)), 2);
    assert_eq!(rich_at(coverage(69.9)), 2);
    assert_eq!(rich_at(coverage(70.1)), 3);
    assert_eq!(rich_at(coverage(0.0)), 1);
    assert_eq!(rich_at(coverage(100.0)), 4);
}

#[test]
fn split_ends_rich_at_the_first_pick_at_or_over_the_confidence() {
    assert_eq!(rich_at(confidence(80.0)), 2);
    assert_eq!(rich_at(confidence(79.9)), 2);
    assert_eq!(rich_at(confidence(80.1)), 3);
    assert_eq!(rich_at(confidence(0.0)), 1);
    assert_eq!(rich_at(confidence(95.0)), 4);
}

#[test]
fn split_with_both_thresholds_ends_where_the_later_is_reached() {
    // Coverage is reached later.
    assert_eq!(rich_at(both(90.0, 50.0)), 3);
    // Confidence is reached later.
    assert_eq!(rich_at(both(40.0, 85.0)), 3);
    // Both at the same pick.
    assert_eq!(rich_at(both(70.0, 80.0)), 2);
    // Never fewer than either alone.
    for (cov, conf) in [(40.0, 95.0), (100.0, 0.0), (60.0, 81.0)] {
        let r = rich_at(both(cov, conf));
        assert!(r >= rich_at(coverage(cov)) && r >= rich_at(confidence(conf)));
    }
}

#[test]
fn split_that_never_reaches_the_confidence_is_the_whole_cover() {
    assert_eq!(
        split(&COV, &CONF, confidence(99.0)),
        Split {
            rich: 4,
            unreached: vec![Unreached {
                measure: Measure::Confidence,
                wanted: 99.0,
                highest: 95.0
            }]
        }
    );
    // With a coverage that is reached, still the whole cover.
    let decided = split(&COV, &CONF, both(40.0, 99.0));
    assert_eq!(decided.rich, 4);
    assert_eq!(decided.unreached.len(), 1);
    assert_eq!(decided.unreached[0].measure, Measure::Confidence);
}

#[test]
fn split_that_never_reaches_the_coverage_is_the_whole_cover() {
    // A real cover always ends at 100% coverage; split does not rely on it.
    let short = [40.0, 70.0];
    let decided = split(&short, &CONF[..2], both(90.0, 99.0));
    assert_eq!(
        decided,
        Split {
            rich: 2,
            unreached: vec![
                Unreached {
                    measure: Measure::Coverage,
                    wanted: 90.0,
                    highest: 70.0
                },
                Unreached {
                    measure: Measure::Confidence,
                    wanted: 99.0,
                    highest: 80.0
                },
            ]
        }
    );
    let decided = split(&short, &CONF[..2], coverage(90.0));
    assert_eq!((decided.rich, decided.unreached.len()), (2, 1));
}

#[test]
fn an_empty_cover_has_nothing_to_split() {
    for thresholds in [DEFAULT, coverage(50.0), confidence(99.0), both(1.0, 1.0)] {
        assert_eq!(
            split(&[], &[], thresholds),
            Split {
                rich: 0,
                unreached: vec![]
            }
        );
    }
}

#[test]
fn coverage_is_exactly_100_only_with_no_row_left() {
    // As in the_cover_stops_at_exact_coverage...: 2000 of 2001 rows first.
    let mut runs = vec![vec![t("a", F, 10), t("b", P, 10)]; 2000];
    runs.push(vec![t("a", P, 10), t("b", F, 10)]);
    let store = store_of(&runs);
    let a = analysis(&store, &[]).unwrap();
    let c = cover_coverages(&a);
    assert_eq!(c.len(), 2);
    assert!(c[0] < 100.0 && c[0] > 99.95, "{c:?}");
    assert_eq!(c[1], 100.0);
    let decided = split(&c, &cover_confidences(&a, &Empirical), coverage(100.0));
    assert_eq!(decided.rich, 2);
    let decided = split(&c, &cover_confidences(&a, &Empirical), coverage(99.95));
    assert_eq!(decided.rich, 1);
}

#[test]
fn coverage_ends_rich_early() {
    let store = store_of(&[
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 10)],
        vec![t("x", F, 10), t("y", F, 10), t("z", P, 10)],
        vec![t("x", F, 10), t("y", P, 10), t("z", P, 10)],
        vec![t("x", P, 10), t("y", P, 10), t("z", F, 10)],
        vec![t("x", P, 10), t("y", P, 10), t("z", P, 10)],
    ]);
    // x covers 75% of the rows; z, now poor, sorts by median with y.
    let (lines, unreached) = select(&store, &[], &Empirical, coverage(75.0), 5).unwrap();
    assert_eq!(
        lines,
        [
            "rich\t75.0\t50.0\ts\tx",
            "poor\t100.0\t100.0\ts\ty",
            "poor\t100.0\t100.0\ts\tz",
        ]
    );
    assert_eq!(unreached, []);
    let (lines, _) = select(&store, &[], &Empirical, coverage(75.1), 5).unwrap();
    assert_eq!(rich(&lines), ["x", "z"]);
}

#[test]
fn smoothed_confidence_can_leave_a_threshold_unreached() {
    let store = store_of(&[vec![t("a", F, 10)], vec![t("a", P, 10)]]);
    // G = 1, so the most smoothed reaches is (1 + 1) / (1 + 0 + 2).
    let (lines, unreached) = select(&store, &[], &Smoothed, confidence(99.0), 0).unwrap();
    assert_eq!(lines, ["rich\t100.0\t66.7\ts\ta"]);
    assert_eq!(unreached.len(), 1);
    assert_eq!(unreached[0].measure, Measure::Confidence);
    assert_eq!(format!("{:.1}", unreached[0].highest), "66.7");
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
    let run = |thresholds, min_runs| {
        select(&store, &[], &Empirical, thresholds, min_runs)
            .unwrap()
            .0
    };
    assert_eq!(
        run(DEFAULT, 5),
        [
            "rich\t75.0\t50.0\ts\tx",
            "rich\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ty",
        ]
    );
    // x alone reaches 50%: z joins the poor, by median.
    assert_eq!(
        run(confidence(50.0), 5),
        [
            "rich\t75.0\t50.0\ts\tx",
            "poor\t100.0\t100.0\ts\tz",
            "poor\t100.0\t100.0\ts\ty",
        ]
    );
    // With 6 results needed, z (5) is new and y (6) still poor.
    assert_eq!(
        run(confidence(50.0), 6),
        ["rich\t75.0\t50.0\ts\tx", "poor\t100.0\t100.0\ts\ty"]
    );
}
