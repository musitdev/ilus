//! Trend filter (spec F8, rule 3): wraps a condition and only keeps its
//! signals in the direction of the trend.
//!
//! ```text
//! source ──> ... ──> Trend(EMA n) ──> TrendFilter(condition) ──> action
//! ```
//!
//! - The trend (close above / below an EMA) is computed by the
//!   [`Trend`](crate::transformer::trend::Trend) transformer: it is data.
//! - The filter is a condition: removing a signal is a decision on the
//!   detections, which transformers do not see.
//! - In an uptrend only bullish (buy) signals pass, in a downtrend only
//!   bearish (sell) ones; when the trend is flat or unknown, nothing passes.
//! - The wrapped condition receives every item (without the trend), so its
//!   state is unchanged by the filter.

use crate::condition::divergence::Divergence;
use crate::condition::ema_rules::{CrossSignal, PullbackSignal, RibbonSignal};
use crate::condition::patterns::PatternMatches;
use crate::condition::{Condition, Signal};
use crate::error::ModifierError;
use crate::source::{Item, SourceData};
use crate::transformer::Derived;
use crate::transformer::trend::TrendValue;
use crate::trigger::{Detection, TriggerEventType, TriggerValue};

/// A signal value that can be filtered by direction.
pub trait SignalFilter: TriggerValue + Sized {
    /// Keep only what goes in the `allowed` direction (`None`: no direction
    /// allowed, flat or unknown trend); `None` if nothing is left. Values with
    /// a neutral part (e.g. exit data) may keep it.
    fn retain(self, allowed: Option<Signal>) -> Option<Self>;

    /// Event type after filtering, if it depends on what is left (e.g. the
    /// first pattern kept). `None`: unchanged.
    fn event_type(&self) -> Option<TriggerEventType> {
        None
    }
}

impl SignalFilter for Divergence {
    fn retain(self, allowed: Option<Signal>) -> Option<Self> {
        let signal = match self.kind {
            crate::condition::divergence::DivergenceKind::Bullish => Signal::Bullish,
            crate::condition::divergence::DivergenceKind::Bearish => Signal::Bearish,
        };
        (Some(signal) == allowed).then_some(self)
    }
}

impl SignalFilter for PatternMatches {
    fn retain(self, allowed: Option<Signal>) -> Option<Self> {
        let kept: Vec<_> = self
            .0
            .into_iter()
            .filter(|m| Some(m.signal) == allowed)
            .collect();
        (!kept.is_empty()).then_some(PatternMatches(kept))
    }

    fn event_type(&self) -> Option<TriggerEventType> {
        self.0.first().map(|m| m.pattern.event_type())
    }
}

macro_rules! by_signal_field {
    ($($t:ty),*) => {$(
        impl SignalFilter for $t {
            fn retain(self, allowed: Option<Signal>) -> Option<Self> {
                (Some(self.signal) == allowed).then_some(self)
            }
        }
    )*};
}

by_signal_field!(CrossSignal, PullbackSignal, RibbonSignal);

/// Wraps a condition: its signals pass only in the direction of the trend.
///
/// Input: the output of a [`Trend`](crate::transformer::trend::Trend)
/// transformer placed on the wrapped condition's input.
pub struct TrendFilter<C> {
    inner: C,
    enabled: bool,
}

impl<C> TrendFilter<C> {
    /// Filter the signals of `inner` by the trend.
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            enabled: true,
        }
    }

    /// The same graph without filtering (every signal passes), e.g. to
    /// compare, or as an on/off gene.
    pub fn disabled(inner: C) -> Self {
        Self {
            inner,
            enabled: false,
        }
    }

    fn filter(
        &self,
        detection: Detection<<C as Condition>::Value>,
        trend: Option<&TrendValue>,
    ) -> Option<Detection<<C as Condition>::Value>>
    where
        C: Condition,
        C::Value: SignalFilter,
    {
        if !self.enabled {
            return Some(detection);
        }
        let allowed = trend.and_then(|t| t.side);
        let value = detection.value.retain(allowed)?;
        Some(Detection {
            event_type: value.event_type().unwrap_or(detection.event_type),
            value,
        })
    }
}

impl<C> Condition for TrendFilter<C>
where
    C: Condition,
    C::Value: SignalFilter,
    Derived<C::Input, TrendValue>: SourceData,
{
    type Input = Derived<C::Input, TrendValue>;
    type Value = C::Value;
    type Modifier = C::Modifier;

    fn warm_up(&self) -> usize {
        self.inner.warm_up()
    }

    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<C::Value>> {
        let detection = self.inner.on_data(Item {
            data: &item.data.data,
            mode: item.mode,
        })?;
        self.filter(detection, item.data.value.as_ref())
    }

    fn on_modifier(
        &mut self,
        modifier: C::Modifier,
    ) -> Result<Option<Detection<C::Value>>, ModifierError> {
        // No trend item here: a detection triggered by a modifier passes
        // only when the filter is disabled.
        let detection = self.inner.on_modifier(modifier)?;
        Ok(detection.filter(|_| !self.enabled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use crate::condition::patterns::{Pattern, PatternMatch};
    use crate::modifier::NoModifier;
    use crate::source::Mode;
    use chrono::{DateTime, Utc};
    use rust_decimal_macros::dec;

    /// Emits the scripted value for each item, and counts the items seen.
    struct Scripted<V> {
        script: Vec<Option<V>>,
        seen: usize,
    }

    impl<V: SignalFilter + Clone> Condition for Scripted<V> {
        type Input = Candle;
        type Value = V;
        type Modifier = NoModifier;

        fn warm_up(&self) -> usize {
            7
        }

        fn on_data(&mut self, _item: Item<'_, Candle>) -> Option<Detection<V>> {
            let value = self.script.get(self.seen).cloned().flatten();
            self.seen += 1;
            value.map(|value| Detection {
                event_type: TriggerEventType::EmaCross,
                value,
            })
        }

        fn on_modifier(&mut self, m: NoModifier) -> Result<Option<Detection<V>>, ModifierError> {
            match m {}
        }
    }

    fn cross(signal: Signal) -> CrossSignal {
        CrossSignal {
            signal,
            value: dec!(1),
            reference: dec!(1),
        }
    }

    fn item(side: Option<Signal>, known: bool) -> Derived<Candle, TrendValue> {
        let c = dec!(10);
        Derived {
            data: Candle {
                open_time: DateTime::<Utc>::UNIX_EPOCH,
                open: c,
                high: c,
                low: c,
                close: c,
                volume: dec!(1),
            },
            value: known.then_some(TrendValue { ema: c, side }),
        }
    }

    fn run<V: SignalFilter + Clone>(
        filter: &mut TrendFilter<Scripted<V>>,
        trends: &[(Option<Signal>, bool)],
    ) -> Vec<Option<Detection<V>>> {
        trends
            .iter()
            .map(|&(side, known)| {
                filter.on_data(Item {
                    data: &item(side, known),
                    mode: Mode::Replay,
                })
            })
            .collect()
    }

    use Signal::{Bearish as Down, Bullish as Up};

    #[test]
    fn only_signals_in_the_trend_direction_pass() {
        let script = vec![
            Some(cross(Up)),   // uptrend: buy passes
            Some(cross(Down)), // uptrend: sell blocked
            Some(cross(Down)), // downtrend: sell passes
            Some(cross(Up)),   // downtrend: buy blocked
            Some(cross(Up)),   // flat trend: blocked
            Some(cross(Up)),   // trend not warmed up: blocked
            None,              // no signal
        ];
        let mut f = TrendFilter::new(Scripted { script, seen: 0 });
        assert_eq!(f.warm_up(), 7, "warm-up of the wrapped condition");
        let out = run(
            &mut f,
            &[
                (Some(Up), true),
                (Some(Up), true),
                (Some(Down), true),
                (Some(Down), true),
                (None, true),
                (None, false),
                (Some(Up), true),
            ],
        );
        let signals: Vec<Option<Signal>> = out
            .iter()
            .map(|d| d.as_ref().map(|d| d.value.signal))
            .collect();
        assert_eq!(
            signals,
            vec![Some(Up), None, Some(Down), None, None, None, None]
        );
        assert_eq!(f.inner.seen, 7, "the wrapped condition sees every item");
    }

    #[test]
    fn disabled_filter_lets_everything_pass() {
        let script = vec![Some(cross(Down)), Some(cross(Up))];
        let mut f = TrendFilter::disabled(Scripted { script, seen: 0 });
        let out = run(&mut f, &[(Some(Up), true), (None, false)]);
        assert!(out.iter().all(Option::is_some));
    }

    #[test]
    fn patterns_are_filtered_one_by_one() {
        let m = |pattern, signal| PatternMatch { pattern, signal };
        let both = PatternMatches(vec![m(Pattern::ShootingStar, Down), m(Pattern::Hammer, Up)]);
        let script = vec![Some(both.clone()), Some(both)];
        let mut f = TrendFilter::new(Scripted { script, seen: 0 });
        let out = run(&mut f, &[(Some(Up), true), (Some(Down), true)]);
        let up = out[0].as_ref().unwrap();
        assert_eq!(up.value.0, vec![m(Pattern::Hammer, Up)]);
        assert_eq!(up.event_type, TriggerEventType::Hammer);
        let down = out[1].as_ref().unwrap();
        assert_eq!(down.value.0, vec![m(Pattern::ShootingStar, Down)]);
        assert_eq!(down.event_type, TriggerEventType::ShootingStar);
    }

    #[test]
    fn divergences_are_filtered_by_kind() {
        use crate::condition::divergence::{Divergence, DivergenceKind, Pivot};
        let p = Pivot {
            time: DateTime::<Utc>::UNIX_EPOCH,
            price: dec!(1),
            rsi: dec!(1),
        };
        let div = |kind| Divergence {
            kind,
            first: p,
            second: p,
            confirmed_at: p.time,
        };
        assert!(div(DivergenceKind::Bullish).retain(Some(Up)).is_some());
        assert!(div(DivergenceKind::Bullish).retain(Some(Down)).is_none());
        assert!(div(DivergenceKind::Bullish).retain(None).is_none());
        assert!(div(DivergenceKind::Bearish).retain(Some(Down)).is_some());
    }
}
