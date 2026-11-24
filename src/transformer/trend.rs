//! Trend transformer: the side of the close relative to an EMA (spec F8,
//! rule 3). Used with [`TrendFilter`](crate::condition::filter::TrendFilter)
//! to only keep signals in the direction of the trend.

use std::marker::PhantomData;

use rust_decimal::Decimal;

use crate::candle::CandleData;
use crate::condition::Signal;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::Transformer;
use crate::transformer::ema::EmaCalc;

/// Trend of a candle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrendValue {
    /// The trend EMA.
    pub ema: Decimal,
    /// Signal allowed by the trend: `Bullish` when the close is above the
    /// EMA (uptrend), `Bearish` when below (downtrend), `None` when equal.
    pub side: Option<Signal>,
}

/// Trend of the closes: their side relative to their EMA of `period`.
/// Works on any item containing a candle (candles, candles + RSI, ...).
#[derive(Debug, Clone)]
pub struct Trend<I> {
    ema: EmaCalc,
    input: PhantomData<fn(I)>,
}

impl<I> Trend<I> {
    /// Trend over an EMA of `period` closes (e.g. 200). `period` must be >= 1.
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        Ok(Self {
            ema: EmaCalc::new(period)?,
            input: PhantomData,
        })
    }
}

impl<I: CandleData> Transformer for Trend<I> {
    type Input = I;
    type Value = TrendValue;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("trend(ema {})", self.ema.period())
    }

    fn warm_up(&self) -> usize {
        self.ema.period()
    }

    fn on_data(&mut self, item: Item<'_, I>) -> Option<TrendValue> {
        let close = item.data.candle().close;
        let ema = self.ema.update(close)?;
        let side = if close > ema {
            Some(Signal::Bullish)
        } else if close < ema {
            Some(Signal::Bearish)
        } else {
            None
        };
        Some(TrendValue { ema, side })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use crate::source::Mode;
    use crate::transformer::Derived;
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
    fn side_of_the_close_relative_to_the_ema() {
        // EMA 2 (k = 2/3): seed mean(10, 10) = 10.
        let mut t = Trend::<Candle>::new(2).unwrap();
        let closes = [dec!(10), dec!(10), dec!(13), dec!(9), dec!(11)];
        let out: Vec<Option<Option<Signal>>> = closes
            .iter()
            .zip(0..)
            .map(|(&c, i)| {
                t.on_data(Item {
                    data: &candle(i, c),
                    mode: Mode::Replay,
                })
                .map(|v| v.side)
            })
            .collect();
        // 10 = EMA 10: flat; 13 > 12: up; 9 < 10: down; 11 > 10.667: up.
        assert_eq!(
            out,
            vec![
                None,
                Some(None),
                Some(Some(Signal::Bullish)),
                Some(Some(Signal::Bearish)),
                Some(Some(Signal::Bullish)),
            ]
        );
    }

    #[test]
    fn works_on_any_item_containing_a_candle() {
        // Candle + RSI (as produced by the RSI transformer).
        let mut t = Trend::<Derived<Candle, Decimal>>::new(1).unwrap();
        let item = Derived {
            data: candle(0, dec!(5)),
            value: Some(dec!(40)),
        };
        let v = t
            .on_data(Item {
                data: &item,
                mode: Mode::Replay,
            })
            .unwrap();
        assert_eq!(v.ema, dec!(5));
        assert_eq!(t.name(), "trend(ema 1)");
        assert!(Trend::<Candle>::new(0).is_err());
    }
}
