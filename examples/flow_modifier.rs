//! The action -> modifier -> condition loop.
//!
//! ```text
//! MockSource ──> Rsi(3) ──> RsiBand ──> Flip
//!                              ^          │
//!                              └──────────┘ Watch(side) (modifier)
//! ```
//!
//! The condition watches ONE side at a time (oversold or overbought). When it
//! triggers, the action flips it to the other side with a modifier. The
//! modifier is applied at the end of the tick, so the very next candle is
//! already evaluated with the new side.
//!
//! What to check in the output:
//! - "modifier applied" appears right after the action, before the next candle;
//! - after each flip, only the other side triggers;
//! - an invalid modifier is rejected and leaves the condition unchanged.
//!
//! Run with: `cargo run --example flow_modifier`

mod common;

use common::{idx, log};
use ilus::action::{Action, ActionContext};
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::{ModifierHandle, ModifierValue};
use ilus::source::Item;
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Oversold,
    Overbought,
}

/// Which side triggered, and the RSI.
#[derive(Debug)]
struct Signal {
    side: Side,
    rsi: Decimal,
}
impl TriggerValue for Signal {}

/// Modifiers accepted by [`RsiBand`].
#[derive(Debug)]
enum BandModifier {
    Watch(Side),
    /// Sets the low limit; rejected if outside 0..=100.
    SetLow(Decimal),
}
impl ModifierValue for BandModifier {}

struct RsiBand {
    side: Side,
    low: Decimal,
    high: Decimal,
}

impl Condition for RsiBand {
    type Input = RsiCandle;
    type Value = Signal;
    type Modifier = BandModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Signal>> {
        let rsi = item.data.value.map(|v| v.round_dp(1));
        log(
            "condition",
            format!(
                "candle #{} close={} rsi={rsi:?} watching={:?} ({:?})",
                idx(&item.data.data),
                item.data.data.close,
                self.side,
                item.mode
            ),
        );
        let rsi = rsi?;
        let hit = match self.side {
            Side::Oversold => rsi < self.low,
            Side::Overbought => rsi > self.high,
        };
        hit.then_some(Detection {
            event_type: TriggerEventType::Other("rsi_band"),
            value: Signal {
                side: self.side,
                rsi,
            },
        })
    }

    fn on_modifier(&mut self, m: BandModifier) -> Result<(), ModifierError> {
        match m {
            BandModifier::Watch(side) => {
                log("condition", format!("modifier applied: watch {side:?}"));
                self.side = side;
                Ok(())
            }
            BandModifier::SetLow(v) if (Decimal::ZERO..=Decimal::ONE_HUNDRED).contains(&v) => {
                log("condition", format!("modifier applied: low={v}"));
                self.low = v;
                Ok(())
            }
            BandModifier::SetLow(v) => {
                log("condition", format!("modifier REJECTED: low={v}"));
                Err(ModifierError::Rejected(format!(
                    "low must be in 0..=100, got {v}"
                )))
            }
        }
    }
}

/// Flips the watched side after each trigger. The first time, it also sends
/// an invalid modifier, to show that it is rejected.
struct Flip {
    band: ModifierHandle<BandModifier>,
    first: bool,
}

impl Action for Flip {
    type Value = Signal;
    type Input = RsiCandle;

    async fn run(
        &mut self,
        event: TriggerEvent<'_, Signal, RsiCandle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let next = match event.value.side {
            Side::Oversold => Side::Overbought,
            Side::Overbought => Side::Oversold,
        };
        log(
            "action",
            format!(
                "{:?} at rsi={} -> send Watch({next:?})",
                event.value.side, event.value.rsi
            ),
        );
        if std::mem::take(&mut self.first) {
            log("action", "send SetLow(150) (invalid)");
            ctx.send(&self.band, BandModifier::SetLow(Decimal::from(150)));
        }
        ctx.send(&self.band, BandModifier::Watch(next));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    // Falling, rising, then falling again.
    let src = g.source(common::replay(
        "SOLUSDT",
        &[20, 19, 18, 17, 16, 17, 18, 19, 20, 21, 20, 19, 18, 17, 16],
    ));
    let rsi = g.transform(Rsi::new(3)?, &src);
    let band = g.condition(
        "rsi-band",
        RsiBand {
            side: Side::Oversold,
            low: Decimal::from(30),
            high: Decimal::from(70),
        },
        &rsi,
    );
    let handle = g.modifier_handle(&band);
    g.action(
        Flip {
            band: handle,
            first: true,
        },
        &band,
    );

    Executor::new(g).run().await;
    log("main", "done");
    Ok(())
}
