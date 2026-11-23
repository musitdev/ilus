//! Simple example wiring every building block in a graph:
//!
//! ```text
//!                 ┌──> CloseAbove ──> RaiseThreshold ─┐ (modifier: SetThreshold)
//! MockSource ─────┤        ^──────────────────────────┘
//!   (candles)     └──> Sma(3) ──> SmaAbove ──> Alert
//! ```
//!
//! One source feeds two branches (fan-out). The first 3 candles bootstrap the
//! SMA (warm-up): they produce no event.
//!
//! Run with: `cargo run --example simple`

mod common;

use std::collections::VecDeque;

use ilus::action::{Action, ActionContext, Alert};
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::{ActionError, ModifierError};
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::{ModifierHandle, ModifierValue, NoModifier};
use ilus::source::Item;
use ilus::transformer::{Derived, Transformer};
use ilus::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

// ---------------------------------------------------------------- Condition 1
/// Triggers when a candle closes above a threshold.
struct CloseAbove {
    threshold: Decimal,
}

/// Value emitted by [`CloseAbove`].
#[derive(Debug)]
struct CloseSignal {
    close: Decimal,
    threshold: Decimal,
}
impl TriggerValue for CloseSignal {}

/// Modifier accepted by [`CloseAbove`].
#[derive(Debug)]
enum CloseAboveModifier {
    SetThreshold(Decimal),
}
impl ModifierValue for CloseAboveModifier {}

impl Condition for CloseAbove {
    type Input = Candle;
    type Value = CloseSignal;
    type Modifier = CloseAboveModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Detection<CloseSignal>> {
        (item.data.close > self.threshold).then_some(Detection {
            event_type: TriggerEventType::Other("close_above"),
            value: CloseSignal {
                close: item.data.close,
                threshold: self.threshold,
            },
        })
    }

    fn on_modifier(&mut self, modifier: CloseAboveModifier) -> Result<(), ModifierError> {
        match modifier {
            CloseAboveModifier::SetThreshold(t) if t > Decimal::ZERO => {
                self.threshold = t;
                Ok(())
            }
            CloseAboveModifier::SetThreshold(t) => Err(ModifierError::Rejected(format!(
                "threshold must be > 0, got {t}"
            ))),
        }
    }
}

// ------------------------------------------------------------------- Action 1
/// Prints, then raises the condition threshold to the triggering close + `step`.
struct RaiseThreshold {
    step: Decimal,
    condition: ModifierHandle<CloseAboveModifier>,
}

impl Action for RaiseThreshold {
    type Value = CloseSignal;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, CloseSignal, Candle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let next = event.value.close + self.step;
        println!(
            "[RAISE] {} close={} > {} -> new threshold {next}",
            event.condition, event.value.close, event.value.threshold
        );
        // Applied at the end of this tick: the next candle uses the new threshold.
        ctx.send(&self.condition, CloseAboveModifier::SetThreshold(next));
        Ok(())
    }
}

// ---------------------------------------------------------------- Transformer
/// Simple moving average of closes over `period` candles.
struct Sma {
    period: usize,
    window: VecDeque<Decimal>,
}

impl Transformer for Sma {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("sma({})", self.period)
    }

    fn warm_up(&self) -> usize {
        self.period
    }

    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        self.window.push_back(item.data.close);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        (self.window.len() == self.period)
            .then(|| self.window.iter().sum::<Decimal>() / Decimal::from(self.period))
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

// ---------------------------------------------------------------- Condition 2
/// Triggers when the SMA of a candle is above a level.
struct SmaAbove {
    level: Decimal,
}

#[derive(Debug)]
#[allow(dead_code)] // read through Debug by the alert
struct Level(Decimal);
impl TriggerValue for Level {}

impl Condition for SmaAbove {
    type Input = Derived<Candle, Decimal>;
    type Value = Level;
    type Modifier = NoModifier;

    fn warm_up(&self) -> usize {
        0
    }

    fn on_data(&mut self, item: Item<'_, Derived<Candle, Decimal>>) -> Option<Detection<Level>> {
        let sma = item.data.value?; // None while the SMA warms up
        (sma > self.level).then_some(Detection {
            event_type: TriggerEventType::Other("sma_above"),
            value: Level(sma.round_dp(2)),
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    let btc = g.source(common::replay(
        "BTCUSDT",
        &[100, 102, 105, 103, 108, 110, 107, 112, 115, 111],
    ));

    // Branch 1: close above threshold; the action raises the threshold.
    let close_above = g.condition(
        "close-above",
        CloseAbove {
            threshold: Decimal::from(101),
        },
        &btc,
    );
    let handle = g.modifier_handle(&close_above);
    g.action(
        RaiseThreshold {
            step: Decimal::from(3),
            condition: handle,
        },
        &close_above,
    );

    // Branch 2: candles -> SMA(3) -> SMA above a level -> alert.
    let sma = g.transform(
        Sma {
            period: 3,
            window: VecDeque::new(),
        },
        &btc,
    );
    let sma_above = g.condition(
        "sma-above",
        SmaAbove {
            level: Decimal::from(110),
        },
        &sma,
    );
    g.action(Alert::new(), &sma_above);

    Executor::new(g).run()?;
    Ok(())
}
