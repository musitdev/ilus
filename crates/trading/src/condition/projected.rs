//! `Projected`: runs a condition on a part of its input, only when that part
//! is new (spec F9), e.g. a 1h divergence on the output of
//! `Aggregate(15m → 1h)` followed by `OnValue(Rsi)`, which repeat the last
//! 1h candle on every 15m candle.

use std::marker::PhantomData;

use chrono::{DateTime, Utc};

use crate::candle::Candle;
use crate::condition::Condition;
use crate::error::ModifierError;
use crate::source::{Item, SourceData};
use crate::transformer::Derived;
use crate::trigger::Detection;

/// Runs `inner` on `project(item)` when the projected item is newer than the
/// last one processed (by its time).
pub struct Projected<C, I, F> {
    inner: C,
    project: F,
    last: Option<DateTime<Utc>>,
    input: PhantomData<fn(I)>,
}

impl<C, I, F> Projected<C, I, F>
where
    C: Condition,
    F: Fn(&I) -> Option<C::Input>,
{
    /// Run `inner` on the part of each item given by `project`.
    pub fn new(inner: C, project: F) -> Self {
        Self {
            inner,
            project,
            last: None,
            input: PhantomData,
        }
    }
}

/// Output of `Aggregate` + `OnValue(T)` (e.g. the 1h candle and its RSI).
pub type AggregatedWith<V> = Derived<Derived<Candle, Candle>, V>;

/// The aggregated candle and the value computed on it (e.g. an RSI candle of
/// the longer timeframe), if both are known.
pub fn aggregated<V: Clone>(item: &AggregatedWith<V>) -> Option<Derived<Candle, V>> {
    Some(Derived {
        data: item.data.value.clone()?,
        value: Some(item.value.clone()?),
    })
}

impl<C, I, F> Condition for Projected<C, I, F>
where
    C: Condition,
    I: SourceData,
    F: Fn(&I) -> Option<C::Input> + Send + 'static,
{
    type Input = I;
    type Value = C::Value;
    type Modifier = C::Modifier;

    fn warm_up(&self) -> usize {
        self.inner.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, I>) -> Option<Detection<C::Value>> {
        let part = (self.project)(item.data)?;
        let time = part.time();
        if self.last.is_some_and(|t| time <= t) {
            return None;
        }
        self.last = Some(time);
        self.inner.on_data(Item {
            data: &part,
            mode: item.mode,
        })
    }

    fn on_modifier(
        &mut self,
        modifier: C::Modifier,
    ) -> Result<Option<Detection<C::Value>>, ModifierError> {
        self.inner.on_modifier(modifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modifier::NoModifier;
    use crate::source::Mode;
    use crate::trigger::{TriggerEventType, TriggerValue};
    use chrono::TimeDelta;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[derive(Debug)]
    struct Seen(DateTime<Utc>, Decimal);
    impl TriggerValue for Seen {}

    /// Emits the time and value of every item it receives.
    struct Echo;
    impl Condition for Echo {
        type Input = Derived<Candle, Decimal>;
        type Value = Seen;
        type Modifier = NoModifier;
        fn warm_up(&self) -> usize {
            3
        }
        fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<Seen>> {
            Some(Detection {
                event_type: TriggerEventType::Other("seen"),
                value: Seen(item.data.data.open_time, item.data.value?),
            })
        }
        fn on_modifier(&mut self, m: NoModifier) -> Result<Option<Detection<Seen>>, ModifierError> {
            match m {}
        }
    }

    fn candle(min: i64) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(min),
            open: dec!(1),
            high: dec!(1),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
        }
    }

    #[test]
    fn runs_once_per_new_aggregated_candle() {
        let mut p = Projected::new(Echo, aggregated::<Decimal>);
        assert_eq!(p.warm_up(), 3);
        // 15m items carrying the last 1h candle (minute 0, then 60) and its RSI.
        let items: Vec<AggregatedWith<Decimal>> = [
            (0, None, None),
            (15, Some(0), Some(40)),
            (30, Some(0), Some(40)),
            (45, Some(0), Some(40)),
            (60, Some(60), Some(45)),
            (75, Some(60), Some(45)),
        ]
        .iter()
        .map(|&(m, h, rsi)| Derived {
            data: Derived {
                data: candle(m),
                value: h.map(candle),
            },
            value: rsi.map(Decimal::from),
        })
        .collect();
        let seen: Vec<(i64, Decimal)> = items
            .iter()
            .filter_map(|data| {
                p.on_data(Item {
                    data,
                    mode: Mode::Replay,
                })
            })
            .map(|d| {
                let Seen(t, v) = d.value;
                ((t - DateTime::<Utc>::UNIX_EPOCH).num_minutes(), v)
            })
            .collect();
        assert_eq!(seen, vec![(0, dec!(40)), (60, dec!(45))]);
    }
}
