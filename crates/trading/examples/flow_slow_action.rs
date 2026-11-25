//! A slow action makes the tick longer: nothing is queued, nothing is lost.
//!
//! ```text
//! MockSource ──> EveryCandle ──> Slow (300 ms, fails on its 2nd run)
//! ```
//!
//! Every candle matches. A tick is sequential (source, condition, actions,
//! modifiers) and runs on the executor thread: a slow (blocking) action only
//! delays the tick; async work would go to the runtime threads. What to check:
//! - "action start" / "action end" always come before the next candle;
//! - the time between two candles is the tick (10 ms) plus the action (300 ms);
//! - a failing action is logged and the next tick continues normally.
//!
//! Run with: `cargo run --example flow_slow_action`

mod common;

use std::time::Duration;

use common::{idx, log};
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::Condition;
use trading::error::{ActionError, ModifierError};
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::NoModifier;
use trading::source::Item;
use trading::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};

/// Matches every candle.
struct EveryCandle;

#[derive(Debug)]
struct CandleIdx(i64);
impl TriggerValue for CandleIdx {}

impl Condition for EveryCandle {
    type Input = Candle;
    type Value = CandleIdx;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<CandleIdx>> {
        log("condition", format!("candle #{} matches", idx(item.data)));
        Some(Detection {
            event_type: TriggerEventType::Other("every"),
            value: CandleIdx(idx(item.data)),
        })
    }
    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<Self::Value>>, ModifierError> {
        match m {}
    }
}

/// Takes 300 ms (blocking); the 2nd run fails.
struct Slow {
    runs: u32,
}

impl Action for Slow {
    type Value = CandleIdx;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, CandleIdx, Candle>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        self.runs += 1;
        log("action", format!("start  (candle #{})", event.value.0));
        // Blocking work: it only delays the tick (executor thread).
        std::thread::sleep(Duration::from_millis(300));
        if self.runs == 2 {
            log("action", "end    -> FAILED");
            return Err(ActionError::Failed("simulated failure".into()));
        }
        log("action", "end    -> ok");
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    let src = g.source(common::replay("BTCUSDT", &[1, 2, 3, 4, 5]));
    let every = g.condition("every", EveryCandle, &src);
    g.action(Slow { runs: 0 }, &every);

    Executor::new(g).run()?;
    log("main", "done");
    Ok(())
}
