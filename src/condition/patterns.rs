//! Candlestick pattern condition: the 13 patterns of spec F0.
//!
//! - Measures: body, range, shadows, midpoint; typical body / range (mean of
//!   the [`ThresholdSet::avg_len`] candles before the last one); prior trend
//!   (close vs its EMA of period [`ThresholdSet::trend_len`], read on the
//!   candle preceding the first candle of the pattern).
//! - Thresholds are data ([`ThresholdSet`]), not hard-coded in the patterns.
//! - Comparisons use a tolerance of `epsilon × price`, so exact threshold hits
//!   are handled consistently.
//! - Several patterns can match the same candle: one detection lists them all
//!   (its event type is the first match, in table order).

use std::collections::VecDeque;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::candle::Candle;
use crate::condition::Condition;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::ModifierValue;
use crate::source::Item;
use crate::trigger::{Detection, TriggerEventType, TriggerValue};

/// Thresholds of the pattern definitions (spec F0). Fields are public, so a
/// single value can be overridden: `ThresholdSet { long_shadow: dec!(3), ..Default::default() }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThresholdSet {
    /// Candles used for the typical body / range.
    pub avg_len: usize,
    /// EMA period of the prior trend.
    pub trend_len: usize,
    /// A "long" shadow is at least this many times the body.
    pub long_shadow: Decimal,
    /// A dominant shadow covers at least this share of the range.
    pub shadow_share: Decimal,
    /// A "missing" shadow is at most this share of the range.
    pub tiny_shadow: Decimal,
    /// Max body / range ratio of a doji.
    pub doji_body: Decimal,
    /// A doji's range is at least this share of the typical range.
    pub doji_min_range: Decimal,
    /// Max body of a star's middle candle, relative to the first body.
    pub star_body: Decimal,
    /// Relative equality tolerance (× price).
    pub epsilon: Decimal,
}

impl Default for ThresholdSet {
    /// The `default` set: the most commonly used values.
    fn default() -> Self {
        Self {
            avg_len: 10,
            trend_len: 20,
            long_shadow: dec!(2),
            shadow_share: dec!(0.6),
            tiny_shadow: dec!(0.1),
            doji_body: dec!(0.05),
            doji_min_range: dec!(0.5),
            star_body: dec!(0.3),
            epsilon: dec!(0.000000001),
        }
    }
}

impl ThresholdSet {
    fn validate(&self) -> Result<(), ParameterError> {
        if self.avg_len == 0 || self.trend_len == 0 {
            return Err(ParameterError::Invalid(
                "patterns: avg_len and trend_len must be >= 1".into(),
            ));
        }
        if self.epsilon < Decimal::ZERO {
            return Err(ParameterError::Invalid(
                "patterns: epsilon must be >= 0".into(),
            ));
        }
        Ok(())
    }
}

/// Direction of a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// Possible rise.
    Bullish,
    /// Possible fall.
    Bearish,
}

/// The 13 patterns, in table order (spec F0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Pattern {
    /// Bullish Engulfing (2 candles).
    BullishEngulfing,
    /// Bearish Engulfing (2 candles).
    BearishEngulfing,
    /// Hammer (1 candle).
    Hammer,
    /// Hanging Man (1 candle).
    HangingMan,
    /// Inverted Hammer (1 candle).
    InvertedHammer,
    /// Shooting Star (1 candle).
    ShootingStar,
    /// Doji (1 candle); signal opposite to the prior trend.
    Doji,
    /// Morning Star (3 candles).
    MorningStar,
    /// Evening Star (3 candles).
    EveningStar,
    /// Bullish Harami (2 candles).
    BullishHarami,
    /// Bearish Harami (2 candles).
    BearishHarami,
    /// Piercing Line (2 candles).
    PiercingLine,
    /// Dark Cloud Cover (2 candles).
    DarkCloudCover,
}

impl Pattern {
    /// Every pattern, in table order.
    pub const ALL: [Self; 13] = [
        Self::BullishEngulfing,
        Self::BearishEngulfing,
        Self::Hammer,
        Self::HangingMan,
        Self::InvertedHammer,
        Self::ShootingStar,
        Self::Doji,
        Self::MorningStar,
        Self::EveningStar,
        Self::BullishHarami,
        Self::BearishHarami,
        Self::PiercingLine,
        Self::DarkCloudCover,
    ];

    /// Number of candles of the pattern.
    pub fn candles(self) -> usize {
        match self {
            Self::Hammer
            | Self::HangingMan
            | Self::InvertedHammer
            | Self::ShootingStar
            | Self::Doji => 1,
            Self::MorningStar | Self::EveningStar => 3,
            _ => 2,
        }
    }

    /// Trigger event type of the pattern.
    pub fn event_type(self) -> TriggerEventType {
        match self {
            Self::BullishEngulfing => TriggerEventType::BullishEngulfing,
            Self::BearishEngulfing => TriggerEventType::BearishEngulfing,
            Self::Hammer => TriggerEventType::Hammer,
            Self::HangingMan => TriggerEventType::HangingMan,
            Self::InvertedHammer => TriggerEventType::InvertedHammer,
            Self::ShootingStar => TriggerEventType::ShootingStar,
            Self::Doji => TriggerEventType::Doji,
            Self::MorningStar => TriggerEventType::MorningStar,
            Self::EveningStar => TriggerEventType::EveningStar,
            Self::BullishHarami => TriggerEventType::BullishHarami,
            Self::BearishHarami => TriggerEventType::BearishHarami,
            Self::PiercingLine => TriggerEventType::PiercingLine,
            Self::DarkCloudCover => TriggerEventType::DarkCloudCover,
        }
    }
}

/// One matched pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternMatch {
    /// The pattern.
    pub pattern: Pattern,
    /// Its signal.
    pub signal: Signal,
}

/// Value of a detection: every pattern matching the candle, in table order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternMatches(pub Vec<PatternMatch>);

impl TriggerValue for PatternMatches {}

/// Modifiers accepted by [`PatternCondition`].
#[derive(Debug, Clone, PartialEq)]
pub enum PatternModifier {
    /// Only report patterns with this signal (`None`: both).
    WatchOnly(Option<Signal>),
    /// Use another threshold set. `avg_len` and `trend_len` cannot change
    /// while running (they size the history): such a set is rejected.
    SetThresholds(ThresholdSet),
}

impl ModifierValue for PatternModifier {}

/// Prior trend of a candle: close vs its EMA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trend {
    Up,
    Down,
    Flat,
}

/// A candle with its trend (`None` until the EMA is defined).
#[derive(Debug, Clone)]
struct Bar {
    c: Candle,
    trend: Option<Trend>,
}

/// Measures of a candle.
fn body(c: &Candle) -> Decimal {
    (c.close - c.open).abs()
}
fn range(c: &Candle) -> Decimal {
    c.high - c.low
}
fn upper(c: &Candle) -> Decimal {
    c.high - c.open.max(c.close)
}
fn lower(c: &Candle) -> Decimal {
    c.open.min(c.close) - c.low
}
fn mid(c: &Candle) -> Decimal {
    (c.open + c.close) / dec!(2)
}

/// Comparisons with a tolerance (`a` vs `b`, equal within `eps`).
#[derive(Clone, Copy)]
struct Cmp {
    eps: Decimal,
}

impl Cmp {
    fn ge(self, a: Decimal, b: Decimal) -> bool {
        a >= b - self.eps
    }
    fn le(self, a: Decimal, b: Decimal) -> bool {
        a <= b + self.eps
    }
    fn gt(self, a: Decimal, b: Decimal) -> bool {
        a > b + self.eps
    }
    fn lt(self, a: Decimal, b: Decimal) -> bool {
        a < b - self.eps
    }
    fn bullish(self, c: &Candle) -> bool {
        self.gt(c.close, c.open)
    }
    fn bearish(self, c: &Candle) -> bool {
        self.lt(c.close, c.open)
    }
}

/// Detects the 13 candlestick patterns on closed candles.
#[derive(Debug, Clone)]
pub struct PatternCondition {
    t: ThresholdSet,
    /// Patterns watched.
    watched: Vec<Pattern>,
    /// Signal watched (`None`: both).
    signal: Option<Signal>,
    /// Last candles (enough for the averages and the 3-candle patterns).
    bars: VecDeque<Bar>,
    /// EMA of the closes, and the closes seen before it is seeded.
    ema: Option<Decimal>,
    seed: Vec<Decimal>,
}

impl PatternCondition {
    /// Watch the 13 patterns with the given thresholds.
    pub fn new(thresholds: ThresholdSet) -> Result<Self, ParameterError> {
        thresholds.validate()?;
        Ok(Self {
            bars: VecDeque::with_capacity(thresholds.avg_len + 4),
            t: thresholds,
            watched: Pattern::ALL.to_vec(),
            signal: None,
            ema: None,
            seed: Vec::new(),
        })
    }

    /// Only watch these patterns (e.g. one condition per pattern).
    pub fn only(mut self, patterns: &[Pattern]) -> Self {
        self.watched = Pattern::ALL
            .into_iter()
            .filter(|p| patterns.contains(p))
            .collect();
        self
    }

    /// Update the EMA with `close`; returns the trend of this candle.
    fn update_trend(&mut self, close: Decimal, cmp: Cmp) -> Option<Trend> {
        let n = self.t.trend_len;
        let ema = match self.ema {
            Some(prev) => {
                let k = dec!(2) / Decimal::from(n + 1);
                prev + k * (close - prev)
            }
            None => {
                self.seed.push(close);
                if self.seed.len() < n {
                    return None;
                }
                // Seeded with the mean of the first `trend_len` closes.
                self.seed.iter().sum::<Decimal>() / Decimal::from(n)
            }
        };
        self.ema = Some(ema);
        Some(if cmp.lt(close, ema) {
            Trend::Down
        } else if cmp.gt(close, ema) {
            Trend::Up
        } else {
            Trend::Flat
        })
    }

    /// The bar `back` candles before the last one (0 = last).
    fn bar(&self, back: usize) -> Option<&Bar> {
        self.bars
            .len()
            .checked_sub(back + 1)
            .and_then(|i| self.bars.get(i))
    }

    /// Trend on the candle preceding the first candle of a pattern.
    fn prior_trend(&self, pattern: Pattern) -> Option<Trend> {
        self.bar(pattern.candles())?.trend
    }

    /// Typical body and range: means over the `avg_len` candles before the last.
    fn typical(&self) -> Option<(Decimal, Decimal)> {
        let n = self.t.avg_len;
        let last = self.bars.len().checked_sub(1)?;
        let first = last.checked_sub(n)?;
        let window = self.bars.range(first..last);
        let (mut b, mut r) = (Decimal::ZERO, Decimal::ZERO);
        for bar in window {
            b += body(&bar.c);
            r += range(&bar.c);
        }
        let n = Decimal::from(n);
        Some((b / n, r / n))
    }

    /// Does `pattern` match the last candle(s)? Returns its signal.
    fn matches(&self, pattern: Pattern, cmp: Cmp) -> Option<Signal> {
        let t = &self.t;
        let trend = self.prior_trend(pattern)?;
        let (typical_body, typical_range) = self.typical()?;
        let k = &self.bar(0)?.c; // last candle
        let (b, r) = (body(k), range(k));
        let zero = Decimal::ZERO;
        let down = trend == Trend::Down;
        let up = trend == Trend::Up;

        // Shapes of the 1-candle patterns.
        let hammer_shape = || {
            cmp.gt(r, zero)
                && cmp.ge(lower(k), t.long_shadow * b)
                && cmp.ge(lower(k), t.shadow_share * r)
                && cmp.le(upper(k), t.tiny_shadow * r)
        };
        let inverted_shape = || {
            cmp.gt(r, zero)
                && cmp.ge(upper(k), t.long_shadow * b)
                && cmp.ge(upper(k), t.shadow_share * r)
                && cmp.le(lower(k), t.tiny_shadow * r)
        };

        let hit = match pattern {
            Pattern::Hammer => down && hammer_shape(),
            Pattern::HangingMan => up && hammer_shape(),
            Pattern::InvertedHammer => down && inverted_shape(),
            Pattern::ShootingStar => up && inverted_shape(),
            Pattern::Doji => {
                let doji = trend != Trend::Flat
                    && cmp.gt(r, zero)
                    && cmp.le(b, t.doji_body * r)
                    && cmp.ge(r, t.doji_min_range * typical_range);
                // Signal opposite to the prior trend.
                return doji.then_some(if down {
                    Signal::Bullish
                } else {
                    Signal::Bearish
                });
            }
            Pattern::BullishEngulfing | Pattern::BearishEngulfing => {
                let p = &self.bar(1)?.c;
                if pattern == Pattern::BullishEngulfing {
                    down && cmp.bearish(p)
                        && cmp.bullish(k)
                        && cmp.le(k.open, p.close)
                        && cmp.ge(k.close, p.open)
                        && cmp.gt(b, body(p))
                } else {
                    up && cmp.bullish(p)
                        && cmp.bearish(k)
                        && cmp.ge(k.open, p.close)
                        && cmp.le(k.close, p.open)
                        && cmp.gt(b, body(p))
                }
            }
            Pattern::BullishHarami | Pattern::BearishHarami => {
                let p = &self.bar(1)?.c;
                let large = cmp.ge(body(p), typical_body);
                if pattern == Pattern::BullishHarami {
                    down && cmp.bearish(p)
                        && large
                        && cmp.bullish(k)
                        && cmp.ge(k.open, p.close)
                        && cmp.le(k.close, p.open)
                        && cmp.lt(b, body(p))
                } else {
                    up && cmp.bullish(p)
                        && large
                        && cmp.bearish(k)
                        && cmp.le(k.open, p.close)
                        && cmp.ge(k.close, p.open)
                        && cmp.lt(b, body(p))
                }
            }
            Pattern::PiercingLine | Pattern::DarkCloudCover => {
                let p = &self.bar(1)?.c;
                let large = cmp.ge(body(p), typical_body);
                if pattern == Pattern::PiercingLine {
                    down && cmp.bearish(p)
                        && large
                        && cmp.bullish(k)
                        && cmp.le(k.open, p.close)
                        && cmp.gt(k.close, mid(p))
                        && cmp.lt(k.close, p.open)
                } else {
                    up && cmp.bullish(p)
                        && large
                        && cmp.bearish(k)
                        && cmp.ge(k.open, p.close)
                        && cmp.gt(k.close, p.open)
                        && cmp.lt(k.close, mid(p))
                }
            }
            Pattern::MorningStar | Pattern::EveningStar => {
                let q = &self.bar(2)?.c; // first
                let p = &self.bar(1)?.c; // middle
                let large = cmp.ge(body(q), typical_body);
                let small = cmp.le(body(p), t.star_body * body(q));
                if pattern == Pattern::MorningStar {
                    down && cmp.bearish(q)
                        && large
                        && small
                        && cmp.le(p.open.min(p.close), q.close)
                        && cmp.bullish(k)
                        && cmp.gt(k.close, mid(q))
                } else {
                    up && cmp.bullish(q)
                        && large
                        && small
                        && cmp.ge(p.open.max(p.close), q.close)
                        && cmp.bearish(k)
                        && cmp.lt(k.close, mid(q))
                }
            }
        };
        let signal = match pattern {
            Pattern::BullishEngulfing
            | Pattern::Hammer
            | Pattern::InvertedHammer
            | Pattern::MorningStar
            | Pattern::BullishHarami
            | Pattern::PiercingLine => Signal::Bullish,
            _ => Signal::Bearish,
        };
        hit.then_some(signal)
    }
}

impl Default for PatternCondition {
    /// The 13 patterns with the default threshold set.
    fn default() -> Self {
        let t = ThresholdSet::default();
        Self {
            bars: VecDeque::with_capacity(t.avg_len + 4),
            t,
            watched: Pattern::ALL.to_vec(),
            signal: None,
            ema: None,
            seed: Vec::new(),
        }
    }
}

impl Condition for PatternCondition {
    type Input = Candle;
    type Value = PatternMatches;
    type Modifier = PatternModifier;

    /// Trend defined on the candle preceding the first candle of a 3-candle
    /// pattern, and the typical body / range window.
    fn warm_up(&self) -> usize {
        (self.t.trend_len + 3).max(self.t.avg_len + 1)
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<PatternMatches>> {
        let c = item.data;
        let cmp = Cmp {
            eps: self.t.epsilon * c.close.abs(),
        };
        let trend = self.update_trend(c.close, cmp);
        self.bars.push_back(Bar {
            c: c.clone(),
            trend,
        });
        while self.bars.len() > self.t.avg_len.max(3) + 1 {
            self.bars.pop_front();
        }
        let matches: Vec<PatternMatch> = self
            .watched
            .iter()
            .filter_map(|&pattern| {
                let signal = self.matches(pattern, cmp)?;
                Some(PatternMatch { pattern, signal })
            })
            .filter(|m| self.signal.is_none_or(|s| s == m.signal))
            .collect();
        let first = matches.first()?.pattern;
        Some(Detection {
            event_type: first.event_type(),
            value: PatternMatches(matches),
        })
    }

    fn on_modifier(
        &mut self,
        modifier: PatternModifier,
    ) -> Result<Option<Detection<PatternMatches>>, ModifierError> {
        match modifier {
            PatternModifier::WatchOnly(signal) => self.signal = signal,
            PatternModifier::SetThresholds(t) => {
                if t.avg_len != self.t.avg_len || t.trend_len != self.t.trend_len {
                    return Err(ModifierError::Rejected(
                        "avg_len and trend_len cannot change while running".into(),
                    ));
                }
                t.validate()
                    .map_err(|e| ModifierError::Rejected(e.to_string()))?;
                self.t = t;
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use chrono::{DateTime, TimeDelta, Utc};

    /// A candle (open, high, low, close), minute `i`.
    fn c(i: i64, o: Decimal, h: Decimal, l: Decimal, cl: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            open: o,
            high: h,
            low: l,
            close: cl,
            volume: dec!(1),
        }
    }

    /// 25 candles in a clear downtrend: body 1, range 2, closes 200, 199, ...
    fn down_history() -> Vec<Candle> {
        (0..25)
            .map(|i| {
                let close = dec!(200) - Decimal::from(i);
                let open = close + dec!(1);
                c(i, open, open + dec!(0.5), close - dec!(0.5), close)
            })
            .collect()
    }

    /// Mirror prices around 500: a downtrend becomes an uptrend, a bullish
    /// shape the bearish one (highs and lows swap).
    fn mirror(candles: &[Candle]) -> Vec<Candle> {
        let m = |x: Decimal| dec!(500) - x;
        candles
            .iter()
            .map(|k| Candle {
                open: m(k.open),
                high: m(k.low),
                low: m(k.high),
                close: m(k.close),
                ..k.clone()
            })
            .collect()
    }

    /// Feed every candle; return the patterns detected on the last one.
    fn detect(candles: &[Candle]) -> Vec<Pattern> {
        let mut cond = PatternCondition::default();
        let mut last = Vec::new();
        for k in candles {
            last = cond
                .on_data(Item {
                    data: k,
                    mode: Mode::Replay,
                })
                .map(|d| d.value.0.iter().map(|m| m.pattern).collect())
                .unwrap_or_default();
        }
        last
    }

    /// History + pattern candles given as (o, h, l, c) relative to the last close P.
    fn with_pattern(pattern: &[(f64, f64, f64, f64)]) -> Vec<Candle> {
        let mut candles = down_history();
        let p = candles.last().map(|k| k.close).unwrap_or_default();
        let d = |x: f64| p + Decimal::try_from(x).unwrap();
        let start = candles.len() as i64;
        for (i, &(o, h, l, cl)) in pattern.iter().enumerate() {
            candles.push(c(start + i as i64, d(o), d(h), d(l), d(cl)));
        }
        candles
    }

    /// A bullish case on a downtrend, its mirror on an uptrend, and a
    /// non-matching variant of each.
    fn check(
        bullish: Pattern,
        bearish: Pattern,
        matching: &[(f64, f64, f64, f64)],
        not_matching: &[(f64, f64, f64, f64)],
    ) {
        let yes = with_pattern(matching);
        let no = with_pattern(not_matching);
        assert!(
            detect(&yes).contains(&bullish),
            "{bullish:?}: {:?}",
            detect(&yes)
        );
        assert!(
            detect(&mirror(&yes)).contains(&bearish),
            "{bearish:?}: {:?}",
            detect(&mirror(&yes))
        );
        assert!(!detect(&no).contains(&bullish), "not {bullish:?}");
        assert!(!detect(&mirror(&no)).contains(&bearish), "not {bearish:?}");
        // Wrong trend: the bullish pattern on the uptrend never matches.
        assert!(!detect(&mirror(&yes)).contains(&bullish));
    }

    #[test]
    fn engulfing() {
        check(
            Pattern::BullishEngulfing,
            Pattern::BearishEngulfing,
            // first bearish (P -> P-1), second bullish wrapping it (P-1.2 -> P+0.3)
            &[(0.0, 0.2, -1.2, -1.0), (-1.2, 0.4, -1.3, 0.3)],
            // second closes below the first open: no engulfing
            &[(0.0, 0.2, -1.2, -1.0), (-1.2, -0.1, -1.3, -0.2)],
        );
    }

    #[test]
    fn hammer_and_shooting_star() {
        check(
            Pattern::Hammer,
            Pattern::ShootingStar,
            // body 0.2, lower shadow 1.8, upper shadow 0.01
            &[(0.0, 0.21, -1.8, 0.2)],
            // upper shadow too long (0.5)
            &[(0.0, 0.7, -1.8, 0.2)],
        );
    }

    #[test]
    fn inverted_hammer_and_hanging_man() {
        check(
            Pattern::InvertedHammer,
            Pattern::HangingMan,
            // body 0.2, upper shadow 1.8, lower shadow 0.01
            &[(0.0, 2.0, -0.01, 0.2)],
            // body too large for the upper shadow (1.0 vs shadow 1.0)
            &[(0.0, 2.0, -0.01, 1.0)],
        );
    }

    #[test]
    fn morning_and_evening_star() {
        check(
            Pattern::MorningStar,
            Pattern::EveningStar,
            // large bearish (P -> P-3), small star below, bullish above the midpoint
            &[
                (0.0, 0.1, -3.1, -3.0),
                (-3.2, -3.0, -3.4, -3.1),
                (-3.0, -0.9, -3.1, -1.0),
            ],
            // third closes below the first midpoint (P-1.5)
            &[
                (0.0, 0.1, -3.1, -3.0),
                (-3.2, -3.0, -3.4, -3.1),
                (-3.0, -1.9, -3.1, -2.0),
            ],
        );
    }

    #[test]
    fn harami() {
        check(
            Pattern::BullishHarami,
            Pattern::BearishHarami,
            // large bearish (P -> P-3), bullish body inside it
            &[(0.0, 0.1, -3.1, -3.0), (-2.5, -0.9, -2.6, -1.0)],
            // second body goes above the first open
            &[(0.0, 0.1, -3.1, -3.0), (-2.5, 0.6, -2.6, 0.5)],
        );
    }

    #[test]
    fn piercing_line_and_dark_cloud_cover() {
        check(
            Pattern::PiercingLine,
            Pattern::DarkCloudCover,
            // large bearish (P -> P-3, midpoint P-1.5), opens below, closes P-1
            &[(0.0, 0.1, -3.1, -3.0), (-3.2, -0.9, -3.3, -1.0)],
            // closes below the midpoint
            &[(0.0, 0.1, -3.1, -3.0), (-3.2, -1.9, -3.3, -2.0)],
        );
    }

    #[test]
    fn doji_signal_is_opposite_to_the_trend() {
        // Body 0.02, range 2 (typical range 2).
        let yes = with_pattern(&[(0.0, 1.0, -1.0, 0.02)]);
        let mut cond = PatternCondition::default();
        let mut last = None;
        for k in &yes {
            last = cond.on_data(Item {
                data: k,
                mode: Mode::Replay,
            });
        }
        let m = last.unwrap().value.0;
        assert!(m.contains(&PatternMatch {
            pattern: Pattern::Doji,
            signal: Signal::Bullish
        }));
        let mut cond = PatternCondition::default();
        let mut last = None;
        for k in &mirror(&yes) {
            last = cond.on_data(Item {
                data: k,
                mode: Mode::Replay,
            });
        }
        assert!(last.unwrap().value.0.contains(&PatternMatch {
            pattern: Pattern::Doji,
            signal: Signal::Bearish
        }));
        // Range too small vs the typical range (0.5 < 0.5 x 2 is false: 0.4).
        assert!(!detect(&with_pattern(&[(0.0, 0.2, -0.2, 0.01)])).contains(&Pattern::Doji));
    }

    #[test]
    fn exact_threshold_hits_match() {
        // Lower shadow exactly 2 x body and exactly 0.6 x range; upper shadow
        // exactly 0.1 x range: body 0.6, range 2, lower 1.2... adjusted below.
        // body 0.3, range 1.0: lower 0.6 (= 2 x body = 0.6 x range), upper 0.1.
        let hammer = with_pattern(&[(0.0, 0.4, -0.6, 0.3)]);
        assert!(detect(&hammer).contains(&Pattern::Hammer));
    }

    #[test]
    fn zero_range_and_flat_trend_never_match() {
        // A zero-range candle after a downtrend.
        assert!(detect(&with_pattern(&[(0.0, 0.0, 0.0, 0.0)])).is_empty());
        // A flat history (close = EMA): trend-dependent patterns do not match.
        let flat: Vec<Candle> = (0..25)
            .map(|i| c(i, dec!(100), dec!(101), dec!(99), dec!(100)))
            .chain([c(25, dec!(100), dec!(100.21), dec!(98.2), dec!(100.2))]) // hammer shape
            .collect();
        assert!(detect(&flat).is_empty());
    }

    #[test]
    fn no_detection_before_the_warm_up() {
        let cond = PatternCondition::default();
        assert_eq!(cond.warm_up(), 23);
        // Only 5 candles of history: the trend is not defined yet.
        let mut candles: Vec<Candle> = down_history().into_iter().take(5).collect();
        let p = candles.last().map(|k| k.close).unwrap_or_default();
        candles.push(c(5, p, p + dec!(0.21), p - dec!(1.8), p + dec!(0.2)));
        assert!(detect(&candles).is_empty());
    }

    #[test]
    fn several_matches_are_reported_in_one_detection() {
        // Body 0.02, lower shadow 1.98, no upper shadow, after a bearish
        // candle of body 1 (= typical body): a hammer shape, a doji body, and
        // a tiny bullish body inside the previous one (harami).
        let both = with_pattern(&[(0.0, 0.02, -1.98, 0.02)]);
        let found = detect(&both);
        assert_eq!(
            found,
            vec![Pattern::Hammer, Pattern::Doji, Pattern::BullishHarami],
            "all matches, in table order"
        );
    }

    #[test]
    fn only_and_watch_only_filter_the_matches() {
        let both = with_pattern(&[(0.0, 0.02, -1.98, 0.02)]);
        let mut cond = PatternCondition::default().only(&[Pattern::Doji]);
        let mut last = None;
        for k in &both {
            last = cond.on_data(Item {
                data: k,
                mode: Mode::Replay,
            });
        }
        let d = last.unwrap();
        assert_eq!(d.event_type, TriggerEventType::Doji);
        assert_eq!(d.value.0.len(), 1);

        let mut cond = PatternCondition::default();
        cond.on_modifier(PatternModifier::WatchOnly(Some(Signal::Bearish)))
            .unwrap();
        let mut last = None;
        for k in &both {
            last = cond.on_data(Item {
                data: k,
                mode: Mode::Replay,
            });
        }
        assert!(last.is_none(), "only bullish patterns here");
    }

    #[test]
    fn thresholds_are_data_and_validated() {
        let strict = ThresholdSet {
            long_shadow: dec!(20),
            ..ThresholdSet::default()
        };
        let hammer = with_pattern(&[(0.0, 0.21, -1.8, 0.2)]);
        let mut cond = PatternCondition::new(strict).unwrap();
        let mut found = false;
        for k in &hammer {
            found = cond
                .on_data(Item {
                    data: k,
                    mode: Mode::Replay,
                })
                .is_some_and(|d| d.value.0.iter().any(|m| m.pattern == Pattern::Hammer));
        }
        assert!(!found, "lower shadow 9 x body < 20 x body");
        assert!(
            PatternCondition::new(ThresholdSet {
                avg_len: 0,
                ..ThresholdSet::default()
            })
            .is_err()
        );
        let mut cond = PatternCondition::default();
        let other_len = ThresholdSet {
            trend_len: 50,
            ..ThresholdSet::default()
        };
        assert!(
            cond.on_modifier(PatternModifier::SetThresholds(other_len))
                .is_err()
        );
    }
}
