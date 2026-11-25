//! Volume transformers (spec F13).
//!
//! - [`VolumeMean`]: the mean volume of the `period` candles **before** the
//!   current one (the reference of a volume spike).
//! - [`Obv`]: On-Balance Volume: a running sum of the volumes, + on a candle
//!   closing above the previous close, − below, unchanged when equal
//!   (starts at 0 on the first candle).
//! - [`ObvEma`]: the OBV and its EMA, as a pair (the input of a cross rule).

use std::collections::VecDeque;

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::Transformer;
use crate::transformer::ema::EmaCalc;

/// Mean volume of the `period` candles before the current one.
#[derive(Debug, Clone)]
pub struct VolumeMean {
    period: usize,
    last: VecDeque<Decimal>,
}

impl VolumeMean {
    /// Mean over `period` candles (>= 1).
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        if period == 0 {
            return Err(ParameterError::Invalid(
                "volume mean: period must be >= 1".into(),
            ));
        }
        Ok(Self {
            period,
            last: VecDeque::with_capacity(period + 1),
        })
    }
}

impl Transformer for VolumeMean {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("volume_mean({})", self.period)
    }

    fn warm_up(&self) -> usize {
        self.period + 1
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        // The mean of the previous candles, then the current one is added.
        let mean = (self.last.len() == self.period)
            .then(|| self.last.iter().sum::<Decimal>() / Decimal::from(self.period as u64));
        self.last.push_back(item.data.volume);
        if self.last.len() > self.period {
            self.last.pop_front();
        }
        mean
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

/// Running OBV state.
#[derive(Debug, Clone, Default)]
struct ObvCalc {
    prev_close: Option<Decimal>,
    obv: Decimal,
}

impl ObvCalc {
    fn update(&mut self, c: &Candle) -> Decimal {
        if let Some(prev) = self.prev_close.replace(c.close) {
            if c.close > prev {
                self.obv += c.volume;
            } else if c.close < prev {
                self.obv -= c.volume;
            }
        }
        self.obv
    }
}

/// On-Balance Volume. Output: candle + OBV (the same shape as an RSI
/// candle, so the divergence condition works on it).
#[derive(Debug, Clone, Default)]
pub struct Obv(ObvCalc);

impl Transformer for Obv {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        "obv".into()
    }

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        Some(self.0.update(item.data))
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

/// OBV and its EMA of `period`: `(Some(obv), ema)` (the EMA is `None`
/// while it warms up). Output: an `EmaPair` (input of the cross rules).
#[derive(Debug, Clone)]
pub struct ObvEma {
    obv: ObvCalc,
    ema: EmaCalc,
}

impl ObvEma {
    /// OBV with its EMA of `period` (>= 1).
    pub fn new(period: usize) -> Result<Self, ParameterError> {
        Ok(Self {
            obv: ObvCalc::default(),
            ema: EmaCalc::new(period)?,
        })
    }
}

impl Transformer for ObvEma {
    type Input = Candle;
    type Value = (Option<Decimal>, Option<Decimal>);
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("obv_ema({})", self.ema.period())
    }

    fn warm_up(&self) -> usize {
        self.ema.period()
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Self::Value> {
        let obv = self.obv.update(item.data);
        Some((Some(obv), self.ema.update(obv)))
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;

    fn candle(i: i64, close: Decimal, volume: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            open: close,
            high: close,
            low: close,
            close,
            volume,
        }
    }

    fn run<T: Transformer<Input = Candle>>(t: &mut T, cs: &[(i64, i64)]) -> Vec<Option<T::Value>> {
        cs.iter()
            .enumerate()
            .map(|(i, &(c, v))| {
                t.on_data(Item {
                    data: &candle(i as i64, Decimal::from(c), Decimal::from(v)),
                    mode: Mode::Replay,
                })
            })
            .collect()
    }

    #[test]
    fn volume_mean_of_the_previous_candles() {
        let mut m = VolumeMean::new(2).unwrap();
        assert_eq!(m.warm_up(), 3);
        let out = run(&mut m, &[(1, 10), (1, 20), (1, 90), (1, 40)]);
        // (10 + 20) / 2 before the 3rd candle, (20 + 90) / 2 before the 4th.
        assert_eq!(out, vec![None, None, Some(dec!(15)), Some(dec!(55))]);
        assert!(VolumeMean::new(0).is_err());
    }

    #[test]
    fn obv_adds_on_up_closes_and_subtracts_on_down_closes() {
        let mut o = Obv::default();
        let out = run(&mut o, &[(10, 5), (11, 7), (11, 9), (9, 4), (12, 6)]);
        // 0, +7, = (equal close), -4, +6.
        let want = [0, 7, 7, 3, 9].map(|x| Some(Decimal::from(x)));
        assert_eq!(out, want.to_vec());
    }

    #[test]
    fn obv_ema_pairs_the_obv_with_its_ema() {
        let mut o = ObvEma::new(2).unwrap();
        let out = run(&mut o, &[(10, 5), (11, 4), (12, 6)]);
        // OBV 0, 4, 10; EMA 2 seeded with mean(0, 4) = 2, then 2 + 2/3 x 8.
        assert_eq!(out[0], Some((Some(dec!(0)), None)));
        assert_eq!(out[1], Some((Some(dec!(4)), Some(dec!(2)))));
        let (obv, ema) = out[2].unwrap();
        assert_eq!(obv, Some(dec!(10)));
        assert_eq!(ema.unwrap().round_dp(6), dec!(7.333333));
    }
}
