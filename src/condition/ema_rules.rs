//! Buy / sell rules based on EMAs (and MACD, built on EMAs).
//!
//! | Rule | Condition | Input | Buy | Sell |
//! |---|---|---|---|---|
//! | 1 | [`PriceCrossEma`] | `Ema` output | close crosses above the EMA | close crosses below |
//! | 2 | [`EmaCross`] | `Combine((Ema fast, Ema slow))` | fast crosses above slow | fast crosses below |
//! | 4 | [`EmaPullback`] | `Combine((Ema short, Ema trend))` | uptrend, price above the short EMA dips to it and closes back above | mirror in a downtrend |
//! | 5 | [`EmaRibbon`] | `Combine((Ema a, Ema b, Ema c))` | EMAs become aligned upward (a > b > c) | aligned downward |
//! | 6 | [`MacdCross`] | `Macd` output | MACD crosses above its signal line | crosses below |
//!
//! A cross triggers **once**, on the candle where the side changes (touching,
//! `a = b`, does not change the side). The first item only sets the side.

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::condition::{Condition, Signal};
use crate::error::ModifierError;
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::Derived;
use crate::transformer::ema::EmaCandle;
use crate::transformer::macd::MacdCandle;
use crate::trigger::{Detection, TriggerEventType, TriggerValue};

/// Candles with two EMAs (output of `Combine((Ema, Ema))`).
pub type EmaPair = Derived<Candle, (Option<Decimal>, Option<Decimal>)>;

/// Candles with three EMAs (output of `Combine((Ema, Ema, Ema))`).
pub type EmaTriple = Derived<Candle, (Option<Decimal>, Option<Decimal>, Option<Decimal>)>;

// ------------------------------------------------------------------ crossing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Above,
    Below,
}

/// Detects when `a − b` changes sign.
#[derive(Debug, Clone, Default)]
struct Crossing {
    side: Option<Side>,
}

impl Crossing {
    /// `Bullish` when `a` crosses above `b`, `Bearish` when below.
    fn update(&mut self, a: Decimal, b: Decimal) -> Option<Signal> {
        let now = if a > b {
            Side::Above
        } else if a < b {
            Side::Below
        } else {
            return None; // touching: the side does not change
        };
        match (self.side.replace(now), now) {
            (Some(Side::Below), Side::Above) => Some(Signal::Bullish),
            (Some(Side::Above), Side::Below) => Some(Signal::Bearish),
            _ => None,
        }
    }
}

/// Value of a crossing rule: `value` crossed `reference`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossSignal {
    /// Buy (bullish) or sell (bearish).
    pub signal: Signal,
    /// The crossing value (close, fast EMA, MACD).
    pub value: Decimal,
    /// What it crossed (EMA, slow EMA, signal line).
    pub reference: Decimal,
}

impl TriggerValue for CrossSignal {}

fn cross_detection(
    event_type: TriggerEventType,
    crossing: &mut Crossing,
    value: Decimal,
    reference: Decimal,
) -> Option<Detection<CrossSignal>> {
    let signal = crossing.update(value, reference)?;
    Some(Detection {
        event_type,
        value: CrossSignal {
            signal,
            value,
            reference,
        },
    })
}

macro_rules! no_modifier {
    ($value:ty) => {
        fn on_modifier(
            &mut self,
            m: NoModifier,
        ) -> Result<Option<Detection<$value>>, ModifierError> {
            match m {}
        }
    };
}

// ------------------------------------------------------------------- rule 1

/// Rule 1: the close crosses its EMA.
#[derive(Debug, Clone, Default)]
pub struct PriceCrossEma {
    crossing: Crossing,
}

impl Condition for PriceCrossEma {
    type Input = EmaCandle;
    type Value = CrossSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1 // one item to know the side
    }

    fn on_data(&mut self, item: Item<'_, EmaCandle>) -> Option<Detection<CrossSignal>> {
        let ema = item.data.value?;
        cross_detection(
            TriggerEventType::EmaPriceCross,
            &mut self.crossing,
            item.data.data.close,
            ema,
        )
    }

    no_modifier!(CrossSignal);
}

// ------------------------------------------------------------------- rule 2

/// Rule 2: the fast EMA crosses the slow EMA (input: `(fast, slow)`), e.g.
/// 9/21, 12/26, or 50/200 (golden cross / death cross).
#[derive(Debug, Clone, Default)]
pub struct EmaCross {
    crossing: Crossing,
}

impl Condition for EmaCross {
    type Input = EmaPair;
    type Value = CrossSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, EmaPair>) -> Option<Detection<CrossSignal>> {
        let (Some(fast), Some(slow)) = item.data.value? else {
            return None;
        };
        cross_detection(TriggerEventType::EmaCross, &mut self.crossing, fast, slow)
    }

    no_modifier!(CrossSignal);
}

// ------------------------------------------------------------------- rule 4

/// Value of [`EmaPullback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullbackSignal {
    /// Buy (pullback in an uptrend) or sell (in a downtrend).
    pub signal: Signal,
    /// Close of the candle.
    pub close: Decimal,
    /// Short EMA (touched).
    pub short: Decimal,
    /// Trend EMA.
    pub trend: Decimal,
}

impl TriggerValue for PullbackSignal {}

/// Rule 4: pullback to the short EMA in a trend (input: `(short, trend)`,
/// e.g. EMA 20 / EMA 50).
/// - Buy: short EMA and close above the trend EMA; the previous close was
///   above the short EMA (the price comes from above); the low touches the
///   short EMA and the close is back above it.
/// - Sell: the mirror (short EMA and close below the trend EMA; previous close
///   below the short EMA; the high touches it and the close is back below).
///
/// Requiring the previous close on the trend side excludes breakouts (a candle
/// opening below the EMA and closing far above it is not a pullback).
#[derive(Debug, Clone, Default)]
pub struct EmaPullback {
    /// Side of the previous close vs the short EMA.
    previous: Option<Side>,
}

impl Condition for EmaPullback {
    type Input = EmaPair;
    type Value = PullbackSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1 // the previous close
    }

    fn on_data(&mut self, item: Item<'_, EmaPair>) -> Option<Detection<PullbackSignal>> {
        let (Some(short), Some(trend)) = item.data.value? else {
            return None;
        };
        let c = &item.data.data;
        let now = if c.close > short {
            Some(Side::Above)
        } else if c.close < short {
            Some(Side::Below)
        } else {
            self.previous
        };
        let previous = std::mem::replace(&mut self.previous, now)?;
        let signal = if previous == Side::Above
            && short > trend
            && c.close > trend
            && c.low <= short
            && c.close > short
        {
            Signal::Bullish
        } else if previous == Side::Below
            && short < trend
            && c.close < trend
            && c.high >= short
            && c.close < short
        {
            Signal::Bearish
        } else {
            return None;
        };
        Some(Detection {
            event_type: TriggerEventType::EmaPullback,
            value: PullbackSignal {
                signal,
                close: c.close,
                short,
                trend,
            },
        })
    }

    no_modifier!(PullbackSignal);
}

// ------------------------------------------------------------------- rule 5

/// Value of [`EmaRibbon`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RibbonSignal {
    /// Buy (aligned upward) or sell (aligned downward).
    pub signal: Signal,
    /// The three EMAs, fastest first.
    pub emas: [Decimal; 3],
}

impl TriggerValue for RibbonSignal {}

/// Rule 5: three EMAs become aligned (input: `(fast, middle, slow)`, e.g.
/// 8 / 13 / 21). Buy when they become `fast > middle > slow`, sell when they
/// become `fast < middle < slow`. Triggers once, when the alignment starts.
#[derive(Debug, Clone, Default)]
pub struct EmaRibbon {
    /// Alignment at the previous item (`None`: not aligned); unknown before
    /// the first item.
    previous: Option<Option<Signal>>,
}

impl Condition for EmaRibbon {
    type Input = EmaTriple;
    type Value = RibbonSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, EmaTriple>) -> Option<Detection<RibbonSignal>> {
        let (Some(a), Some(b), Some(c)) = item.data.value? else {
            return None;
        };
        let now = if a > b && b > c {
            Some(Signal::Bullish)
        } else if a < b && b < c {
            Some(Signal::Bearish)
        } else {
            None
        };
        let previous = self.previous.replace(now)?;
        let signal = now.filter(|s| previous != Some(*s))?;
        Some(Detection {
            event_type: TriggerEventType::EmaRibbon,
            value: RibbonSignal {
                signal,
                emas: [a, b, c],
            },
        })
    }

    no_modifier!(RibbonSignal);
}

// ------------------------------------------------------------------- rule 6

/// Rule 6: the MACD line crosses its signal line (input: `Macd` output).
#[derive(Debug, Clone, Default)]
pub struct MacdCross {
    crossing: Crossing,
}

impl Condition for MacdCross {
    type Input = MacdCandle;
    type Value = CrossSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, MacdCandle>) -> Option<Detection<CrossSignal>> {
        let macd = item.data.value?;
        let signal = macd.signal?;
        cross_detection(
            TriggerEventType::MacdCross,
            &mut self.crossing,
            macd.macd,
            signal,
        )
    }

    no_modifier!(CrossSignal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use crate::transformer::macd::MacdValue;
    use chrono::{DateTime, TimeDelta, Utc};
    use rust_decimal_macros::dec;

    fn candle(i: i64, low: Decimal, high: Decimal, close: Decimal) -> Candle {
        Candle {
            open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
            open: close,
            high,
            low,
            close,
            volume: dec!(1),
        }
    }

    fn flat(i: i64, close: Decimal) -> Candle {
        candle(i, close, close, close)
    }

    /// Feed the items; return the signal of each one.
    fn signals<C: Condition>(cond: &mut C, items: &[C::Input]) -> Vec<Option<Signal>>
    where
        C::Value: HasSignal,
    {
        items
            .iter()
            .map(|data| {
                cond.on_data(Item {
                    data,
                    mode: Mode::Replay,
                })
                .map(|d| d.value.signal())
            })
            .collect()
    }

    trait HasSignal {
        fn signal(&self) -> Signal;
    }
    impl HasSignal for CrossSignal {
        fn signal(&self) -> Signal {
            self.signal
        }
    }
    impl HasSignal for PullbackSignal {
        fn signal(&self) -> Signal {
            self.signal
        }
    }
    impl HasSignal for RibbonSignal {
        fn signal(&self) -> Signal {
            self.signal
        }
    }

    const B: Option<Signal> = Some(Signal::Bullish);
    const S: Option<Signal> = Some(Signal::Bearish);

    #[test]
    fn rule1_close_crossing_the_ema_triggers_once_per_cross() {
        // EMA fixed at 10; closes 9, 9.5, 10 (touch), 11, 12, 9.
        let closes = [dec!(9), dec!(9.5), dec!(10), dec!(11), dec!(12), dec!(9)];
        let items: Vec<EmaCandle> = closes
            .iter()
            .zip(0..)
            .map(|(&c, i)| Derived {
                data: flat(i, c),
                value: Some(dec!(10)),
            })
            .collect();
        let got = signals(&mut PriceCrossEma::default(), &items);
        assert_eq!(got, vec![None, None, None, B, None, S]);
        // No EMA yet: nothing.
        let warming = [Derived {
            data: flat(0, dec!(1)),
            value: None,
        }];
        assert_eq!(signals(&mut PriceCrossEma::default(), &warming), vec![None]);
    }

    fn pair(i: i64, a: Option<Decimal>, b: Option<Decimal>) -> EmaPair {
        Derived {
            data: flat(i, dec!(100)),
            value: Some((a, b)),
        }
    }

    #[test]
    fn rule2_fast_ema_crossing_the_slow_one() {
        let items = [
            pair(0, Some(dec!(9)), Some(dec!(10))),
            pair(1, Some(dec!(11)), Some(dec!(10))), // golden cross
            pair(2, Some(dec!(12)), Some(dec!(10))),
            pair(3, Some(dec!(9)), Some(dec!(10))), // death cross
            pair(4, None, Some(dec!(10))),          // fast warming up: ignored
        ];
        assert_eq!(
            signals(&mut EmaCross::default(), &items),
            vec![None, B, None, S, None]
        );
    }

    #[test]
    fn rule4_pullback_to_the_short_ema_in_a_trend() {
        let with = |low, high, close, short, trend| Derived {
            data: candle(0, low, high, close),
            value: Some((Some(short), Some(trend))),
        };
        // Uptrend: short EMA 105 above trend EMA 100.
        let up = [
            with(dec!(106), dec!(108), dec!(107), dec!(105), dec!(100)), // above: sets the side
            with(dec!(104), dec!(107), dec!(106), dec!(105), dec!(100)), // touch, back above: buy
            with(dec!(105.5), dec!(107), dec!(106), dec!(105), dec!(100)), // no touch
            with(dec!(104), dec!(106), dec!(104.5), dec!(105), dec!(100)), // closes below
        ];
        assert_eq!(
            signals(&mut EmaPullback::default(), &up),
            vec![None, B, None, None]
        );
        // Breakout: the previous close was below the short EMA: not a pullback.
        let breakout = [
            with(dec!(103), dec!(104.5), dec!(104), dec!(105), dec!(100)),
            with(dec!(104), dec!(112), dec!(111), dec!(105), dec!(100)),
        ];
        assert_eq!(
            signals(&mut EmaPullback::default(), &breakout),
            vec![None, None]
        );
        // Downtrend (short 95 < trend 100): previous close below, high touches, back below.
        let down = [
            with(dec!(92), dec!(94), dec!(93), dec!(95), dec!(100)),
            with(dec!(93), dec!(96), dec!(94), dec!(95), dec!(100)),
        ];
        assert_eq!(signals(&mut EmaPullback::default(), &down), vec![None, S]);
    }

    #[test]
    fn rule5_ribbon_triggers_when_the_alignment_starts() {
        let triple = |i, a, b, c| Derived {
            data: flat(i, dec!(100)),
            value: Some((Some(a), Some(b), Some(c))),
        };
        let items = [
            triple(0, dec!(10), dec!(11), dec!(9)),  // not aligned
            triple(1, dec!(12), dec!(11), dec!(10)), // aligned up: buy
            triple(2, dec!(13), dec!(12), dec!(11)), // still aligned: nothing
            triple(3, dec!(11), dec!(12), dec!(11)), // broken
            triple(4, dec!(9), dec!(10), dec!(11)),  // aligned down: sell
        ];
        assert_eq!(
            signals(&mut EmaRibbon::default(), &items),
            vec![None, B, None, None, S]
        );
        // Already aligned on the first item: not a new alignment.
        let first = [triple(0, dec!(3), dec!(2), dec!(1))];
        assert_eq!(signals(&mut EmaRibbon::default(), &first), vec![None]);
    }

    #[test]
    fn rule6_macd_crossing_its_signal_line() {
        let macd = |i, m, s| Derived {
            data: flat(i, dec!(100)),
            value: Some(MacdValue { macd: m, signal: s }),
        };
        let items = [
            macd(0, dec!(0.5), None), // signal line warming up
            macd(1, dec!(-0.2), Some(dec!(0.1))),
            macd(2, dec!(0.3), Some(dec!(0.1))), // crosses above
            macd(3, dec!(0.0), Some(dec!(0.1))), // crosses below
        ];
        assert_eq!(
            signals(&mut MacdCross::default(), &items),
            vec![None, None, B, S]
        );
    }
}
