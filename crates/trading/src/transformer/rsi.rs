//! RSI (Relative Strength Index) transformer (spec F4).
//!
//! Input: candles. Output: each candle with its RSI (0..=100), computed on
//! closes with Wilder smoothing. No value during warm-up (`period + 1` candles).

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::{Derived, Transformer};

/// Default RSI period.
pub const DEFAULT_PERIOD: usize = 14;

/// Item produced by the RSI transformed source: the candle and its RSI.
pub type RsiCandle = Derived<Candle, Decimal>;

/// RSI transformer.
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    prev_close: Option<Decimal>,
    /// Number of close-to-close changes seen so far.
    changes: usize,
    /// Average gain / loss (plain sums until `period` changes are seen).
    avg_gain: Decimal,
    avg_loss: Decimal,
}

impl Rsi {
    /// Create an RSI over `period` candles. `period` must be at least 1.
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        if period == 0 {
            return Err(ParameterError::Invalid("RSI period must be >= 1".into()));
        }
        Ok(Self {
            period,
            prev_close: None,
            changes: 0,
            avg_gain: Decimal::ZERO,
            avg_loss: Decimal::ZERO,
        })
    }

    fn rsi(&self) -> Decimal {
        let hundred = Decimal::ONE_HUNDRED;
        if self.avg_loss.is_zero() {
            return if self.avg_gain.is_zero() {
                Decimal::from(50)
            } else {
                hundred
            };
        }
        let rs = self.avg_gain / self.avg_loss;
        hundred - hundred / (Decimal::ONE + rs)
    }
}

impl Default for Rsi {
    fn default() -> Self {
        Self {
            period: DEFAULT_PERIOD,
            prev_close: None,
            changes: 0,
            avg_gain: Decimal::ZERO,
            avg_loss: Decimal::ZERO,
        }
    }
}

impl Transformer for Rsi {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("rsi({})", self.period)
    }

    fn warm_up(&self) -> usize {
        self.period + 1
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        let candle = item.data;
        let prev = self.prev_close.replace(candle.close)?;
        let change = candle.close - prev;
        let (gain, loss) = if change > Decimal::ZERO {
            (change, Decimal::ZERO)
        } else {
            (Decimal::ZERO, -change)
        };

        self.changes += 1;
        let period = Decimal::from(self.period);
        if self.changes < self.period {
            // Seeding: accumulate sums.
            self.avg_gain += gain;
            self.avg_loss += loss;
            None
        } else if self.changes == self.period {
            // First average: plain mean of the first `period` changes.
            self.avg_gain = (self.avg_gain + gain) / period;
            self.avg_loss = (self.avg_loss + loss) / period;
            Some(self.rsi())
        } else {
            // Wilder smoothing.
            let keep = period - Decimal::ONE;
            self.avg_gain = (self.avg_gain * keep + gain) / period;
            self.avg_loss = (self.avg_loss * keep + loss) / period;
            Some(self.rsi())
        }
    }

    fn on_modifier(&mut self, modifier: NoModifier) -> Result<(), ModifierError> {
        match modifier {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;

    fn candles(closes: &[Decimal]) -> Vec<Candle> {
        closes
            .iter()
            .zip(0..)
            .map(|(&close, i)| Candle {
                open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
                open: close,
                high: close,
                low: close,
                close,
                volume: Decimal::ONE,
            })
            .collect()
    }

    fn run(rsi: &mut Rsi, closes: &[Decimal]) -> Vec<Option<Decimal>> {
        candles(closes)
            .iter()
            .map(|c| {
                rsi.on_data(Item {
                    data: c,
                    mode: crate::source::Mode::Replay,
                })
                .map(|v| v.round_dp(8))
            })
            .collect()
    }

    #[test]
    fn rejects_zero_period() {
        assert!(Rsi::new(0).is_err());
    }

    #[test]
    fn hand_computed_sequence() {
        // Changes: +1 +1 -1 +2 0, period 3.
        // idx3: avg gain 2/3, avg loss 1/3  -> RS 2 -> 66.666…
        // idx4: gain (2/3*2+2)/3 = 10/9, loss (1/3*2)/3 = 2/9 -> RS 5 -> 83.333…
        // idx5: gain (10/9*2)/3 = 20/27, loss (2/9*2)/3 = 4/27 -> RS 5 -> 83.333…
        let mut rsi = Rsi::new(3).unwrap();
        let out = run(
            &mut rsi,
            &[dec!(10), dec!(11), dec!(12), dec!(11), dec!(13), dec!(13)],
        );
        assert_eq!(
            out,
            vec![
                None,
                None,
                None,
                Some(dec!(66.66666667)),
                Some(dec!(83.33333333)),
                Some(dec!(83.33333333)),
            ]
        );
    }

    #[test]
    fn warm_up_is_period_plus_one() {
        let mut rsi = Rsi::new(3).unwrap();
        assert_eq!(rsi.warm_up(), 4);
        let out = run(&mut rsi, &[dec!(1), dec!(2), dec!(3), dec!(4)]);
        assert_eq!(out.iter().filter(|v| v.is_some()).count(), 1);
        assert!(out[3].is_some());
    }

    #[test]
    fn only_gains_is_100() {
        let mut rsi = Rsi::new(2).unwrap();
        let out = run(&mut rsi, &[dec!(1), dec!(2), dec!(3)]);
        assert_eq!(out[2], Some(dec!(100)));
    }

    #[test]
    fn only_losses_is_0() {
        let mut rsi = Rsi::new(2).unwrap();
        let out = run(&mut rsi, &[dec!(3), dec!(2), dec!(1)]);
        assert_eq!(out[2], Some(dec!(0)));
    }

    #[test]
    fn flat_is_50() {
        let mut rsi = Rsi::new(2).unwrap();
        let out = run(&mut rsi, &[dec!(5), dec!(5), dec!(5)]);
        assert_eq!(out[2], Some(dec!(50)));
    }
}
