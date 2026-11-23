//! Helpers shared by the `flow_*` examples.
#![allow(dead_code)] // each example uses a subset

use std::fmt::Display;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use ilus::action::{Action, EventOf};
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::ActionError;
use ilus::modifier::ModifierSender;
use ilus::source::Timeframe;
use ilus::source::mock::MockSource;
use rust_decimal::Decimal;

static START: OnceLock<Instant> = OnceLock::new();

/// Print a timeline line: elapsed time, who, message.
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

/// Mock candle source sending one candle every `every_ms` milliseconds.
pub fn mock(asset: &str, closes: &[i64], every_ms: u64) -> MockSource<Candle> {
    START.get_or_init(Instant::now);
    MockSource::new(asset, Timeframe::M1, candles(closes))
        .with_interval(Duration::from_millis(every_ms))
}

/// Candle index (minutes since the epoch), to identify candles in logs.
pub fn idx(c: &Candle) -> i64 {
    c.open_time.timestamp() / 60
}

/// Action logging the event it receives. Works with any condition.
pub struct LogAction;

impl<C: Condition> Action<C> for LogAction {
    async fn run(
        &mut self,
        event: EventOf<C>,
        _modifiers: &ModifierSender<C::Modifier>,
    ) -> Result<(), ActionError> {
        log(
            "action",
            format!(
                "rule={} {:?} value={:?} source={}",
                event.rule, event.event_type, event.value, event.source
            ),
        );
        Ok(())
    }
}
