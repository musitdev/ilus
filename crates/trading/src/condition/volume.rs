//! Volume spike condition (spec F13): the volume of a candle is more than
//! `factor` times the mean volume of the candles before it (input: the
//! output of [`VolumeMean`](crate::transformer::volume::VolumeMean)). The
//! signal follows the candle: bullish when it closes above its open,
//! bearish below, none for a doji.

use rust_decimal::Decimal;

use crate::candle::Candle;
use crate::condition::{Condition, Signal};
use crate::error::{ModifierError, ParameterError};
use crate::modifier::NoModifier;
use crate::source::Item;
use crate::transformer::Derived;
use crate::trigger::{Detection, TriggerEventType, TriggerValue};

/// Value of a volume spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpikeSignal {
    /// Direction of the candle.
    pub signal: Signal,
    /// Volume of the candle.
    pub volume: Decimal,
    /// Mean volume before it.
    pub mean: Decimal,
}

impl TriggerValue for SpikeSignal {}

/// Volume spike: volume > `factor` × mean volume.
#[derive(Debug, Clone)]
pub struct VolumeSpike {
    factor: Decimal,
}

impl VolumeSpike {
    /// Spikes of more than `factor` (> 1) times the mean.
    pub fn new(factor: Decimal) -> Result<Self, ParameterError> {
        if factor <= Decimal::ONE {
            return Err(ParameterError::Invalid(
                "volume spike: factor must be > 1".into(),
            ));
        }
        Ok(Self { factor })
    }
}

impl Condition for VolumeSpike {
    type Input = Derived<Candle, Decimal>;
    type Value = SpikeSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        1
    }

    fn on_data(&mut self, item: Item<'_, Self::Input>) -> Option<Detection<SpikeSignal>> {
        let c = &item.data.data;
        let mean = item.data.value?;
        if c.volume <= self.factor * mean {
            return None;
        }
        let signal = if c.close > c.open {
            Signal::Bullish
        } else if c.close < c.open {
            Signal::Bearish
        } else {
            return None;
        };
        Some(Detection {
            event_type: TriggerEventType::VolumeSpike,
            value: SpikeSignal {
                signal,
                volume: c.volume,
                mean,
            },
        })
    }

    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<SpikeSignal>>, ModifierError> {
        match m {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;
    use chrono::{DateTime, Utc};
    use rust_decimal_macros::dec;

    fn spike(
        open: Decimal,
        close: Decimal,
        volume: Decimal,
        mean: Option<Decimal>,
    ) -> Option<Signal> {
        let item = Derived {
            data: Candle {
                open_time: DateTime::<Utc>::UNIX_EPOCH,
                open,
                high: open.max(close),
                low: open.min(close),
                close,
                volume,
            },
            value: mean,
        };
        VolumeSpike::new(dec!(2))
            .unwrap()
            .on_data(Item {
                data: &item,
                mode: Mode::Replay,
            })
            .map(|d| d.value.signal)
    }

    #[test]
    fn spikes_follow_the_candle_direction() {
        let m = Some(dec!(100));
        assert_eq!(
            spike(dec!(10), dec!(11), dec!(250), m),
            Some(Signal::Bullish)
        );
        assert_eq!(
            spike(dec!(11), dec!(10), dec!(250), m),
            Some(Signal::Bearish)
        );
        assert_eq!(spike(dec!(10), dec!(10), dec!(250), m), None, "doji");
        assert_eq!(
            spike(dec!(10), dec!(11), dec!(200), m),
            None,
            "not more than 2x"
        );
        assert_eq!(
            spike(dec!(10), dec!(11), dec!(250), None),
            None,
            "no mean yet"
        );
        assert!(VolumeSpike::new(dec!(1)).is_err());
    }
}
