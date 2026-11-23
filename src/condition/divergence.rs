//! RSI divergence condition (spec F6).
//!
//! - **Bullish** (bear phase): price makes a lower minimum, RSI a higher minimum.
//! - **Bearish** (bull phase): price makes a higher maximum, RSI a lower maximum.
//!
//! Minima / maxima are pivots: a candle whose low (high) is beyond the `left`
//! candles before and the `right` candles after it. A pivot is therefore only
//! confirmed `right` candles later, which is when the detection is emitted.

use std::cmp::Ordering;
use std::collections::VecDeque;

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;

use crate::condition::Condition;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::{Item, Timeframe};
use crate::transformer::rsi::RsiCandle;
use crate::trigger::{Detection, TriggerEventType, TriggerValue};

/// Kind of divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    /// Price lower low, RSI higher low: possible end of a bear phase.
    Bullish,
    /// Price higher high, RSI lower high: possible end of a bull phase.
    Bearish,
}

/// A price minimum or maximum with the RSI of the same candle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pivot {
    /// Open time of the pivot candle.
    pub time: DateTime<Utc>,
    /// Low (minimum pivot) or high (maximum pivot) of the candle.
    pub price: Decimal,
    /// RSI of the candle.
    pub rsi: Decimal,
}

/// Value emitted when a divergence is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Bullish or bearish.
    pub kind: DivergenceKind,
    /// Previous pivot (A).
    pub first: Pivot,
    /// New pivot (B).
    pub second: Pivot,
    /// Open time of the candle that confirmed pivot B.
    pub confirmed_at: DateTime<Utc>,
}

impl TriggerValue for Divergence {}

/// Parameters of [`RsiDivergence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DivergenceParams {
    /// Candles before a pivot.
    pub left: usize,
    /// Candles after a pivot (confirmation delay).
    pub right: usize,
    /// Minimum time between two compared pivots (inclusive).
    pub min_gap: TimeDelta,
    /// Maximum time between two compared pivots (inclusive).
    pub max_gap: TimeDelta,
}

impl DivergenceParams {
    /// Default parameters for a timeframe: `left` = `right` = 3 candles,
    /// `max_gap` = 60 candles and `min_gap` = 5 candles at 1m, 3 at 5m,
    /// 2 from 15m up.
    pub fn for_timeframe(timeframe: Timeframe) -> Self {
        let min_candles = match timeframe {
            Timeframe::M1 => 5,
            Timeframe::M5 => 3,
            Timeframe::M15 | Timeframe::H1 | Timeframe::H4 | Timeframe::D1 => 2,
        };
        let candle = timeframe.duration();
        Self {
            left: 3,
            right: 3,
            min_gap: candle * min_candles,
            max_gap: candle * 60,
        }
    }
}

impl Default for DivergenceParams {
    /// Defaults for 1-minute candles (see [`DivergenceParams::for_timeframe`]).
    fn default() -> Self {
        Self::for_timeframe(Timeframe::M1)
    }
}

#[derive(Debug, Clone, Copy)]
struct Bar {
    time: DateTime<Utc>,
    low: Decimal,
    high: Decimal,
    rsi: Option<Decimal>,
}

/// Detects bullish and bearish RSI divergences.
#[derive(Debug, Clone)]
pub struct RsiDivergence {
    params: DivergenceParams,
    /// The last `left + 1 + right` candles; the candidate pivot is at `left`.
    window: VecDeque<Bar>,
    last_min: Option<Pivot>,
    last_max: Option<Pivot>,
}

impl RsiDivergence {
    /// Create the condition. Rejects `left` or `right` = 0, a negative
    /// `min_gap` and `min_gap > max_gap`.
    pub fn new(params: DivergenceParams) -> Result<Self, ParameterError> {
        if params.left == 0 || params.right == 0 {
            return Err(ParameterError::Invalid(
                "divergence: left and right must be >= 1".into(),
            ));
        }
        if params.min_gap < TimeDelta::zero() {
            return Err(ParameterError::Invalid(
                "divergence: min_gap must be >= 0".into(),
            ));
        }
        if params.min_gap > params.max_gap {
            return Err(ParameterError::Invalid(
                "divergence: min_gap must be <= max_gap".into(),
            ));
        }
        Ok(Self::unchecked(params))
    }

    fn unchecked(params: DivergenceParams) -> Self {
        Self {
            params,
            window: VecDeque::with_capacity(params.left + params.right + 2),
            last_min: None,
            last_max: None,
        }
    }

    /// Is the candidate (at `left`) a pivot for `key`? `beyond(a, b)` means
    /// "a is strictly beyond b" (lower for minima, higher for maxima).
    /// Strict on the left side, non-strict on the right side.
    fn is_pivot(&self, key: fn(&Bar) -> Decimal, beyond: fn(Decimal, Decimal) -> bool) -> bool {
        let left = self.params.left;
        let v = key(&self.window[left]);
        self.window
            .iter()
            .enumerate()
            .all(|(i, bar)| match i.cmp(&left) {
                Ordering::Less => beyond(v, key(bar)),
                Ordering::Equal => true,
                Ordering::Greater => !beyond(key(bar), v),
            })
    }

    /// Compare a new pivot B with the previous pivot A of the same kind.
    fn check(&self, previous: Option<Pivot>, b: Pivot, kind: DivergenceKind) -> bool {
        let Some(a) = previous else {
            return false;
        };
        // Measured on the pivot times: independent of the timeframe and of
        // missing candles.
        let gap = b.time - a.time;
        if gap < self.params.min_gap || gap > self.params.max_gap {
            return false;
        }
        match kind {
            DivergenceKind::Bullish => b.price < a.price && b.rsi > a.rsi,
            DivergenceKind::Bearish => b.price > a.price && b.rsi < a.rsi,
        }
    }
}

impl Default for RsiDivergence {
    /// Uses [`DivergenceParams::default`] (always valid).
    fn default() -> Self {
        Self::unchecked(DivergenceParams::default())
    }
}

impl Condition for RsiDivergence {
    type Input = RsiCandle;
    type Value = Divergence;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.params.left + 1 + self.params.right
    }

    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Divergence>> {
        let item = item.data;
        let c = &item.data;
        self.window.push_back(Bar {
            time: c.open_time,
            low: c.low,
            high: c.high,
            rsi: item.value,
        });
        let size = self.params.left + 1 + self.params.right;
        if self.window.len() > size {
            self.window.pop_front();
        }
        if self.window.len() < size {
            return None;
        }

        // The candidate is confirmed by the candle that just closed.
        let candidate = self.window[self.params.left];
        let confirmed_at = c.open_time;
        let mut detection = None;

        if self.is_pivot(|b| b.low, |a, b| a < b)
            && let Some(rsi) = candidate.rsi
        {
            let b = Pivot {
                time: candidate.time,
                price: candidate.low,
                rsi,
            };
            if self.check(self.last_min, b, DivergenceKind::Bullish) {
                detection = self.last_min.map(|a| (DivergenceKind::Bullish, a, b));
            }
            self.last_min = Some(b);
        }

        if self.is_pivot(|b| b.high, |a, b| a > b)
            && let Some(rsi) = candidate.rsi
        {
            let b = Pivot {
                time: candidate.time,
                price: candidate.high,
                rsi,
            };
            if detection.is_none() && self.check(self.last_max, b, DivergenceKind::Bearish) {
                detection = self.last_max.map(|a| (DivergenceKind::Bearish, a, b));
            }
            self.last_max = Some(b);
        }

        detection.map(|(kind, first, second)| Detection {
            event_type: match kind {
                DivergenceKind::Bullish => TriggerEventType::BullishDivergence,
                DivergenceKind::Bearish => TriggerEventType::BearishDivergence,
            },
            value: Divergence {
                kind,
                first,
                second,
                confirmed_at,
            },
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use crate::transformer::Derived;
    use chrono::TimeDelta;

    fn at(i: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i)
    }

    fn item(i: i64, low: i64, high: i64, rsi: Option<i64>) -> RsiCandle {
        let (low, high) = (Decimal::from(low), Decimal::from(high));
        Derived {
            data: Candle {
                open_time: at(i),
                open: low,
                high,
                low,
                close: high,
                volume: Decimal::ONE,
            },
            value: rsi.map(Decimal::from),
        }
    }

    /// Bars defined by their low (high = low + 1).
    fn by_low(lows: &[i64], rsi: &[Option<i64>]) -> Vec<RsiCandle> {
        (0..)
            .zip(lows.iter().zip(rsi))
            .map(|(i, (&l, &r))| item(i, l, l + 1, r))
            .collect()
    }

    /// Bars defined by their high (low = high - 1).
    fn by_high(highs: &[i64], rsi: &[Option<i64>]) -> Vec<RsiCandle> {
        (0..)
            .zip(highs.iter().zip(rsi))
            .map(|(i, (&h, &r))| item(i, h - 1, h, r))
            .collect()
    }

    /// Test candles are 1 minute apart: gaps given in candles = minutes.
    fn params(min_gap: i64, max_gap: i64) -> DivergenceParams {
        DivergenceParams {
            left: 2,
            right: 2,
            min_gap: TimeDelta::minutes(min_gap),
            max_gap: TimeDelta::minutes(max_gap),
        }
    }

    /// Feed all items; return (index of the confirming candle, detection).
    fn run(p: DivergenceParams, items: &[RsiCandle]) -> Vec<(usize, Detection<Divergence>)> {
        let mut cond = RsiDivergence::new(p).unwrap();
        items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                cond.on_data(Item {
                    data: it,
                    mode: crate::source::Mode::Replay,
                })
                .map(|d| (i, d))
            })
            .collect()
    }

    fn s(v: &[i64]) -> Vec<Option<i64>> {
        v.iter().copied().map(Some).collect()
    }

    // Minimum pivots at #2 (low 10) and #7 (low 8): price lower low.
    const LOWS: [i64; 11] = [15, 14, 10, 14, 15, 13, 12, 8, 12, 13, 14];
    // Maximum pivots at #2 (high 10) and #7 (high 12): price higher high.
    const HIGHS: [i64; 11] = [5, 6, 10, 6, 5, 7, 8, 12, 8, 7, 6];

    #[test]
    fn bullish_divergence_is_emitted_on_confirmation() {
        // RSI at #2 = 25, at #7 = 30: higher low.
        let rsi = s(&[40, 35, 25, 35, 40, 38, 36, 30, 36, 38, 40]);
        let out = run(params(2, 20), &by_low(&LOWS, &rsi));
        assert_eq!(out.len(), 1);
        let (i, d) = &out[0];
        assert_eq!(*i, 9, "confirmed right = 2 candles after pivot #7");
        assert_eq!(d.event_type, TriggerEventType::BullishDivergence);
        assert_eq!(d.value.kind, DivergenceKind::Bullish);
        assert_eq!(
            (d.value.first.time, d.value.first.price),
            (at(2), Decimal::from(10))
        );
        assert_eq!(
            (d.value.second.time, d.value.second.price),
            (at(7), Decimal::from(8))
        );
        assert_eq!(d.value.first.rsi, Decimal::from(25));
        assert_eq!(d.value.second.rsi, Decimal::from(30));
        assert_eq!(d.value.confirmed_at, at(9));
    }

    #[test]
    fn bearish_divergence_is_emitted_on_confirmation() {
        // RSI at #2 = 75, at #7 = 70: lower high.
        let rsi = s(&[60, 65, 75, 65, 60, 62, 64, 70, 64, 62, 60]);
        let out = run(params(2, 20), &by_high(&HIGHS, &rsi));
        assert_eq!(out.len(), 1);
        let (i, d) = &out[0];
        assert_eq!(*i, 9);
        assert_eq!(d.event_type, TriggerEventType::BearishDivergence);
        assert_eq!(d.value.first.price, Decimal::from(10));
        assert_eq!(d.value.second.price, Decimal::from(12));
    }

    #[test]
    fn no_divergence_when_rsi_follows_price() {
        // RSI lower low too (25 -> 20).
        let rsi = s(&[40, 35, 25, 35, 40, 38, 36, 20, 36, 38, 40]);
        assert!(run(params(2, 20), &by_low(&LOWS, &rsi)).is_empty());
    }

    #[test]
    fn gap_must_be_within_bounds() {
        let rsi = s(&[40, 35, 25, 35, 40, 38, 36, 30, 36, 38, 40]);
        let items = by_low(&LOWS, &rsi); // gap = 5
        assert!(run(params(6, 20), &items).is_empty(), "too close");
        assert!(run(params(2, 4), &items).is_empty(), "too far");
        assert_eq!(run(params(5, 5), &items).len(), 1, "bounds are inclusive");
    }

    #[test]
    fn nothing_before_confirmation() {
        let rsi = s(&[40, 35, 25, 35, 40, 38, 36, 30, 36, 38, 40]);
        let items = by_low(&LOWS, &rsi);
        // Stop one candle before the confirmation of pivot #7.
        assert!(run(params(2, 20), &items[..9]).is_empty());
    }

    #[test]
    fn pivot_without_rsi_is_ignored() {
        let mut rsi = s(&[40, 35, 25, 35, 40, 38, 36, 30, 36, 38, 40]);
        rsi[2] = None; // pivot A still warming up
        assert!(run(params(2, 20), &by_low(&LOWS, &rsi)).is_empty());
    }

    #[test]
    fn flat_bottom_gives_one_pivot() {
        // #2 and #3 share the same low: only #2 is a pivot (strict left, non-strict right).
        let lows = [15, 14, 10, 10, 15, 13, 12, 8, 12, 13, 14];
        let rsi = s(&[40, 35, 25, 26, 40, 38, 36, 30, 36, 38, 40]);
        let out = run(params(2, 20), &by_low(&lows, &rsi));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.value.first.time, at(2));
    }

    #[test]
    fn rejects_invalid_params() {
        let p = DivergenceParams::default();
        assert!(RsiDivergence::new(DivergenceParams { left: 0, ..p }).is_err());
        assert!(RsiDivergence::new(DivergenceParams { right: 0, ..p }).is_err());
        assert!(
            RsiDivergence::new(DivergenceParams {
                min_gap: TimeDelta::minutes(10),
                max_gap: TimeDelta::minutes(5),
                ..p
            })
            .is_err()
        );
        assert!(
            RsiDivergence::new(DivergenceParams {
                min_gap: TimeDelta::minutes(-1),
                ..p
            })
            .is_err()
        );
        assert!(RsiDivergence::new(p).is_ok());
    }

    #[test]
    fn default_gaps_depend_on_the_timeframe() {
        let p = |tf| DivergenceParams::for_timeframe(tf);
        assert_eq!(p(Timeframe::M1).min_gap, TimeDelta::minutes(5));
        assert_eq!(p(Timeframe::M1).max_gap, TimeDelta::minutes(60));
        assert_eq!(p(Timeframe::M5).min_gap, TimeDelta::minutes(15));
        assert_eq!(p(Timeframe::M15).min_gap, TimeDelta::minutes(30));
        assert_eq!(p(Timeframe::M15).max_gap, TimeDelta::hours(15));
        assert_eq!(p(Timeframe::H1).min_gap, TimeDelta::hours(2));
        assert_eq!(DivergenceParams::default(), p(Timeframe::M1));
    }

    #[test]
    fn gap_is_measured_in_time_on_15m_candles() {
        // Same shape as LOWS, but with 15-minute candles: pivots 5 candles = 75 min apart.
        let rsi = s(&[40, 35, 25, 35, 40, 38, 36, 30, 36, 38, 40]);
        let items: Vec<RsiCandle> = by_low(&LOWS, &rsi)
            .into_iter()
            .zip(0..)
            .map(|(mut it, i)| {
                it.data.open_time = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(15 * i);
                it
            })
            .collect();
        let p15 = DivergenceParams {
            left: 2,
            right: 2,
            ..DivergenceParams::for_timeframe(Timeframe::M15)
        };
        assert_eq!(run(p15, &items).len(), 1, "75 min is within 30 min .. 15 h");
        let strict = DivergenceParams {
            min_gap: TimeDelta::minutes(90),
            ..p15
        };
        assert!(run(strict, &items).is_empty(), "75 min < 90 min");
    }
}
