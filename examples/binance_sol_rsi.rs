//! Live example: Binance SOL/USDT 1m candles -> RSI(14) -> alert on extremes.
//!
//! ```text
//! BinanceSource(SOLUSDT, 1m) ──> Rsi(14) ──> RsiExtreme(30, 70) ──> Alert
//! ```
//!
//! Bootstraps the last 50 closed candles (printed, no alert), then follows the
//! live feed: one REST request just after each candle close. Every candle is
//! printed with its RSI; an alert fires when RSI < 30 or > 70 (live only).
//!
//! Run with: `cargo run --example binance_sol_rsi` (Ctrl+C to stop).

use ilus::action::Alert;
use ilus::condition::Condition;
use ilus::error::ModifierError;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::NoModifier;
use ilus::source::binance::BinanceSource;
use ilus::source::{Item, Timeframe};
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Triggers when RSI leaves the `[low, high]` band.
struct RsiExtreme {
    low: Decimal,
    high: Decimal,
}

#[derive(Debug)]
#[allow(dead_code)] // read through Debug by the alert
enum RsiSignal {
    Oversold(Decimal),
    Overbought(Decimal),
}
impl TriggerValue for RsiSignal {}

impl Condition for RsiExtreme {
    type Input = RsiCandle;
    type Value = RsiSignal;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<RsiSignal>> {
        let mode = item.mode;
        let item = item.data;
        let c = &item.data;
        let rsi = item.value.map(|v| v.round_dp(2));
        println!(
            "[SOL] {} close={} volume={} rsi={} ({mode:?})",
            c.open_time,
            c.close,
            c.volume,
            rsi.map_or("warming up".into(), |v| v.to_string())
        );
        let rsi = rsi?;
        let value = if rsi < self.low {
            RsiSignal::Oversold(rsi)
        } else if rsi > self.high {
            RsiSignal::Overbought(rsi)
        } else {
            return None;
        };
        Some(Detection {
            event_type: TriggerEventType::Other("rsi_extreme"),
            value,
        })
    }

    fn on_modifier(&mut self, modifier: NoModifier) -> Result<(), ModifierError> {
        match modifier {}
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    // Bootstrap: at least 50 candles (more than the RSI warm-up), then live.
    let sol = g.source(BinanceSource::new("SOLUSDT", Timeframe::M1).with_bootstrap(50));
    let sol_rsi = g.transform(Rsi::new(14)?, &sol);
    let extreme = g.condition(
        "sol-rsi-extreme",
        RsiExtreme {
            low: Decimal::from(30),
            high: Decimal::from(70),
        },
        &sol_rsi,
    );
    g.action(Alert::new(), &extreme);

    Executor::new(g).run().await;
    Ok(())
}
