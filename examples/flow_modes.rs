//! Running modes: bootstrap, then normal (live) mode with calls aligned on
//! candle closes. Uses a simulated clock, so it runs instantly.
//!
//! ```text
//! MockSource::live (1m candles) ──> Rsi(3) ──> RsiBelow(50) ──> PrintAt
//! ```
//!
//! What to check in the output:
//! - the first 4 candles (RSI(3) warm-up = 4) are *bootstrap*: they reach the
//!   condition but no action runs;
//! - then each candle is delivered in *normal* mode just after its close
//!   (close + 2 s), at the executor time printed by the action.
//!
//! Run with: `cargo run --example flow_modes`

mod common;

use chrono::{DateTime, Utc};
use common::idx;
use ilus::action::{Action, ActionContext};
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::executor::{Clock, Executor};
use ilus::graph::Graph;
use ilus::modifier::NoModifier;
use ilus::source::mock::MockSource;
use ilus::source::{Item, Timeframe};
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

/// Triggers when RSI < level; prints every candle it sees.
struct RsiBelow(Decimal);

#[derive(Debug)]
struct Low(Decimal);
impl TriggerValue for Low {}

impl Condition for RsiBelow {
    type Input = RsiCandle;
    type Value = Low;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Low>> {
        let rsi = item.data.value.map(|v| v.round_dp(1));
        println!(
            "condition | candle #{} close={} rsi={rsi:?} mode={:?}",
            idx(&item.data.data),
            item.data.data.close,
            item.mode
        );
        let rsi = rsi.filter(|r| *r < self.0)?;
        Some(Detection {
            event_type: TriggerEventType::Other("rsi_below"),
            value: Low(rsi),
        })
    }
    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<Self::Value>>, ModifierError> {
        match m {}
    }
}

/// Prints the executor time at which it runs.
struct PrintAt;

impl Action for PrintAt {
    type Value = Low;
    type Input = RsiCandle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Low, RsiCandle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        println!(
            "action    | rsi={} for the candle opened at {}, run at {} ({:?})",
            event.value.0,
            event.time.format("%H:%M:%S"),
            ctx.now().format("%H:%M:%S"),
            event.mode
        );
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let candles = common::candles(&[20, 19, 18, 17, 16, 15, 16, 14, 13]);
    let mut g = Graph::new();
    let src = g.source(MockSource::live("SOLUSDT", Timeframe::M1, candles));
    let rsi = g.transform(Rsi::new(3)?, &src);
    let below = g.condition("rsi-below-50", RsiBelow(Decimal::from(50)), &rsi);
    g.action(PrintAt, &below);

    // Simulated clock: starts at 00:00, waiting only advances it.
    let start = DateTime::<Utc>::UNIX_EPOCH;
    Executor::new(g).with_clock(Clock::Simulated(start)).run()?;
    Ok(())
}
