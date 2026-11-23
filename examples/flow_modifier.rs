//! The action -> modifier -> condition loop.
//!
//! ```text
//! MockSource ──> Rsi(3) ──> RsiBand ──trigger──> Flip
//!                              ^                   │
//!                              └──── Watch(mode) ──┘  (modifier)
//! ```
//!
//! The condition watches ONE side at a time (oversold or overbought). When it
//! triggers, the action flips it to the other side through a modifier, so
//! the bot alternates: oversold -> overbought -> oversold ...
//!
//! What to check in the output:
//! - the modifier is applied ("applied") BEFORE the next candle is evaluated;
//! - after each flip, only the other side triggers;
//! - an invalid modifier is rejected and leaves the condition unchanged.
//!
//! Run with: `cargo run --example flow_modifier`

mod common;

use common::{idx, log, mock};
use ilus::action::{Action, EventOf};
use ilus::bot::Bot;
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::{ModifierSender, ModifierValue};
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::transformer::Transformed;
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Oversold,
    Overbought,
}

/// Value emitted: which side triggered and the RSI.
#[derive(Debug)]
struct Signal {
    side: Mode,
    rsi: Decimal,
}
impl TriggerValue for Signal {}

/// Modifiers accepted by [`RsiBand`].
#[derive(Debug)]
enum BandModifier {
    Watch(Mode),
    /// Sets the low limit; rejected if outside 0..=100.
    SetLow(Decimal),
}
impl ModifierValue for BandModifier {}

struct RsiBand {
    mode: Mode,
    low: Decimal,
    high: Decimal,
}

impl Condition for RsiBand {
    type Input = RsiCandle;
    type Value = Signal;
    type Modifier = BandModifier;

    fn on_data(&mut self, item: &RsiCandle) -> Option<Detection<Signal>> {
        let rsi = item.value.map(|v| v.round_dp(1));
        log(
            "condition",
            format!(
                "candle #{} close={} rsi={rsi:?} watching={:?}",
                idx(&item.data),
                item.data.close,
                self.mode
            ),
        );
        let rsi = rsi?;
        let hit = match self.mode {
            Mode::Oversold => rsi < self.low,
            Mode::Overbought => rsi > self.high,
        };
        hit.then_some(Detection {
            event_type: TriggerEventType::Other("rsi_band"),
            value: Signal {
                side: self.mode,
                rsi,
            },
        })
    }

    fn on_modifier(&mut self, m: BandModifier) -> Result<(), ModifierError> {
        match m {
            BandModifier::Watch(mode) => {
                log("condition", format!("modifier applied: watch {mode:?}"));
                self.mode = mode;
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
/// an invalid modifier to show that it is rejected.
struct Flip {
    first: bool,
}

impl Action<RsiBand> for Flip {
    async fn run(
        &mut self,
        event: EventOf<RsiBand>,
        modifiers: &ModifierSender<BandModifier>,
    ) -> Result<(), ActionError> {
        let next = match event.value.side {
            Mode::Oversold => Mode::Overbought,
            Mode::Overbought => Mode::Oversold,
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
            modifiers
                .send(BandModifier::SetLow(Decimal::from(150)))
                .map_err(|e| ActionError::Failed(e.to_string()))?;
        }
        modifiers
            .send(BandModifier::Watch(next))
            .map_err(|e| ActionError::Failed(e.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bot = Bot::new();

    // Falling, rising, then falling again.
    let closes = [20, 19, 18, 17, 16, 17, 18, 19, 20, 21, 20, 19, 18, 17, 16];
    let mut src = mock("SOLUSDT", &closes, 100);
    let mut rsi = Transformed::new(Rsi::new(3)?, src.get_source_stream()?);

    bot.rule(Rule::new(
        "rsi-band",
        rsi.get_source_stream()?,
        RsiBand {
            mode: Mode::Oversold,
            low: Decimal::from(30),
            high: Decimal::from(70),
        },
        Flip { first: true },
    ));
    bot.source(src);
    bot.source(rsi);

    bot.run().await;
    log("main", "done");
    Ok(())
}
