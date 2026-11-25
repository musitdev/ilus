//! Several sources in one graph: at each global tick (bootstrap and replay),
//! every source is called one after the other and delivers one item.
//!
//! ```text
//! MockSource "FAST" (8 candles) ──> Above(5)   ──> LogAction
//! MockSource "SLOW" (3 candles) ──> Above(150) ──> LogAction
//! ```
//!
//! What to check in the output:
//! - the two sources alternate, one candle each per tick (FAST, SLOW, FAST, ...);
//! - once SLOW has ended, FAST continues alone;
//! - each condition only sees its own source;
//! - the executor stops when every source has ended.
//!
//! Run with: `cargo run --example flow_parallel`

mod common;

use common::{LogAction, idx, log};
use rust_decimal::Decimal;
use trading::candle::Candle;
use trading::condition::Condition;
use trading::error::ModifierError;
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::NoModifier;
use trading::source::Item;
use trading::trigger::{Detection, TriggerEventType, TriggerValue};

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

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<Close>> {
        let c = item.data;
        log(self.name, format!("candle #{} close={}", idx(c), c.close));
        (c.close > self.level).then_some(Detection {
            event_type: TriggerEventType::Other("above"),
            value: Close(c.close),
        })
    }
    fn on_modifier(
        &mut self,
        m: NoModifier,
    ) -> Result<Option<Detection<Self::Value>>, ModifierError> {
        match m {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    let fast = g.source(common::replay("FAST", &[1, 2, 3, 4, 5, 6, 7, 8]));
    let slow = g.source(common::replay("SLOW", &[100, 200, 300]));

    let fast_above = g.condition(
        "fast-above-5",
        Above {
            name: "cond FAST",
            level: Decimal::from(5),
        },
        &fast,
    );
    let slow_above = g.condition(
        "slow-above-150",
        Above {
            name: "cond SLOW",
            level: Decimal::from(150),
        },
        &slow,
    );
    g.action(LogAction::new("action"), &fast_above);
    g.action(LogAction::new("action"), &slow_above);

    Executor::new(g).run()?;
    log("main", "executor stopped: all sources ended");
    Ok(())
}
