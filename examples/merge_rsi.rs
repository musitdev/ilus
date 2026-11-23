//! Two timeframes in one condition: SOL/USDT 15m + 1h, with an RSI on each.
//!
//! ```text
//! BinanceSource 15m ─┐
//!                    ├─ MergeSource ──> Combine(OnMain(Rsi 14), OnOther(Rsi 14)) ──> BothLow ──> Print
//! BinanceSource 1h ──┘   (15m, last closed 1h)     (RSI 15m, RSI 1h)
//! ```
//!
//! - The merge emits each 15m candle with the last 1h candle **already
//!   closed** (no look-ahead); the same 1h candle repeats until a new one.
//! - `OnOther(Rsi)` only advances the 1h RSI when a new 1h candle arrives.
//! - The condition triggers when RSI 15m < 30 and RSI 1h < 40.
//!
//! Replays the last `days` days (default 3) after a bootstrap of the RSIs.
//!
//! Run with: `cargo run --example merge_rsi -- [days]`

use std::time::Duration;

use chrono::Utc;
use ilus::action::{Action, ActionContext};
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::NoModifier;
use ilus::source::binance::BinanceSource;
use ilus::source::merge::{MergeSource, Merged};
use ilus::source::{Item, Timeframe};
use ilus::transformer::Derived;
use ilus::transformer::combine::{Combine, OnMain, OnOther};
use ilus::transformer::rsi::Rsi;
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Output of the combined transformer: merged candles + (RSI 15m, RSI 1h).
type Rsis = Derived<Merged<Candle, Candle>, (Option<Decimal>, Option<Decimal>)>;

/// Triggers when both RSIs are low.
struct BothLow {
    low_15m: Decimal,
    low_1h: Decimal,
}

#[derive(Debug)]
struct Low {
    rsi_15m: Decimal,
    rsi_1h: Decimal,
}
impl TriggerValue for Low {}

impl Condition for BothLow {
    type Input = Rsis;
    type Value = Low;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, Rsis>) -> Option<Detection<Low>> {
        let (Some(rsi_15m), Some(rsi_1h)) = item.data.value? else {
            return None;
        };
        (rsi_15m < self.low_15m && rsi_1h < self.low_1h).then_some(Detection {
            event_type: TriggerEventType::Other("both_low"),
            value: Low { rsi_15m, rsi_1h },
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

/// Prints each signal with the 15m and 1h candles it was computed from.
struct Print;

impl Action for Print {
    type Value = Low;
    type Input = Rsis;

    async fn run(
        &mut self,
        event: TriggerEvent<'_, Low, Rsis>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let m = &event.data.data;
        let one_hour = m.other.as_ref().map_or("-".into(), |h| {
            h.open_time.format("%m-%d %H:%M").to_string()
        });
        println!(
            "{}  close={:<8} RSI 15m={:<6} RSI 1h={:<6} (1h candle {one_hour})",
            m.main.open_time.format("%m-%d %H:%M"),
            m.main.close.normalize(),
            event.value.rsi_15m.round_dp(2),
            event.value.rsi_1h.round_dp(2),
        );
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let days: u64 = match std::env::args().nth(1) {
        Some(d) => d.parse().map_err(|e| format!("invalid days '{d}': {e}"))?,
        None => 3,
    };
    let to = Utc::now();
    let from = to - chrono::Days::new(days);
    println!(
        "SOL/USDT 15m + 1h, RSI 15m < 30 and RSI 1h < 40, {} -> {} (UTC)\n",
        from.format("%Y-%m-%d %H:%M"),
        to.format("%Y-%m-%d %H:%M")
    );

    let sol_15m = BinanceSource::new("SOLUSDT", Timeframe::M15).replay(from, to);
    let sol_1h = BinanceSource::new("SOLUSDT", Timeframe::H1).replay(from, to);
    // RSI(14) on 1h needs 15 1h candles of warm-up (counted in 1h items).
    let merged = MergeSource::new(sol_15m, sol_1h).with_other_bootstrap(15);

    let mut g = Graph::new();
    let sol = g.source(merged);
    let rsis = g.transform(
        Combine::new((OnMain::new(Rsi::new(14)?), OnOther::new(Rsi::new(14)?))),
        &sol,
    );
    let both_low = g.condition(
        "both-low",
        BothLow {
            low_15m: Decimal::from(30),
            low_1h: Decimal::from(40),
        },
        &rsis,
    );
    g.action(Print, &both_low);

    Executor::new(g).with_tick(Duration::ZERO).run().await;
    Ok(())
}
