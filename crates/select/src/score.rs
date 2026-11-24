//! Scoring of a genome over several months (spec F11).
//!
//! - [`period_score`]: the score of one month from its gain and its
//!   activity (positions per day): `gain − penalty × (1 − activity)`, the
//!   activity being 1 between `low` and `high` positions per day (the
//!   target, e.g. 1 – 2), rising linearly from 0 below, decreasing as
//!   `high / rate` above. No trade at all costs the full penalty.
//! - [`balanced_folds`]: train / test splits of the months; built in pairs
//!   (A/B, then B/A) so that each month is used as often for training as
//!   for testing.
//! - [`cross_score`]: the mean over the folds of
//!   `mean(train) − gap_weight × |mean(train) − mean(test)|`: it rewards
//!   the training score and penalises the difference with the test months.

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::rng::Rng;

/// Target activity of the month score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Activity {
    /// Lowest number of positions per day of the target band (> 0).
    pub low: f64,
    /// Highest number of positions per day of the target band (>= low).
    pub high: f64,
    /// Score lost by a month without any position (gain points).
    pub penalty: Decimal,
}

impl Default for Activity {
    /// 1 – 2 positions per day, penalty 20.
    fn default() -> Self {
        Self {
            low: 1.0,
            high: 2.0,
            penalty: Decimal::from(20),
        }
    }
}

impl Activity {
    /// Activity factor in [0, 1] of `rate` positions per day.
    pub fn factor(&self, rate: f64) -> f64 {
        if rate <= 0.0 || self.low <= 0.0 {
            0.0
        } else if rate < self.low {
            rate / self.low
        } else if rate <= self.high {
            1.0
        } else {
            self.high / rate
        }
    }
}

/// Score of one month: `gain − penalty × (1 − activity)` with `positions`
/// opened over `days` days.
pub fn period_score(gain: Decimal, positions: usize, days: f64, target: &Activity) -> Decimal {
    let rate = if days > 0.0 {
        positions as f64 / days
    } else {
        0.0
    };
    let missing = Decimal::from_f64(1.0 - target.factor(rate)).unwrap_or(Decimal::ONE);
    gain - target.penalty * missing
}

/// One split of the months: indices of the training and test months.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// Training months.
    pub train: Vec<usize>,
    /// Test months.
    pub test: Vec<usize>,
}

/// `2 × pairs` folds over `n` months (n even, >= 2): each pair is a random
/// half A and the other half B, giving the folds (A, B) and (B, A). The
/// pairs have different halves when possible. Deterministic for `seed`.
pub fn balanced_folds(n: usize, pairs: usize, seed: u64) -> Result<Vec<Fold>, String> {
    if n < 2 || !n.is_multiple_of(2) {
        return Err("folds: an even number of months (>= 2) is needed".into());
    }
    let mut rng = Rng::new(seed);
    let mut halves: Vec<Vec<usize>> = Vec::new();
    let mut folds = Vec::new();
    for _ in 0..pairs {
        let mut a = Vec::new();
        for _attempt in 0..100 {
            let mut idx: Vec<usize> = (0..n).collect();
            for i in (1..n).rev() {
                idx.swap(i, rng.below(i + 1));
            }
            a = idx[..n / 2].to_vec();
            a.sort_unstable();
            let b: Vec<usize> = (0..n).filter(|i| !a.contains(i)).collect();
            if !halves.contains(&a) && !halves.contains(&b) {
                break;
            }
        }
        let b: Vec<usize> = (0..n).filter(|i| !a.contains(i)).collect();
        halves.push(a.clone());
        halves.push(b.clone());
        folds.push(Fold {
            train: a.clone(),
            test: b.clone(),
        });
        folds.push(Fold { train: b, test: a });
    }
    Ok(folds)
}

fn mean(xs: impl Iterator<Item = Decimal>) -> Decimal {
    let (sum, n) = xs.fold((Decimal::ZERO, 0u64), |(s, n), x| (s + x, n + 1));
    if n == 0 {
        Decimal::ZERO
    } else {
        sum / Decimal::from(n)
    }
}

/// Score of one fold: `mean(train) − gap_weight × |mean(train) − mean(test)|`.
pub fn fold_score(scores: &[Decimal], fold: &Fold, gap_weight: Decimal) -> Decimal {
    let pick = |ids: &[usize]| mean(ids.iter().filter_map(|&i| scores.get(i).copied()));
    let (train, test) = (pick(&fold.train), pick(&fold.test));
    train - gap_weight * (train - test).abs()
}

/// Mean of the fold scores (the fitness of a genome).
pub fn cross_score(scores: &[Decimal], folds: &[Fold], gap_weight: Decimal) -> Decimal {
    mean(folds.iter().map(|f| fold_score(scores, f, gap_weight)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn one_or_two_trades_per_day_score_best() {
        let t = Activity::default(); // 1 - 2 per day, penalty 20
        // 30 days: 30 to 60 positions, no penalty.
        assert_eq!(period_score(dec!(10), 30, 30.0, &t), dec!(10));
        assert_eq!(period_score(dec!(10), 60, 30.0, &t), dec!(10));
        // Half the target (15): half the penalty.
        assert_eq!(period_score(dec!(10), 15, 30.0, &t), dec!(0));
        // No trade: the full penalty.
        assert_eq!(period_score(dec!(0), 0, 30.0, &t), dec!(-20));
        // 4 per day (120): factor 0.5, penalty 10.
        assert_eq!(period_score(dec!(10), 120, 30.0, &t), dec!(0));
        // More trades towards the target increase the score.
        let s = |n| period_score(dec!(5), n, 30.0, &t);
        assert!(s(0) < s(5) && s(5) < s(20) && s(20) < s(30));
        // Losses count in full.
        assert_eq!(period_score(dec!(-12), 45, 30.0, &t), dec!(-12));
    }

    #[test]
    fn folds_are_balanced_and_different() {
        let folds = balanced_folds(12, 2, 7).unwrap();
        assert_eq!(folds.len(), 4);
        for f in &folds {
            assert_eq!((f.train.len(), f.test.len()), (6, 6));
            let mut all: Vec<usize> = f.train.iter().chain(&f.test).copied().collect();
            all.sort_unstable();
            assert_eq!(all, (0..12).collect::<Vec<_>>());
        }
        for m in 0..12 {
            let trained = folds.iter().filter(|f| f.train.contains(&m)).count();
            assert_eq!(trained, 2, "month {m}");
        }
        // The training sets are all different.
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(folds[i].train, folds[j].train);
            }
        }
        assert_eq!(folds, balanced_folds(12, 2, 7).unwrap(), "deterministic");
        assert!(balanced_folds(5, 2, 1).is_err());
    }

    #[test]
    fn the_gap_between_train_and_test_is_penalised() {
        let fold = Fold {
            train: vec![0, 1],
            test: vec![2, 3],
        };
        // train mean 10, test mean 4: 10 - 1 x 6 = 4.
        let scores = [dec!(12), dec!(8), dec!(5), dec!(3)];
        assert_eq!(fold_score(&scores, &fold, dec!(1)), dec!(4));
        assert_eq!(fold_score(&scores, &fold, dec!(0)), dec!(10));
        // Consistent months: no penalty.
        let even = [dec!(5); 4];
        assert_eq!(fold_score(&even, &fold, dec!(1)), dec!(5));
        let swapped = Fold {
            train: vec![2, 3],
            test: vec![0, 1],
        };
        // (4 + (4 - 6)) / 2 = 1.
        assert_eq!(cross_score(&scores, &[fold, swapped], dec!(1)), dec!(1));
    }
}
