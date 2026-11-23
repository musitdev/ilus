//! Independent rules running in parallel.
//!
//! ```text
//! MockSource "FAST" (every 50 ms, 8 candles)  ──> Above(fast) ──> LogAction
//! MockSource "SLOW" (every 200 ms, 3 candles) ──> Above(slow) ──> LogAction
//! ```
//!
//! What to check in the output:
//! - both streams interleave: neither rule waits for the other;
//! - each rule only sees its own source;
//! - the slow source ends first in candles but later in time; the bot stops
//!   only when every source has ended and every action has finished.
//!
//! Run with: `cargo run --example flow_parallel`

mod common;

use common::{LogAction, idx, log, mock};
use ilus::bot::Bot;
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::ModifierError;
use ilus::modifier::NoModifier;
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Triggers when the close is above `level`.
struct Above {
    name: &'static str,
    level: Decimal,
}

#[derive(Debug)]
#[allow(dead_code)] // read through Debug
struct Close(Decimal);
impl TriggerValue for Close {}

impl Condition for Above {
    type Input = Candle;
    type Value = Close;
    type Modifier = NoModifier;

    fn on_data(&mut self, c: &Candle) -> Option<Detection<Close>> {
        log(self.name, format!("candle #{} close={}", idx(c), c.close));
        (c.close > self.level).then_some(Detection {
            event_type: TriggerEventType::Other("above"),
            value: Close(c.close),
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bot = Bot::new();

    let mut fast = mock("FAST", &[1, 2, 3, 4, 5, 6, 7, 8], 50);
    let mut slow = mock("SLOW", &[100, 200, 300], 200);

    bot.rule(Rule::new(
        "fast-above-5",
        fast.get_source_stream()?,
        Above {
            name: "cond FAST",
            level: Decimal::from(5),
        },
        LogAction,
    ));
    bot.rule(Rule::new(
        "slow-above-150",
        slow.get_source_stream()?,
        Above {
            name: "cond SLOW",
            level: Decimal::from(150),
        },
        LogAction,
    ));
    bot.source(fast);
    bot.source(slow);

    bot.run().await;
    log("main", "bot.run() returned: all sources ended");
    Ok(())
}
