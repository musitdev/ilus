//! Two independent chains linked by a modifier: a condition can trigger when
//! it receives a modifier (the modifier is one of its rules).
//!
//! ```text
//! Chain K (e.g. Kucoin 1h):    MockSource ──> Rsi(3) ──> ArmedRsiBelow(40) ──> Print "K"
//!                                                              ^
//!                                                              │ Arm (modifier)
//! Chain B (e.g. Binance 15m):  MockSource ──> Rsi(3) ──> RsiBelow(30) ──> ArmK
//! ```
//!
//! K triggers when it is **armed** and its RSI is below 40. B arms it when its
//! own RSI goes below 30. What to check in the output:
//! - in the tick where K already has a low RSI but is not armed, B triggers,
//!   its modifier arms K, and K triggers **in the same tick**, with its last
//!   candle;
//! - after that, K triggers on its own data while its RSI stays low; a new
//!   `Arm` while already armed is ignored by K's rules (no double trigger).
//!
//! Both sources are replayed one candle per tick (mock data, no network).
//!
//! Run with: `cargo run --example flow_cross`

mod common;

use common::{idx, log};
use rust_decimal::Decimal;
use trading::action::{Action, ActionContext};
use trading::condition::Condition;
use trading::error::{ActionError, ModifierError};
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::{ModifierHandle, ModifierValue, NoModifier};
use trading::source::Item;
use trading::transformer::rsi::{Rsi, RsiCandle};
use trading::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};

#[derive(Debug)]
struct Low(Decimal);
impl TriggerValue for Low {}

/// Modifier: "the other chain gave its signal".
#[derive(Debug)]
struct Arm;
impl ModifierValue for Arm {}

/// Chain K: triggers when armed **and** RSI < level.
struct ArmedRsiBelow {
    level: Decimal,
    armed: bool,
    last_rsi: Option<Decimal>,
}

impl ArmedRsiBelow {
    /// The rules, checked on new data and on a modifier.
    fn check(&self) -> Option<Detection<Low>> {
        let rsi = self.last_rsi?;
        (self.armed && rsi < self.level).then_some(Detection {
            event_type: TriggerEventType::Other("armed_rsi_below"),
            value: Low(rsi.round_dp(1)),
        })
    }
}

impl Condition for ArmedRsiBelow {
    type Input = RsiCandle;
    type Value = Low;
    type Modifier = Arm;

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Low>> {
        self.last_rsi = item.data.value;
        log(
            "cond K",
            format!(
                "candle #{} rsi={:?} armed={} ({:?})",
                idx(&item.data.data),
                self.last_rsi.map(|r| r.round_dp(1)),
                self.armed,
                item.mode
            ),
        );
        self.check()
    }
    fn on_modifier(&mut self, _: Arm) -> Result<Option<Detection<Low>>, ModifierError> {
        // Rule: only a *new* arming is a reason to trigger (already armed:
        // the data rule has already been checked on the last candle).
        if std::mem::replace(&mut self.armed, true) {
            log("cond K", "modifier Arm received (already armed)");
            return Ok(None);
        }
        log("cond K", "modifier Arm received -> armed");
        Ok(self.check())
    }
}

/// Chain B: triggers when RSI < level.
struct RsiBelow(Decimal);

impl Condition for RsiBelow {
    type Input = RsiCandle;
    type Value = Low;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, RsiCandle>) -> Option<Detection<Low>> {
        let rsi = item.data.value?;
        log(
            "cond B",
            format!(
                "candle #{} rsi={} ({:?})",
                idx(&item.data.data),
                rsi.round_dp(1),
                item.mode
            ),
        );
        (rsi < self.0).then_some(Detection {
            event_type: TriggerEventType::Other("rsi_below"),
            value: Low(rsi.round_dp(1)),
        })
    }
    fn on_modifier(&mut self, m: NoModifier) -> Result<Option<Detection<Low>>, ModifierError> {
        match m {}
    }
}

/// Chain B's action: arms chain K.
struct ArmK(ModifierHandle<Arm>);

impl Action for ArmK {
    type Value = Low;
    type Input = RsiCandle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Low, RsiCandle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        log(
            "action B",
            format!("rsi={} -> send Arm to K", event.value.0),
        );
        ctx.send(&self.0, Arm);
        Ok(())
    }
}

/// Chain K's action.
struct PrintK;

impl Action for PrintK {
    type Value = Low;
    type Input = RsiCandle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Low, RsiCandle>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        log(
            "action K",
            format!(
                "TRIGGERED: rsi={} on candle #{}",
                event.value.0,
                idx(&event.data.data)
            ),
        );
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();

    // Chain K first: its modifier handle is given to chain B's action.
    let k = g.source(common::replay(
        "KUCOIN-1h",
        &[20, 19, 18, 17, 16, 15, 15, 14],
    ));
    let k_rsi = g.transform(Rsi::new(3)?, &k);
    let k_cond = g.condition(
        "k-armed-rsi",
        ArmedRsiBelow {
            level: Decimal::from(40),
            armed: false,
            last_rsi: None,
        },
        &k_rsi,
    );
    let arm_k = g.modifier_handle(&k_cond);
    g.action(PrintK, &k_cond);

    // Chain B: its RSI goes low later.
    let b = g.source(common::replay(
        "BINANCE-15m",
        &[10, 11, 12, 13, 12, 11, 10, 9],
    ));
    let b_rsi = g.transform(Rsi::new(3)?, &b);
    let b_cond = g.condition("b-rsi-below", RsiBelow(Decimal::from(30)), &b_rsi);
    g.action(ArmK(arm_k), &b_cond);

    Executor::new(g).run()?;
    log("main", "done");
    Ok(())
}
