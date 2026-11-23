//! Event queue and backpressure: what happens when the action is slower than the feed.
//!
//! ```text
//! MockSource (1 candle / 100 ms) ──> EveryCandle ──queue (max 3)──> Slow (350 ms, fails on 2nd run)
//! ```
//!
//! Every candle matches. What to check in the output:
//! - no event is lost: the action processes every candle, in order;
//! - while the queue holds fewer than 3 events, the condition keeps reading;
//! - when 3 events are pending, the condition stops reading ("paused") until the
//!   queue is empty, then resumes: the source is slowed down;
//! - a failing action is logged and the next event is processed anyway.
//!
//! Run with: `cargo run --example flow_slow_action`

mod common;

use std::time::Duration;

use common::{idx, log, mock};
use ilus::action::{Action, EventOf};
use ilus::bot::Bot;
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::{ModifierSender, NoModifier};
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};

/// Matches every candle.
struct EveryCandle;

#[derive(Debug)]
struct CandleIdx(i64);
impl TriggerValue for CandleIdx {}

impl Condition for EveryCandle {
    type Input = Candle;
    type Value = CandleIdx;
    type Modifier = NoModifier;

    fn on_data(&mut self, c: &Candle) -> Option<Detection<CandleIdx>> {
        log("condition", format!("candle #{} matches", idx(c)));
        Some(Detection {
            event_type: TriggerEventType::Other("every"),
            value: CandleIdx(idx(c)),
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

/// Takes 350 ms; the 2nd run fails.
struct Slow {
    runs: u32,
}

impl Action<EveryCandle> for Slow {
    async fn run(
        &mut self,
        event: EventOf<EveryCandle>,
        _modifiers: &ModifierSender<NoModifier>,
    ) -> Result<(), ActionError> {
        self.runs += 1;
        log(
            "action",
            format!("start  (candle #{}) run {}", event.value.0, self.runs),
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
        if self.runs == 2 {
            log("action", "end    -> FAILED");
            return Err(ActionError::Failed("simulated failure".into()));
        }
        log("action", "end    -> ok");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bot = Bot::new();
    let mut src = mock("BTCUSDT", &[1, 2, 3, 4, 5, 6, 7, 8], 100);

    bot.rule(
        Rule::new(
            "every",
            src.get_source_stream()?,
            EveryCandle,
            Slow { runs: 0 },
        )
        .with_max_pending(3),
    );
    bot.source(src);

    bot.run().await;
    log("main", "done");
    Ok(())
}
