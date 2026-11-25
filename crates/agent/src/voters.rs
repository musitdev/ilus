//! Voters (spec F9): one condition per rule, emitting its [`Ballots`] (open,
//! revoke, exit) on fixed criteria; their parameters are genes.

use rust_decimal::Decimal;

use crate::{Ballot, Ballots, Side};
use trading::candle::{Candle, CandleData};
use trading::condition::divergence::{DivergenceKind, DivergenceParams, Pivots, RsiDivergence};
use trading::condition::ema_rules::{EmaCross, EmaPair};
use trading::condition::patterns::PatternCondition;
use trading::condition::volume::VolumeSpike;
use trading::condition::{Condition, Signal};
use trading::error::{ModifierError, ParameterError};
use trading::modifier::NoModifier;
use trading::source::Item;
use trading::transformer::Derived;
use trading::transformer::rsi::RsiCandle;
use trading::transformer::trend::TrendValue;
use trading::trigger::{Detection, TriggerEventType};

fn side(signal: Signal) -> Side {
    match signal {
        Signal::Bullish => Side::Long,
        Signal::Bearish => Side::Short,
    }
}

/// The detection for `ballots`, if any.
fn detection(event_type: TriggerEventType, ballots: Vec<Ballot>) -> Option<Detection<Ballots>> {
    if ballots.is_empty() {
        return None;
    }
    Some(Detection {
        event_type,
        value: Ballots(ballots),
    })
}

/// An open ballot invalidated when the close breaks `level` (below for a
/// long, above for a short).
#[derive(Debug, Clone, Copy)]
struct Active {
    side: Side,
    level: Decimal,
}

impl Active {
    fn broken(&self, close: Decimal) -> bool {
        match self.side {
            Side::Long => close < self.level,
            Side::Short => close > self.level,
        }
    }
}

macro_rules! no_modifier {
    () => {
        fn on_modifier(
            &mut self,
            m: NoModifier,
        ) -> Result<Option<Detection<Ballots>>, ModifierError> {
            match m {}
        }
    };
}

// --------------------------------------------------------------- divergence

/// RSI divergence rule.
/// - Open: bullish divergence → long, bearish → short.
/// - Revoke: the close breaks the B pivot (below its low for a long, above
///   its high for a short).
/// - Exit: a high pivot with RSI above `exit_high` → exit long; a low pivot
///   with RSI below `exit_low` → exit short.
pub struct DivergenceVoter {
    divergence: RsiDivergence,
    pivots: Pivots,
    /// RSI exit levels (high, low); `None`: no exit ballot.
    exits: Option<(Decimal, Decimal)>,
    active: Option<Active>,
}

impl DivergenceVoter {
    /// Divergences with `params`, RSI exits at `exit_high` / `exit_low`
    /// (within 0..=100; 100 / 0: never).
    pub fn new(
        params: DivergenceParams,
        exit_high: Decimal,
        exit_low: Decimal,
    ) -> Result<Self, ParameterError> {
        let rsi = Decimal::ZERO..=Decimal::ONE_HUNDRED;
        if !rsi.contains(&exit_high) || !rsi.contains(&exit_low) {
            return Err(ParameterError::Invalid(
                "divergence voter: exit levels must be within 0..=100".into(),
            ));
        }
        Ok(Self {
            divergence: RsiDivergence::new(params)?,
            pivots: Pivots::new(params.left, params.right),
            exits: Some((exit_high, exit_low)),
            active: None,
        })
    }

    /// Divergences with an indicator that has no levels (e.g. the OBV):
    /// any pivot can start a divergence (`rsi_low` / `rsi_high` ignored),
    /// and no exit ballot (the indicator has no overbought / oversold).
    pub fn indicator(params: DivergenceParams) -> Result<Self, ParameterError> {
        Ok(Self {
            divergence: RsiDivergence::any_level(params)?,
            pivots: Pivots::new(params.left, params.right),
            exits: None,
            active: None,
        })
    }
}

impl Condition for DivergenceVoter {
    type Input = RsiCandle;
    type Value = Ballots;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.divergence.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Ballots>> {
        let pivots = self.pivots.push(item.data);
        let found = self.divergence.on_data(item);
        let mut ballots = Vec::new();
        let mut event_type = TriggerEventType::Other("ballots");
        if let Some(d) = found {
            let s = match d.value.kind {
                DivergenceKind::Bullish => Side::Long,
                DivergenceKind::Bearish => Side::Short,
            };
            self.active = Some(Active {
                side: s,
                level: d.value.second.price,
            });
            event_type = d.event_type;
            ballots.push(Ballot::Open(s));
        } else if self.active.is_some_and(|a| a.broken(item.data.data.close)) {
            self.active = None;
            ballots.push(Ballot::Revoke);
        }
        if let Some((high, low)) = self.exits {
            if pivots.high.is_some_and(|p| p.rsi > high) {
                ballots.push(Ballot::Exit(Side::Long));
            }
            if pivots.low.is_some_and(|p| p.rsi < low) {
                ballots.push(Ballot::Exit(Side::Short));
            }
        }
        detection(event_type, ballots)
    }

    no_modifier!();
}

// ---------------------------------------------------------------- EMA cross

/// Fast / slow EMA cross rule.
/// - Open: cross up → long, cross down → short (the opposite cross replaces
///   the vote).
/// - Exit: cross down → exit long, cross up → exit short.
#[derive(Debug, Clone, Default)]
pub struct EmaCrossVoter {
    cross: EmaCross,
}

impl Condition for EmaCrossVoter {
    type Input = EmaPair;
    type Value = Ballots;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.cross.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, EmaPair>) -> Option<Detection<Ballots>> {
        let d = self.cross.on_data(item)?;
        let s = side(d.value.signal);
        detection(
            d.event_type,
            vec![Ballot::Open(s), Ballot::Exit(s.opposite())],
        )
    }

    no_modifier!();
}

// -------------------------------------------------------------------- trend

/// Trend rule (on the output of a [`Trend`](trading::transformer::trend::Trend)
/// transformer), on changes only.
/// - Open: the trend becomes up → long, down → short.
/// - Revoke: the trend becomes flat.
/// - Exit: the trend becomes down → exit long, up → exit short.
pub struct TrendVoter<I> {
    last: Option<Option<Signal>>,
    input: std::marker::PhantomData<fn(I)>,
}

impl<I> Default for TrendVoter<I> {
    fn default() -> Self {
        Self {
            last: None,
            input: std::marker::PhantomData,
        }
    }
}

impl<I: CandleData> Condition for TrendVoter<I> {
    type Input = Derived<I, TrendValue>;
    type Value = Ballots;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<Ballots>> {
        let now = item.data.value?.side;
        if self.last == Some(now) {
            return None;
        }
        self.last = Some(now);
        let ballots = match now {
            Some(signal) => {
                let s = side(signal);
                vec![Ballot::Open(s), Ballot::Exit(s.opposite())]
            }
            None => vec![Ballot::Revoke],
        };
        detection(TriggerEventType::Other("trend"), ballots)
    }

    no_modifier!();
}

// ----------------------------------------------------------------- patterns

/// Candlestick patterns rule.
/// - Open: bullish pattern(s) → long, bearish → short (both directions on
///   the same candle: no ballot); the opposite replaces the vote.
/// - Revoke: the close breaks the low (long) / high (short) of the pattern's
///   last candle.
/// - Exit: a bearish pattern → exit long, a bullish one → exit short.
pub struct PatternVoter {
    patterns: PatternCondition,
    active: Option<Active>,
}

impl PatternVoter {
    /// Vote on the patterns detected by `patterns`.
    pub fn new(patterns: PatternCondition) -> Self {
        Self {
            patterns,
            active: None,
        }
    }
}

impl Condition for PatternVoter {
    type Input = Candle;
    type Value = Ballots;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.patterns.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<Ballots>> {
        let c = item.data;
        if let Some(d) = self.patterns.on_data(item) {
            let bull = d.value.0.iter().any(|m| m.signal == Signal::Bullish);
            let bear = d.value.0.iter().any(|m| m.signal == Signal::Bearish);
            let s = match (bull, bear) {
                (true, false) => Side::Long,
                (false, true) => Side::Short,
                _ => return None,
            };
            let level = match s {
                Side::Long => c.low,
                Side::Short => c.high,
            };
            self.active = Some(Active { side: s, level });
            return detection(
                d.event_type,
                vec![Ballot::Open(s), Ballot::Exit(s.opposite())],
            );
        }
        if self.active.is_some_and(|a| a.broken(c.close)) {
            self.active = None;
            return detection(TriggerEventType::Other("ballots"), vec![Ballot::Revoke]);
        }
        None
    }

    no_modifier!();
}

// ------------------------------------------------------------ volume spike

/// Volume spike rule (input: candles + their mean volume, see
/// [`VolumeMean`](trading::transformer::volume::VolumeMean)).
/// - Open: a spike on a green candle → long, on a red candle → short; the
///   opposite replaces the vote.
/// - Revoke: the close breaks the low (long) / high (short) of the spike
///   candle.
/// - Exit: an opposite spike.
pub struct VolumeSpikeVoter {
    spike: VolumeSpike,
    active: Option<Active>,
}

impl VolumeSpikeVoter {
    /// Vote on the spikes detected by `spike`.
    pub fn new(spike: VolumeSpike) -> Self {
        Self {
            spike,
            active: None,
        }
    }
}

impl Condition for VolumeSpikeVoter {
    type Input = Derived<Candle, Decimal>;
    type Value = Ballots;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        self.spike.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<Ballots>> {
        let c = &item.data.data;
        if let Some(d) = self.spike.on_data(item) {
            let s = side(d.value.signal);
            let level = match s {
                Side::Long => c.low,
                Side::Short => c.high,
            };
            self.active = Some(Active { side: s, level });
            return detection(
                d.event_type,
                vec![Ballot::Open(s), Ballot::Exit(s.opposite())],
            );
        }
        if self.active.is_some_and(|a| a.broken(c.close)) {
            self.active = None;
            return detection(TriggerEventType::Other("ballots"), vec![Ballot::Revoke]);
        }
        None
    }

    no_modifier!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;
    use trading::condition::patterns::{Pattern, ThresholdSet};
    use trading::source::Mode;
    use trading::transformer::Transformer;
    use trading::transformer::trend::Trend;

    use Ballot::{Exit, Open, Revoke};
    use Side::{Long, Short};

    fn time(i: usize) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i as i64)
    }

    /// Feed `items`; (index, ballots) of every detection.
    fn run<C: Condition<Value = Ballots>>(
        c: &mut C,
        items: &[C::Input],
    ) -> Vec<(usize, Vec<Ballot>)> {
        items
            .iter()
            .enumerate()
            .filter_map(|(i, data)| {
                c.on_data(Item {
                    data,
                    mode: Mode::Replay,
                })
                .map(|d| (i, d.value.0))
            })
            .collect()
    }

    /// Candles + RSI from (low, rsi): high = low + 1, close = low + 0.5.
    fn rsi_bars(spec: &[(i64, i64)]) -> Vec<RsiCandle> {
        spec.iter()
            .enumerate()
            .map(|(i, &(low, rsi))| {
                let low = Decimal::from(low);
                Derived {
                    data: Candle {
                        open_time: time(i),
                        open: low + dec!(0.5),
                        high: low + dec!(1),
                        low,
                        close: low + dec!(0.5),
                        volume: dec!(1),
                    },
                    value: Some(Decimal::from(rsi)),
                }
            })
            .collect()
    }

    #[test]
    fn divergence_voter_opens_revokes_and_exits() {
        let params = DivergenceParams {
            left: 2,
            right: 2,
            min_gap: TimeDelta::minutes(1),
            ..DivergenceParams::default()
        };
        let mut v = DivergenceVoter::new(params, dec!(75), dec!(30)).unwrap();
        let items = rsi_bars(&[
            // valley A: low 100, RSI 20 (index 2, confirmed at 4)
            (104, 50),
            (102, 50),
            (100, 20),
            (102, 50),
            (104, 50),
            // valley B: lower low 95, higher RSI 25 (index 12, confirmed at 14)
            (106, 50),
            (106, 50),
            (106, 50),
            (106, 50),
            (106, 50),
            (99, 50),
            (97, 50),
            (95, 25),
            (97, 50),
            (99, 50),
            // close 94.5 < B's low 95: revoked
            (94, 50),
            // peak: high 101 with RSI 80 (index 18, confirmed at 20)
            (96, 50),
            (98, 50),
            (100, 80),
            (98, 50),
            (96, 50),
        ]);
        assert_eq!(
            run(&mut v, &items),
            vec![
                (4, vec![Exit(Short)]),              // low pivot, RSI 20 < 30
                (14, vec![Open(Long), Exit(Short)]), // bullish divergence
                (15, vec![Revoke]),
                (20, vec![Exit(Long)]), // high pivot, RSI 80 > 75
            ]
        );
        assert!(DivergenceVoter::new(params, dec!(101), dec!(30)).is_err());
    }

    fn ema_pair(i: usize, fast: i64, slow: i64) -> EmaPair {
        Derived {
            data: Candle {
                open_time: time(i),
                open: dec!(1),
                high: dec!(1),
                low: dec!(1),
                close: dec!(1),
                volume: dec!(1),
            },
            value: Some((Some(Decimal::from(fast)), Some(Decimal::from(slow)))),
        }
    }

    #[test]
    fn ema_cross_voter_opens_and_exits_on_crosses() {
        let items: Vec<EmaPair> = [(1, 2), (3, 2), (4, 2), (1, 2)]
            .iter()
            .enumerate()
            .map(|(i, &(f, s))| ema_pair(i, f, s))
            .collect();
        assert_eq!(
            run(&mut EmaCrossVoter::default(), &items),
            vec![
                (1, vec![Open(Long), Exit(Short)]),
                (3, vec![Open(Short), Exit(Long)]),
            ]
        );
    }

    #[test]
    fn trend_voter_on_changes_only() {
        // EMA 2 of the closes 10, 10 | 13, 14 (up) | 9 (down) | then equal.
        let mut trend = Trend::<Candle>::new(2).unwrap();
        let items: Vec<Derived<Candle, TrendValue>> = [10, 10, 13, 14, 9]
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let c = Decimal::from(c);
                let data = Candle {
                    open_time: time(i),
                    open: c,
                    high: c,
                    low: c,
                    close: c,
                    volume: dec!(1),
                };
                let value = trend.on_data(Item {
                    data: &data,
                    mode: Mode::Replay,
                });
                Derived { data, value }
            })
            .collect();
        assert_eq!(
            run(&mut TrendVoter::default(), &items),
            vec![
                (1, vec![Revoke]), // close = EMA: flat
                (2, vec![Open(Long), Exit(Short)]),
                (4, vec![Open(Short), Exit(Long)]),
            ]
        );
    }

    /// 25 candles in a downtrend (closes 200, 199, ... 176), then `extra`
    /// (open, high, low, close).
    fn pattern_series(extra: &[(Decimal, Decimal, Decimal, Decimal)]) -> Vec<Candle> {
        let mut out: Vec<Candle> = (0..25)
            .map(|i| {
                let close = dec!(200) - Decimal::from(i);
                Candle {
                    open_time: time(i as usize),
                    open: close + dec!(1),
                    high: close + dec!(1.5),
                    low: close - dec!(0.5),
                    close,
                    volume: dec!(1),
                }
            })
            .collect();
        for &(open, high, low, close) in extra {
            out.push(Candle {
                open_time: time(out.len()),
                open,
                high,
                low,
                close,
                volume: dec!(1),
            });
        }
        out
    }

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

    #[test]
    fn pattern_voter_opens_and_revokes_on_the_pattern_extreme() {
        // Hammer after the downtrend (P = 176): body 0.2, lower shadow 1.8;
        // then a close above its low (kept), then below it (revoked).
        let series = pattern_series(&[
            (dec!(176), dec!(176.21), dec!(174.2), dec!(176.2)),
            (dec!(176), dec!(176.5), dec!(174.5), dec!(175)),
            (dec!(175), dec!(175.5), dec!(173.5), dec!(174)),
        ]);
        let only = |p: Pattern| {
            PatternVoter::new(
                PatternCondition::new(ThresholdSet::default())
                    .unwrap()
                    .only(&[p]),
            )
        };
        assert_eq!(
            run(&mut only(Pattern::Hammer), &series),
            vec![(25, vec![Open(Long), Exit(Short)]), (27, vec![Revoke])]
        );
        // Mirror: a shooting star after an uptrend, revoked above its high.
        assert_eq!(
            run(&mut only(Pattern::ShootingStar), &mirror(&series)),
            vec![(25, vec![Open(Short), Exit(Long)]), (27, vec![Revoke])]
        );
    }

    #[test]
    fn volume_spike_voter_opens_revokes_and_exits() {
        // (open, close, volume, mean): low = min(open, close).
        let bars = [
            (10, 11, 100, Some(100)), // no spike
            (11, 13, 300, Some(100)), // green spike: long, level = low 11
            (13, 12, 100, Some(150)), // close 12 >= 11: kept
            (12, 10, 100, Some(150)), // close 10 < 11: revoked
            (10, 9, 500, Some(150)),  // red spike: short (exit long)
        ];
        let items: Vec<Derived<Candle, Decimal>> = bars
            .iter()
            .enumerate()
            .map(|(i, &(o, c, v, m))| {
                let (o, c) = (Decimal::from(o), Decimal::from(c));
                Derived {
                    data: Candle {
                        open_time: time(i),
                        open: o,
                        high: o.max(c),
                        low: o.min(c),
                        close: c,
                        volume: Decimal::from(v),
                    },
                    value: m.map(Decimal::from),
                }
            })
            .collect();
        let mut v = VolumeSpikeVoter::new(VolumeSpike::new(dec!(2)).unwrap());
        assert_eq!(
            run(&mut v, &items),
            vec![
                (1, vec![Open(Long), Exit(Short)]),
                (3, vec![Revoke]),
                (4, vec![Open(Short), Exit(Long)]),
            ]
        );
    }
}
