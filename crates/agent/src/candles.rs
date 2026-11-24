//! Candles for the simulations (spec F10): fetched once from Binance, or
//! read from / written to a CSV file (`open_time_ms,open,high,low,close,volume`).

use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::Condition;
use trading::error::{ActionError, ModifierError};
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::NoModifier;
use trading::source::binance::BinanceSource;
use trading::source::{Item, Timeframe};
use trading::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};

/// Every candle (to collect them).
struct Every;

#[derive(Debug)]
struct Tick;
impl TriggerValue for Tick {}

impl Condition for Every {
    type Input = Candle;
    type Value = Tick;
    type Modifier = NoModifier;
    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, _: Item<'_, Candle>) -> Option<Detection<Tick>> {
        Some(Detection {
            event_type: TriggerEventType::Other("candle"),
            value: Tick,
        })
    }
    fn on_modifier(&mut self, m: NoModifier) -> Result<Option<Detection<Tick>>, ModifierError> {
        match m {}
    }
}

struct Collect(Arc<Mutex<Vec<Candle>>>);

impl Action for Collect {
    type Value = Tick;
    type Input = Candle;
    fn run(
        &mut self,
        event: TriggerEvent<'_, Tick, Candle>,
        _: &mut ActionContext,
    ) -> Result<(), ActionError> {
        if let Ok(mut v) = self.0.lock() {
            v.push(event.data.clone());
        }
        Ok(())
    }
}

/// Closed `timeframe` candles of `asset` from `from` to `to` (excluded),
/// from Binance (network; not for tests).
pub fn fetch_binance(
    asset: &str,
    timeframe: Timeframe,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Candle>, String> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut g = Graph::new();
    let src = g.source(BinanceSource::new(asset, timeframe).replay(from, to));
    let c = g.condition("every", Every, &src);
    g.action(Collect(out.clone()), &c);
    Executor::new(g)
        .with_tick(Duration::ZERO)
        .run()
        .map_err(|e| e.to_string())?;
    let v = out.lock().map_err(|_| "lock poisoned".to_string())?.clone();
    Ok(v)
}

/// Candles as CSV text (with a header line).
pub fn to_csv(candles: &[Candle]) -> String {
    let mut s = String::from("open_time_ms,open,high,low,close,volume\n");
    for c in candles {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{}",
            c.open_time.timestamp_millis(),
            c.open,
            c.high,
            c.low,
            c.close,
            c.volume
        );
    }
    s
}

/// Candles from CSV text (header line skipped).
pub fn from_csv(text: &str) -> Result<Vec<Candle>, String> {
    text.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let bad = |what: &str| format!("line {}: {what}", i + 2);
            if f.len() != 6 {
                return Err(bad("6 fields expected"));
            }
            let d = |k: usize| Decimal::from_str(f[k]).map_err(|e| bad(&e.to_string()));
            let ms: i64 = f[0].parse().map_err(|_| bad("bad time"))?;
            Ok(Candle {
                open_time: DateTime::from_timestamp_millis(ms).ok_or_else(|| bad("bad time"))?,
                open: d(1)?,
                high: d(2)?,
                low: d(3)?,
                close: d(4)?,
                volume: d(5)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn csv_round_trip_and_errors() {
        let c = Candle {
            open_time: DateTime::from_timestamp_millis(1_790_208_000_000).unwrap(),
            open: dec!(114.99),
            high: dec!(115.38),
            low: dec!(114.91),
            close: dec!(115.09),
            volume: dec!(21827.344),
        };
        let text = to_csv(std::slice::from_ref(&c));
        assert_eq!(from_csv(&text).unwrap(), vec![c]);
        assert!(from_csv("h\n1,2,3\n").unwrap_err().contains("line 2"));
        assert!(from_csv("h\n1,a,1,1,1,1\n").is_err());
    }
}
