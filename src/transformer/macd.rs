//! MACD transformer (Moving Average Convergence Divergence), built on EMAs.
//!
//! - MACD line: `EMA(fast) − EMA(slow)` of the closes;
//! - signal line: EMA of the MACD line over `signal` values;
//! - histogram: `MACD − signal`.
//!
//! Default: 12 / 26 / 9. The MACD line exists after `slow` candles, the
//! signal line after `slow + signal − 1` candles (warm-up).

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::ema::EmaCalc;
use crate::transformer::{Derived, Transformer};

/// Value of the MACD transformer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacdValue {
    /// MACD line: EMA(fast) − EMA(slow).
    pub macd: Decimal,
    /// Signal line: EMA of the MACD line (`None` while it warms up).
    pub signal: Option<Decimal>,
}

impl MacdValue {
    /// Histogram: MACD − signal.
    pub fn histogram(&self) -> Option<Decimal> {
        self.signal.map(|s| self.macd - s)
    }
}

/// Item produced by the MACD transformer: the candle and its MACD.
pub type MacdCandle = Derived<Candle, MacdValue>;

/// MACD of the closes.
#[derive(Debug, Clone)]
pub struct Macd {
    fast: EmaCalc,
    slow: EmaCalc,
    signal: EmaCalc,
}

impl Macd {
    /// MACD with the given periods; `fast` must be shorter than `slow`, all
    /// at least 1.
    pub fn new(fast: usize, slow: usize, signal: usize) -> Result<Self, ParameterError> {
        if fast >= slow {
            return Err(ParameterError::Invalid(
                "MACD: the fast period must be shorter than the slow one".into(),
            ));
        }
        Ok(Self {
            fast: EmaCalc::new(fast)?,
            slow: EmaCalc::new(slow)?,
            signal: EmaCalc::new(signal)?,
        })
    }
}

impl Default for Macd {
    /// MACD 12 / 26 / 9.
    fn default() -> Self {
        Self {
            fast: EmaCalc::clamped(12),
            slow: EmaCalc::clamped(26),
            signal: EmaCalc::clamped(9),
        }
    }
}

impl Transformer for Macd {
    type Input = Candle;
    type Value = MacdValue;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!(
            "macd({},{},{})",
            self.fast.period(),
            self.slow.period(),
            self.signal.period()
        )
    }

    fn warm_up(&self) -> usize {
        self.slow.period() + self.signal.period() - 1
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<MacdValue> {
        let close = item.data.close;
        let fast = self.fast.update(close);
        let slow = self.slow.update(close);
        let macd = fast? - slow?;
        let signal = self.signal.update(macd);
        Some(MacdValue { macd, signal })
    }

    fn on_modifier(&mut self, modifier: NoModifier) -> Result<(), ModifierError> {
        match modifier {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;

    fn candle(i: i64, close: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            open: close,
            high: close,
            low: close,
            close,
            volume: dec!(1),
        }
    }

    #[test]
    fn hand_computed_macd() {
        // fast 2 (k = 2/3), slow 3 (k = 1/2), signal 2 (k = 2/3).
        // closes 1 2 3 4 5 9
        // fast:  -, 1.5, 2.5, 3.5, 4.5, 7.5
        // slow:  -, -,   2,   3,   4,   6.5
        // macd:  -, -,   0.5, 0.5, 0.5, 1
        // signal: -, -,  -,   0.5 (mean), 0.5, 0.5 + 2/3 x 0.5 = 0.8333...
        let mut m = Macd::new(2, 3, 2).unwrap();
        assert_eq!(m.warm_up(), 4);
        let closes = [1, 2, 3, 4, 5, 9];
        let out: Vec<Option<(Decimal, Option<Decimal>)>> = closes
            .iter()
            .zip(0..)
            .map(|(&c, i)| {
                let k = candle(i, Decimal::from(c));
                m.on_data(Item {
                    data: &k,
                    mode: Mode::Replay,
                })
                .map(|v| (v.macd.round_dp(6), v.signal.map(|s| s.round_dp(6))))
            })
            .collect();
        assert_eq!(
            out,
            vec![
                None,
                None,
                Some((dec!(0.5), None)),
                Some((dec!(0.5), Some(dec!(0.5)))),
                Some((dec!(0.5), Some(dec!(0.5)))),
                Some((dec!(1), Some(dec!(0.833333)))),
            ]
        );
    }

    #[test]
    fn histogram_and_validation() {
        let v = MacdValue {
            macd: dec!(1),
            signal: Some(dec!(0.25)),
        };
        assert_eq!(v.histogram(), Some(dec!(0.75)));
        assert!(Macd::new(26, 12, 9).is_err());
        assert!(Macd::new(12, 26, 0).is_err());
        assert_eq!(Macd::default().name(), "macd(12,26,9)");
        assert_eq!(Macd::default().warm_up(), 34);
    }
}
