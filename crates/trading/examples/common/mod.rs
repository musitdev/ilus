//! Helpers shared by the `flow_*` examples and `simple`.
#![allow(dead_code)] // each example uses a subset

use std::fmt::Display;
use std::marker::PhantomData;
use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::error::ActionError;
use trading::source::mock::MockSource;
use trading::source::{SourceData, Timeframe};
use trading::trigger::{TriggerEvent, TriggerValue};

static START: OnceLock<Instant> = OnceLock::new();

/// Print a timeline line: elapsed real time, who, message.
pub fn log(who: &str, msg: impl Display) {
    let ms = START.get_or_init(Instant::now).elapsed().as_millis();
    println!("[{ms:>5}ms] {who:<12} | {msg}");
}

/// Candles with the given closes, one per minute from the epoch.
pub fn candles(closes: &[i64]) -> Vec<Candle> {
    closes
        .iter()
        .zip(0..)
        .map(|(&c, i)| {
            let close = Decimal::from(c);
            Candle {
                open_time: DateTime::<Utc>::UNIX_EPOCH + TimeDelta::minutes(i),
                open: close,
                high: close,
                low: close,
                close,
                volume: Decimal::ONE,
            }
        })
        .collect()
}

/// Mock source replaying candles with the given closes (1m).
pub fn replay(asset: &str, closes: &[i64]) -> MockSource<Candle> {
    START.get_or_init(Instant::now);
    MockSource::replay(asset, Timeframe::M1, candles(closes))
}

/// Candle index (minutes since the epoch), to identify candles in logs.
pub fn idx(c: &Candle) -> i64 {
    c.open_time.timestamp() / 60
}

/// Action logging the events it receives. Works with any condition.
pub struct LogAction<V, D> {
    name: &'static str,
    types: PhantomData<fn(V, D)>,
}

impl<V, D> LogAction<V, D> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            types: PhantomData,
        }
    }
}

impl<V: TriggerValue, D: SourceData> Action for LogAction<V, D> {
    type Value = V;
    type Input = D;

    fn run(
        &mut self,
        event: TriggerEvent<'_, V, D>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        log(
            self.name,
            format!(
                "{} {:?} value={:?} ({:?})",
                event.condition, event.event_type, event.value, event.mode
            ),
        );
        Ok(())
    }
}
