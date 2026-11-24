//! EMA (Exponential Moving Average) transformer (spec F4).
//!
//! `EMA = previous EMA + k × (close − previous EMA)`, with `k = 2 / (period + 1)`.
//! The first value is the plain mean of the first `period` closes (seed); no
//! value before (warm-up: `period` candles). Computed in `Decimal`.

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::{Derived, Transformer};

/// Item produced by the EMA transformer: the candle and its EMA.
pub type EmaCandle = Derived<Candle, Decimal>;

/// EMA calculation, usable on its own (e.g. by other conditions).
#[derive(Debug, Clone, PartialEq)]
pub struct EmaCalc {
    period: usize,
    k: Decimal,
    value: Option<Decimal>,
    seed: Vec<Decimal>,
}

impl EmaCalc {
    /// EMA over `period` values. `period` must be at least 1.
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        if period == 0 {
            return Err(ParameterError::Invalid("EMA period must be >= 1".into()));
        }
        Ok(Self {
            period,
            k: Decimal::TWO / Decimal::from(period + 1),
            value: None,
            seed: Vec::with_capacity(period),
        })
    }

    /// EMA over `period.max(1)` values (never fails).
    pub(crate) fn clamped(period: usize) -> Self {
        let period = period.max(1);
        Self {
            period,
            k: Decimal::TWO / Decimal::from(period + 1),
            value: None,
            seed: Vec::with_capacity(period),
        }
    }

    /// Period of the EMA.
    pub fn period(&self) -> usize {
        self.period
    }

    /// Add a value; returns the EMA, or `None` during the warm-up.
    pub fn update(&mut self, x: Decimal) -> Option<Decimal> {
        let ema = match self.value {
            Some(prev) => prev + self.k * (x - prev),
            None => {
                self.seed.push(x);
                if self.seed.len() < self.period {
                    return None;
                }
                let mean = self.seed.iter().sum::<Decimal>() / Decimal::from(self.period);
                self.seed = Vec::new();
                mean
            }
        };
        self.value = Some(ema);
        Some(ema)
    }

    /// Last EMA value.
    pub fn value(&self) -> Option<Decimal> {
        self.value
    }
}

/// EMA of the closes.
#[derive(Debug, Clone)]
pub struct Ema(EmaCalc);

impl Ema {
    /// EMA over `period` candles. `period` must be at least 1.
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        EmaCalc::new(period).map(Self)
    }
}

impl Transformer for Ema {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("ema({})", self.0.period())
    }

    fn warm_up(&self) -> usize {
        self.0.period()
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        self.0.update(item.data.close)
    }

    fn on_modifier(&mut self, modifier: NoModifier) -> Result<(), ModifierError> {
        match modifier {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn run(period: usize, xs: &[Decimal]) -> Vec<Option<Decimal>> {
        let mut e = EmaCalc::new(period).unwrap();
        xs.iter().map(|&x| e.update(x)).collect()
    }

    #[test]
    fn hand_computed_sequence() {
        // Period 3: k = 0.5. Seed: mean(1, 2, 3) = 2.
        // 4 -> 2 + 0.5 x (4 - 2) = 3; 5 -> 3 + 0.5 x (5 - 3) = 4; 2 -> 4 + 0.5 x (2 - 4) = 3.
        assert_eq!(
            run(3, &[dec!(1), dec!(2), dec!(3), dec!(4), dec!(5), dec!(2)]),
            vec![
                None,
                None,
                Some(dec!(2)),
                Some(dec!(3)),
                Some(dec!(4)),
                Some(dec!(3))
            ]
        );
    }

    #[test]
    fn period_4_weights() {
        // k = 2 / 5 = 0.4. Seed mean(10, 10, 10, 10) = 10; 20 -> 14; 20 -> 14 + 0.4 x 6 = 16.4.
        assert_eq!(
            run(
                4,
                &[dec!(10), dec!(10), dec!(10), dec!(10), dec!(20), dec!(20)]
            ),
            vec![
                None,
                None,
                None,
                Some(dec!(10)),
                Some(dec!(14)),
                Some(dec!(16.4))
            ]
        );
    }

    #[test]
    fn constant_series_stays_constant_and_period_1_is_the_value() {
        assert!(
            run(5, &[dec!(7); 9])
                .iter()
                .skip(4)
                .all(|v| *v == Some(dec!(7)))
        );
        assert_eq!(
            run(1, &[dec!(3), dec!(8)]),
            vec![Some(dec!(3)), Some(dec!(8))]
        );
    }

    #[test]
    fn rejects_zero_period_and_declares_its_warm_up() {
        assert!(Ema::new(0).is_err());
        let ema = Ema::new(20).unwrap();
        assert_eq!(ema.warm_up(), 20);
        assert_eq!(ema.name(), "ema(20)");
    }
}
