//! Choosing tests from history: what `greenshift select` prints.
//!
//! Everything is computed from [`History`] first ([`analyse`]); deciding
//! how much of the cover counts as rich ([`split`]) and rendering lines
//! ([`lines`]) are separate steps over those numbers.

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

#[cfg(test)]
mod tests;
