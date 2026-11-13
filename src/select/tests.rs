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
