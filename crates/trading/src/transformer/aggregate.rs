//! Aggregation transformer: builds candles of a longer timeframe (spec F4).
//!
//! For each input candle it emits the **last completed** candle of the longer
//! timeframe (repeated until the next one completes; `None` before the
//! first). Periods are aligned on the clock (00:00, 00:05, …). The candle
//! still forming is never sent, nor an incomplete period (e.g. when the data
//! starts in the middle of a period, or a candle is missing).

use chrono::{DateTime, Utc};

use crate::candle::Candle;
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::{Item, Timeframe};
use crate::transformer::Transformer;

/// Aggregates `from` candles into `to` candles (e.g. 1m → 5m).
#[derive(Debug, Clone)]
pub struct Aggregate {
    from: Timeframe,
    to: Timeframe,
    /// Candle being built, with the number of input candles in it.
    building: Option<(Candle, usize)>,
    last_done: Option<Candle>,
}

impl Aggregate {
    /// `to` must be a longer timeframe, multiple of `from`.
    pub fn new(from: Timeframe, to: Timeframe) -> Result<Self, ParameterError> {
        let (f, t) = (from.duration().num_seconds(), to.duration().num_seconds());
        if t <= f || t % f != 0 {
            return Err(ParameterError::Invalid(format!(
                "aggregate: {to} must be a multiple of {from}"
            )));
        }
        Ok(Self {
            from,
            to,
            building: None,
            last_done: None,
        })
    }

    /// Input candles per output candle.
    fn ratio(&self) -> usize {
        let ratio = self.to.duration().num_seconds() / self.from.duration().num_seconds();
        usize::try_from(ratio).unwrap_or(1)
    }

    /// Start of the `to` period containing `t`.
    fn period_start(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        let period = self.to.duration().num_seconds();
        let secs = t.timestamp();
        DateTime::from_timestamp(secs - secs.rem_euclid(period), 0).unwrap_or(t)
    }
}

impl Transformer for Aggregate {
    type Input = Candle;
    type Value = Candle;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("aggregate({}->{})", self.from, self.to)
    }

    fn warm_up(&self) -> usize {
        self.ratio()
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Candle> {
        let c = item.data;
        let start = self.period_start(c.open_time);
        match &mut self.building {
            // Same period: extend the candle being built.
            Some((b, count)) if b.open_time == start => {
                b.high = b.high.max(c.high);
                b.low = b.low.min(c.low);
                b.close = c.close;
                b.volume += c.volume;
                *count += 1;
            }
            // New period (an unfinished previous period is dropped).
            _ => {
                let first = Candle {
                    open_time: start,
                    ..c.clone()
                };
                self.building = Some((first, 1));
            }
        }
        // The input candle closing the period completes it, if no candle is missing.
        if c.open_time + self.from.duration() == start + self.to.duration()
            && let Some((candle, count)) = self.building.take()
            && count == self.ratio()
        {
            self.last_done = Some(candle);
        }
        self.last_done.clone()
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use chrono::TimeDelta;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn candle(min: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min),
            open: Decimal::from(open),
            high: Decimal::from(high),
            low: Decimal::from(low),
            close: Decimal::from(close),
            volume: dec!(1),
        }
    }

    fn feed(a: &mut Aggregate, c: &Candle) -> Option<Candle> {
        a.on_data(Item {
            data: c,
            mode: Mode::Replay,
        })
    }

    #[test]
    fn builds_closed_5m_candles_and_repeats_the_last_one() {
        let mut a = Aggregate::new(Timeframe::M1, Timeframe::M5).unwrap();
        assert_eq!(a.warm_up(), 5);
        let input = [
            candle(0, 10, 12, 9, 11),
            candle(1, 11, 15, 10, 14),
            candle(2, 14, 14, 8, 9),
            candle(3, 9, 10, 9, 10),
            candle(4, 10, 11, 10, 11), // closes 00:00-00:05
            candle(5, 11, 13, 11, 12),
        ];
        let out: Vec<Option<Candle>> = input.iter().map(|c| feed(&mut a, c)).collect();
        assert!(
            out[..4].iter().all(Option::is_none),
            "nothing before the first close"
        );
        let first = candle(0, 10, 15, 8, 11);
        assert_eq!(
            out[4],
            Some(Candle {
                volume: dec!(5),
                ..first.clone()
            })
        );
        // The forming 00:05 candle is never sent: the last completed one repeats.
        assert_eq!(out[5], out[4]);
    }

    #[test]
    fn the_closing_candle_counts_for_high_low_and_close() {
        // The last 1m candle of the period holds the high, the low and the close.
        let mut a = Aggregate::new(Timeframe::M1, Timeframe::M5).unwrap();
        let mut last = None;
        for m in 0..4 {
            last = feed(&mut a, &candle(m, 10, 11, 9, 10));
        }
        assert!(last.is_none());
        let done = feed(&mut a, &candle(4, 10, 20, 5, 7)).unwrap();
        assert_eq!(
            (done.open, done.high, done.low, done.close),
            (dec!(10), dec!(20), dec!(5), dec!(7))
        );
    }

    #[test]
    fn incomplete_periods_are_never_sent() {
        let mut a = Aggregate::new(Timeframe::M1, Timeframe::M5).unwrap();
        // Starts in the middle of a period: 00:00-00:05 is incomplete.
        assert!(feed(&mut a, &candle(3, 1, 1, 1, 1)).is_none());
        assert!(feed(&mut a, &candle(4, 2, 2, 2, 2)).is_none());
        // 00:05-00:10, with 00:07 missing: incomplete too.
        for m in [5, 6, 8, 9] {
            assert!(feed(&mut a, &candle(m, 3, 3, 3, 3)).is_none());
        }
        // 00:10-00:15 complete, aligned on the clock.
        let mut last = None;
        for m in 10..15 {
            last = feed(&mut a, &candle(m, 4, 4, 4, 4));
        }
        let done = last.unwrap();
        assert_eq!(done.open_time, candle(10, 0, 0, 0, 0).open_time);
        assert_eq!(done.volume, dec!(5));
    }

    #[test]
    fn rejects_incompatible_timeframes() {
        assert!(Aggregate::new(Timeframe::M5, Timeframe::M1).is_err());
        assert!(Aggregate::new(Timeframe::M15, Timeframe::M15).is_err());
        assert!(Aggregate::new(Timeframe::M15, Timeframe::H1).is_ok());
    }
}
