//! RSI divergence condition (spec F6).
//!
//! - **Bullish** (bear phase): price makes a lower minimum, RSI a higher minimum.
//! - **Bearish** (bull phase): price makes a higher maximum, RSI a lower maximum.
//!
//! Minima / maxima are pivots: a candle whose low (high) is beyond the `left`
//! candles before and the `right` candles after it. A pivot is therefore only
//! confirmed `right` candles later, which is when the detection is emitted.
//!
//! Search (lows; highs are the mirror):
//! - a low **starts** a search (first pivot A) only if its RSI is at most
//!   `rsi_low` (e.g. 30; 100 for any level);
//! - the diverging low is searched in the **next `lookahead` lows** (e.g. 3):
//!   a lower low with a higher RSI, at least `min_gap` after A, gives the
//!   signal, and A is consumed;
//! - a low with a **lower RSI than A** (not diverging) becomes the new A and
//!   restarts the window; other lows only count;
//! - after `lookahead` lows without divergence, A expires.

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
    /// Minimum time between the first pivot and the diverging one (inclusive).
    pub min_gap: TimeDelta,
    /// A low starts a bullish search only if its RSI is at most this level
    /// (100: any level).
    pub rsi_low: Decimal,
    /// A high starts a bearish search only if its RSI is at least this level
    /// (0: any level).
    pub rsi_high: Decimal,
    /// Number of pivots, after the first one, in which the diverging pivot is
    /// searched.
    pub lookahead: usize,
}

impl DivergenceParams {
    /// Default parameters for a timeframe: `left` = `right` = 3 candles;
    /// `min_gap` = 5 candles at 1m, 3 at 5m, 2 from 15m up; `rsi_low` = 30,
    /// `rsi_high` = 70; `lookahead` = 3 pivots.
    pub fn for_timeframe(timeframe: Timeframe) -> Self {
        let min_candles = match timeframe {
            Timeframe::M1 => 5,
            Timeframe::M5 => 3,
            Timeframe::M15 | Timeframe::H1 | Timeframe::H4 | Timeframe::D1 => 2,
        };
        Self {
            left: 3,
            right: 3,
            min_gap: timeframe.duration() * min_candles,
            rsi_low: Decimal::from(30),
            rsi_high: Decimal::from(70),
            lookahead: 3,
        }
    }
}

impl Default for DivergenceParams {
    /// Defaults for 1-minute candles (see [`DivergenceParams::for_timeframe`]).
    fn default() -> Self {
        Self::for_timeframe(Timeframe::M1)
    }
}

/// A test between the first pivot A and a new pivot B.
type PivotTest = fn(&Pivot, &Pivot) -> bool;

/// Search of a divergence on one side (lows or highs): the first pivot A,
/// and the number of pivots seen since.
#[derive(Debug, Clone, Default)]
struct Search {
    first: Option<Pivot>,
    seen: usize,
}

#[derive(Debug, Clone, Copy)]
struct Bar {
    time: DateTime<Utc>,
    low: Decimal,
    high: Decimal,
    rsi: Option<Decimal>,
}

/// Pivots confirmed by the last candle (at most one of each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfirmedPivots {
    /// A price minimum (its low), with its RSI.
    pub low: Option<Pivot>,
    /// A price maximum (its high), with its RSI.
    pub high: Option<Pivot>,
}

/// Detects price pivots on candles with RSI: a candle whose low (high) is
/// strictly beyond the `left` candles before it and at least as far as the
/// `right` candles after it. A pivot is confirmed `right` candles later.
/// Candles without RSI (warm-up) give no pivot.
#[derive(Debug, Clone)]
pub struct Pivots {
    left: usize,
    right: usize,
    /// The last `left + 1 + right` candles; the candidate pivot is at `left`.
    window: VecDeque<Bar>,
}

impl Pivots {
    /// Pivots with `left` / `right` candles around them (at least 1).
    pub fn new(left: usize, right: usize) -> Self {
        let (left, right) = (left.max(1), right.max(1));
        Self {
            left,
            right,
            window: VecDeque::with_capacity(left + right + 2),
        }
    }

    /// Candles needed before the first pivot.
    pub fn warm_up(&self) -> usize {
        self.left + 1 + self.right
    }

    /// Add a closed candle; returns the pivots it confirms.
    pub fn push(&mut self, item: &RsiCandle) -> ConfirmedPivots {
        let c = &item.data;
        self.window.push_back(Bar {
            time: c.open_time,
            low: c.low,
            high: c.high,
            rsi: item.value,
        });
        if self.window.len() > self.warm_up() {
            self.window.pop_front();
        }
        if self.window.len() < self.warm_up() {
            return ConfirmedPivots::default();
        }
        let candidate = self.window[self.left];
        let Some(rsi) = candidate.rsi else {
            return ConfirmedPivots::default();
        };
        let pivot = |price| Pivot {
            time: candidate.time,
            price,
            rsi,
        };
        ConfirmedPivots {
            low: self
                .is_pivot(|b| b.low, |a, b| a < b)
                .then(|| pivot(candidate.low)),
            high: self
                .is_pivot(|b| b.high, |a, b| a > b)
                .then(|| pivot(candidate.high)),
        }
    }

    /// Is the candidate (at `left`) a pivot for `key`? `beyond(a, b)` means
    /// "a is strictly beyond b" (lower for minima, higher for maxima).
    /// Strict on the left side, non-strict on the right side.
    fn is_pivot(&self, key: fn(&Bar) -> Decimal, beyond: fn(Decimal, Decimal) -> bool) -> bool {
        let left = self.left;
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
}

/// Detects bullish and bearish RSI divergences.
#[derive(Debug, Clone)]
pub struct RsiDivergence {
    params: DivergenceParams,
    pivots: Pivots,
    lows: Search,
    highs: Search,
    /// `false`: any pivot can be the first one (indicators without levels,
    /// e.g. the OBV); `rsi_low` / `rsi_high` are ignored.
    levels: bool,
}

impl RsiDivergence {
    /// Create the condition. Rejects `left`, `right` or `lookahead` = 0, a
    /// negative `min_gap`, and RSI levels outside 0..=100.
    pub fn new(params: DivergenceParams) -> Result<Self, ParameterError> {
        let invalid = |m: &str| Err(ParameterError::Invalid(format!("divergence: {m}")));
        if params.left == 0 || params.right == 0 {
            return invalid("left and right must be >= 1");
        }
        if params.min_gap < TimeDelta::zero() {
            return invalid("min_gap must be >= 0");
        }
        if params.lookahead == 0 {
            return invalid("lookahead must be >= 1");
        }
        let rsi = Decimal::ZERO..=Decimal::ONE_HUNDRED;
        if !rsi.contains(&params.rsi_low) || !rsi.contains(&params.rsi_high) {
            return invalid("rsi_low and rsi_high must be within 0..=100");
        }
        Ok(Self::unchecked(params))
    }

    /// Divergences of the price with an indicator that has no levels (e.g.
    /// the OBV): any pivot can be the first one; `rsi_low` / `rsi_high` are
    /// ignored. The other checks of [`new`](Self::new) apply.
    pub fn any_level(params: DivergenceParams) -> Result<Self, ParameterError> {
        let checked = DivergenceParams {
            rsi_low: Decimal::ONE_HUNDRED,
            rsi_high: Decimal::ZERO,
            ..params
        };
        Ok(Self {
            levels: false,
            ..Self::new(checked)?
        })
    }

    fn unchecked(params: DivergenceParams) -> Self {
        Self {
            params,
            pivots: Pivots::new(params.left, params.right),
            lows: Search::default(),
            highs: Search::default(),
            levels: true,
        }
    }

    /// A new pivot `b` on the side `kind` (lows: bullish, highs: bearish).
    /// Returns the diverging pair (A, B), if any.
    ///
    /// - With no first pivot A, `b` becomes A if its RSI passes the level
    ///   (`rsi_low` for lows, `rsi_high` for highs).
    /// - With A: `b` diverges (lower low and higher RSI, or higher high and
    ///   lower RSI, at least `min_gap` later) -> signal, A is consumed;
    ///   otherwise, a more extreme RSI than A's makes `b` the new A (the
    ///   window restarts); otherwise `b` counts, and A expires after
    ///   `lookahead` pivots (then `b` may start a new search).
    fn on_pivot(&mut self, b: Pivot, kind: DivergenceKind) -> Option<(Pivot, Pivot)> {
        let p = self.params;
        let (search, valid, diverges, more_extreme): (&mut Search, bool, PivotTest, PivotTest) =
            match kind {
                DivergenceKind::Bullish => (
                    &mut self.lows,
                    !self.levels || b.rsi <= p.rsi_low,
                    |a, b| b.price < a.price && b.rsi > a.rsi,
                    |a, b| b.rsi < a.rsi,
                ),
                DivergenceKind::Bearish => (
                    &mut self.highs,
                    !self.levels || b.rsi >= p.rsi_high,
                    |a, b| b.price > a.price && b.rsi < a.rsi,
                    |a, b| b.rsi > a.rsi,
                ),
            };
        if let Some(a) = search.first {
            search.seen += 1;
            if b.time - a.time >= p.min_gap && diverges(&a, &b) {
                *search = Search::default(); // A is consumed
                return Some((a, b));
            }
            if more_extreme(&a, &b) {
                *search = Search {
                    first: Some(b),
                    seen: 0,
                };
                return None;
            }
            if search.seen < p.lookahead {
                return None;
            }
            *search = Search::default(); // A expires
        }
        if valid {
            search.first = Some(b);
        }
        None
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
        let confirmed_at = item.data.data.open_time;
        let pivots = self.pivots.push(item.data);
        let bullish = pivots.low.and_then(|b| {
            self.on_pivot(b, DivergenceKind::Bullish)
                .map(|(a, b)| (DivergenceKind::Bullish, a, b))
        });
        let bearish = pivots.high.and_then(|b| {
            self.on_pivot(b, DivergenceKind::Bearish)
                .map(|(a, b)| (DivergenceKind::Bearish, a, b))
        });
        let detection = bullish.or(bearish);

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

    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<Self::Value>>, ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use crate::source::Mode;
    use crate::transformer::Derived;
    use chrono::{DateTime, Utc};

    /// Valleys: each is 5 candles with lows `v+4, v+2, v, v+2, v+4`; the
    /// middle one is a low pivot (left = right = 2) with the given RSI. Highs
    /// are `low + 1`. Other candles have RSI 50.
    fn valleys(spec: &[(i64, i64)]) -> Vec<RsiCandle> {
        let mut out = Vec::new();
        for &(v, rsi) in spec {
            for (offset, r) in [(4, 50), (2, 50), (0, rsi), (2, 50), (4, 50)] {
                let i = out.len() as i64;
                let low = Decimal::from(v + offset);
                out.push(Derived {
                    data: Candle {
                        open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
                        open: low,
                        high: low + Decimal::ONE,
                        low,
                        close: low + Decimal::ONE,
                        volume: Decimal::ONE,
                    },
                    value: Some(Decimal::from(r)),
                });
            }
        }
        // Closing candles so that the last valley is confirmed.
        let last = out.last().cloned();
        if let Some(last) = last {
            for k in 1..=2 {
                let mut c = last.clone();
                c.data.open_time += TimeDelta::minutes(k);
                c.data.low += Decimal::from(k);
                c.data.high += Decimal::from(k);
                out.push(c);
            }
        }
        out
    }

    /// Mirror prices around 500 and RSI around 50: lows become highs.
    fn mirror(items: &[RsiCandle]) -> Vec<RsiCandle> {
        let m = |x: Decimal| Decimal::from(500) - x;
        items
            .iter()
            .map(|d| Derived {
                data: Candle {
                    open: m(d.data.open),
                    high: m(d.data.low),
                    low: m(d.data.high),
                    close: m(d.data.close),
                    ..d.data.clone()
                },
                value: d.value.map(|r| Decimal::ONE_HUNDRED - r),
            })
            .collect()
    }

    fn params() -> DivergenceParams {
        DivergenceParams {
            left: 2,
            right: 2,
            min_gap: TimeDelta::minutes(1),
            ..DivergenceParams::default()
        }
    }

    /// (kind, A price, B price) of every detection.
    fn run(p: DivergenceParams, items: &[RsiCandle]) -> Vec<(DivergenceKind, Decimal, Decimal)> {
        let mut cond = RsiDivergence::new(p).unwrap();
        items
            .iter()
            .filter_map(|data| {
                cond.on_data(Item {
                    data,
                    mode: Mode::Replay,
                })
            })
            .map(|d| (d.value.kind, d.value.first.price, d.value.second.price))
            .collect()
    }

    fn bullish(items: &[RsiCandle]) -> Vec<(Decimal, Decimal)> {
        run(params(), items)
            .into_iter()
            .filter(|(k, ..)| *k == DivergenceKind::Bullish)
            .map(|(_, a, b)| (a, b))
            .collect()
    }

    fn d(v: i64) -> Decimal {
        Decimal::from(v)
    }

    #[test]
    fn the_first_low_must_be_oversold() {
        // A at 100 with RSI 25 (<= 30), B lower (98) with RSI 28: divergence.
        assert_eq!(
            bullish(&valleys(&[(100, 25), (98, 28)])),
            vec![(d(100), d(98))]
        );
        // Same shape but A has RSI 35 (> 30): no search, no divergence.
        assert!(bullish(&valleys(&[(100, 35), (98, 40)])).is_empty());
    }

    #[test]
    fn the_diverging_low_is_searched_in_the_next_lookahead_lows() {
        // A (25), then 2 higher lows that do not diverge, then B: 3rd low -> found.
        let found = valleys(&[(100, 25), (105, 40), (106, 40), (98, 28)]);
        assert_eq!(bullish(&found), vec![(d(100), d(98))]);
        // B is the 4th low after A: A has expired.
        let late = valleys(&[(100, 25), (105, 40), (106, 40), (107, 40), (98, 28)]);
        assert!(bullish(&late).is_empty());
    }

    #[test]
    fn a_lower_rsi_restarts_the_window() {
        // A (100, RSI 25); then 95 with RSI 20 (lower RSI: not a divergence)
        // becomes the new A; then 93 with RSI 22 diverges from it.
        let items = valleys(&[(100, 25), (95, 20), (93, 22)]);
        assert_eq!(bullish(&items), vec![(d(95), d(93))]);
    }

    #[test]
    fn an_oversold_low_with_a_higher_rsi_does_not_replace_the_first() {
        // A (100, RSI 25); 105 with RSI 28 (<= 30 but higher than A): counts
        // only; 99 with RSI 27 diverges from A, not from 105.
        let items = valleys(&[(100, 25), (105, 28), (99, 27)]);
        assert_eq!(bullish(&items), vec![(d(100), d(99))]);
    }

    #[test]
    fn the_first_low_is_consumed_by_a_divergence() {
        // A (25) -> B (98, 28): signal. C (96, 29) is lower with a higher RSI
        // than B, but no search is running (B does not start one).
        let items = valleys(&[(100, 25), (98, 28), (96, 29)]);
        assert_eq!(bullish(&items), vec![(d(100), d(98))]);
    }

    #[test]
    fn any_level_with_100_and_0() {
        let any = DivergenceParams {
            rsi_low: d(100),
            rsi_high: d(0),
            ..params()
        };
        let items = valleys(&[(100, 55), (98, 58)]);
        let found: Vec<_> = run(any, &items)
            .into_iter()
            .filter(|(k, ..)| *k == DivergenceKind::Bullish)
            .collect();
        assert_eq!(found, vec![(DivergenceKind::Bullish, d(100), d(98))]);
        assert!(bullish(&items).is_empty(), "not with the default level 30");
    }

    #[test]
    fn highs_are_the_mirror() {
        // Mirrored valleys: highs at 400 (RSI 75) then 402 (RSI 72).
        let items = mirror(&valleys(&[(100, 25), (98, 28)]));
        let bearish: Vec<_> = run(params(), &items)
            .into_iter()
            .filter(|(k, ..)| *k == DivergenceKind::Bearish)
            .collect();
        assert_eq!(bearish, vec![(DivergenceKind::Bearish, d(400), d(402))]);
        // First high not overbought (RSI 65 < 70): nothing.
        let weak = mirror(&valleys(&[(100, 35), (98, 40)]));
        assert!(
            run(params(), &weak)
                .iter()
                .all(|(k, ..)| *k != DivergenceKind::Bearish)
        );
    }

    #[test]
    fn min_gap_is_respected() {
        let far = DivergenceParams {
            min_gap: TimeDelta::minutes(10), // valleys are 5 minutes apart
            ..params()
        };
        let items = valleys(&[(100, 25), (98, 28)]);
        assert!(run(far, &items).is_empty());
    }

    #[test]
    fn default_gaps_depend_on_the_timeframe() {
        let p = DivergenceParams::for_timeframe;
        assert_eq!(p(Timeframe::M1).min_gap, TimeDelta::minutes(5));
        assert_eq!(p(Timeframe::M5).min_gap, TimeDelta::minutes(15));
        assert_eq!(p(Timeframe::M15).min_gap, TimeDelta::minutes(30));
        assert_eq!(p(Timeframe::H1).min_gap, TimeDelta::hours(2));
        assert_eq!(DivergenceParams::default(), p(Timeframe::M1));
        let defaults = p(Timeframe::M15);
        assert_eq!((defaults.rsi_low, defaults.rsi_high), (d(30), d(70)));
        assert_eq!(defaults.lookahead, 3);
    }

    #[test]
    fn rejects_invalid_params() {
        let p = params();
        let bad = [
            DivergenceParams { left: 0, ..p },
            DivergenceParams { right: 0, ..p },
            DivergenceParams {
                min_gap: TimeDelta::minutes(-1),
                ..p
            },
            DivergenceParams { lookahead: 0, ..p },
            DivergenceParams {
                rsi_low: d(101),
                ..p
            },
            DivergenceParams {
                rsi_high: d(-1),
                ..p
            },
        ];
        for b in bad {
            assert!(RsiDivergence::new(b).is_err(), "{b:?}");
        }
        assert!(RsiDivergence::new(p).is_ok());
    }

    #[test]
    fn pivots_are_confirmed_right_candles_later() {
        // One valley (low 100, RSI 25): the low pivot is confirmed 2 candles
        // after it (right = 2), with its time, price and RSI.
        let items = valleys(&[(100, 25)]);
        let mut pivots = Pivots::new(2, 2);
        assert_eq!(pivots.warm_up(), 5);
        let confirmed: Vec<(usize, Pivot)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| pivots.push(it).low.map(|p| (i, p)))
            .collect();
        assert_eq!(confirmed.len(), 1);
        let (i, p) = confirmed[0];
        assert_eq!(i, 4, "valley at index 2, confirmed at index 4");
        assert_eq!((p.price, p.rsi), (d(100), d(25)));
        assert_eq!(p.time, items[2].data.open_time);
        // Highs: the mirrored valley gives one high pivot.
        let mut pivots = Pivots::new(2, 2);
        let highs = mirror(&items)
            .iter()
            .filter_map(|it| pivots.push(it).high)
            .count();
        assert_eq!(highs, 1);
    }

    #[test]
    fn any_level_for_indicators_without_levels() {
        // OBV-like values: lower low with a higher indicator (5000 -> 6000).
        let items = valleys(&[(100, 5000), (98, 6000)]);
        let count = |mut c: RsiDivergence| {
            items
                .iter()
                .filter_map(|data| {
                    c.on_data(Item {
                        data,
                        mode: Mode::Replay,
                    })
                })
                .count()
        };
        // With the RSI levels (30 / 70), 5000 is never a valid first low.
        assert_eq!(count(RsiDivergence::new(params()).unwrap()), 0);
        assert_eq!(count(RsiDivergence::any_level(params()).unwrap()), 1);
        // The other checks still apply.
        let bad = DivergenceParams {
            lookahead: 0,
            ..params()
        };
        assert!(RsiDivergence::any_level(bad).is_err());
    }
}
