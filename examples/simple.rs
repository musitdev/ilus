//! Simple example wiring every building block:
//!
//! ```text
//! MockSource A (candles) ──> CloseAbove ──> RaiseThreshold ─┐ (sends modifier)
//!                               ^──────── SetThreshold ─────┘
//! MockSource B (candles) ──> Sma(3) ──> SmaAbove ──> Alert
//! ```
//!
//! One consumer per source: each chain has its own source.
//!
//! Run with: `cargo run --example simple`

use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use ilus::action::{Action, Alert, EventOf};
use ilus::bot::Bot;
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::SourceError;
use ilus::error::{ActionError, ModifierError};
use ilus::modifier::{ModifierSender, ModifierValue, NoModifier};
use ilus::rule::Rule;
use ilus::source::mock::MockSource;
use ilus::source::{Source, Timeframe};
use ilus::transformer::{Derived, Transformed, Transformer};
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

// ---------------------------------------------------------------- Condition 1
/// Triggers when a candle closes above a threshold.
struct CloseAbove {
    threshold: Decimal,
}

/// Value emitted by [`CloseAbove`].
#[derive(Debug)]
#[allow(dead_code)] // fields are read through Debug by the alert
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

    fn on_data(&mut self, candle: &Candle) -> Option<Detection<CloseSignal>> {
        (candle.close > self.threshold).then_some(Detection {
            event_type: TriggerEventType::Other("close_above"),
            value: CloseSignal {
                close: candle.close,
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
/// Alerts, then raises the condition threshold to the triggering close + `step`.
struct RaiseThreshold {
    step: Decimal,
}

impl Action<CloseAbove> for RaiseThreshold {
    async fn run(
        &mut self,
        event: EventOf<CloseAbove>,
        modifiers: &ModifierSender<CloseAboveModifier>,
    ) -> Result<(), ActionError> {
        let next = event.value.close + self.step;
        println!(
            "[RAISE] rule={} close={} > {} -> new threshold {next}",
            event.rule, event.value.close, event.value.threshold
        );
        modifiers
            .send(CloseAboveModifier::SetThreshold(next))
            .map_err(|e| ActionError::Failed(e.to_string()))
    }
}

// ---------------------------------------------------------------- Transformer
/// Simple moving average of closes over `period` candles.
struct Sma {
    period: usize,
    window: VecDeque<Decimal>,
}

impl Sma {
    fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
        }
    }
}

impl Transformer for Sma {
    type Input = Candle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        format!("sma({})", self.period)
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }

    fn warm_up(&self) -> usize {
        self.period
    }

    fn on_data(&mut self, candle: &Candle) -> Option<Decimal> {
        self.window.push_back(candle.close);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        (self.window.len() == self.period)
            .then(|| self.window.iter().sum::<Decimal>() / Decimal::from(self.period))
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

    fn on_data(&mut self, item: &Derived<Candle, Decimal>) -> Option<Detection<Level>> {
        let sma = item.value?; // None while the SMA warms up
        (sma > self.level).then_some(Detection {
            event_type: TriggerEventType::Other("sma_above"),
            value: Level(sma),
        })
    }

    fn on_modifier(&mut self, modifier: NoModifier) -> Result<(), ModifierError> {
        match modifier {}
    }
}

// ----------------------------------------------------------------------- Data
fn candle(minute: i64, close: i64) -> Candle {
    let close = Decimal::from(close);
    let open_time: DateTime<Utc> = DateTime::UNIX_EPOCH + TimeDelta::minutes(minute);
    Candle {
        open_time,
        open: close,
        high: close,
        low: close,
        close,
        volume: Decimal::ONE,
    }
}

fn btc_source() -> MockSource<Candle> {
    let closes = [100, 102, 105, 103, 108, 110, 107, 112, 115, 111];
    let candles = closes
        .iter()
        .zip(0..)
        .map(|(&c, minute)| candle(minute, c))
        .collect();
    MockSource::new("BTCUSDT", Timeframe::M1, candles).with_interval(Duration::from_millis(100))
}

#[tokio::main]
async fn main() -> Result<(), SourceError> {
    let mut bot = Bot::new();

    // Rule 1: close above threshold; the action raises the threshold via a modifier.
    let mut btc_a = btc_source();
    bot.rule(Rule::new(
        "close-above",
        btc_a.get_source_stream()?,
        CloseAbove {
            threshold: Decimal::from(101),
        },
        RaiseThreshold {
            step: Decimal::from(3),
        },
    ));
    bot.source(btc_a);

    // Rule 2: candles -> SMA(3) -> SMA above a level, plain alert.
    let mut btc_b = btc_source();
    let mut btc_sma = Transformed::new(Sma::new(3), btc_b.get_source_stream()?);
    bot.rule(Rule::new(
        "sma-above",
        btc_sma.get_source_stream()?,
        SmaAbove {
            level: Decimal::from(110),
        },
        Alert,
    ));
    bot.source(btc_b);
    bot.source(btc_sma);

    bot.run().await;
    Ok(())
}
