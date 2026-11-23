//! Data propagation through a transformer chain.
//!
//! ```text
//! MockSource ──Candle──> Sma(3) ──Derived<Candle, sma>──> Slope ──Derived<Derived<Candle, sma>, slope>──> Rising ──> LogAction
//! ```
//!
//! What to check in the output:
//! - every candle reaches the condition, even during warm-up (values are `None`);
//! - `None` propagates: Slope has no value while Sma has none;
//! - the condition can read every level: candle, sma, slope;
//! - when the source ends, the whole chain ends and `bot.run()` returns.
//!
//! Run with: `cargo run --example flow_chain`

mod common;

use common::{LogAction, idx, log, mock};
use ilus::bot::Bot;
use ilus::candle::Candle;
use ilus::condition::Condition;
use ilus::error::ModifierError;
use ilus::modifier::NoModifier;
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::transformer::{Derived, Transformed, Transformer};
use ilus::trigger::{Detection, TriggerEventType, TriggerValue};
use rust_decimal::Decimal;

type SmaCandle = Derived<Candle, Decimal>;

/// Round for display only.
fn r(v: Option<Decimal>) -> Option<Decimal> {
    v.map(|v| v.round_dp(3))
}
type SlopeCandle = Derived<SmaCandle, Decimal>;

/// Simple moving average of closes.
struct Sma {
    period: usize,
    window: Vec<Decimal>,
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

    fn on_data(&mut self, c: &Candle) -> Option<Decimal> {
        self.window.push(c.close);
        if self.window.len() > self.period {
            self.window.remove(0);
        }
        let sma = (self.window.len() == self.period)
            .then(|| self.window.iter().sum::<Decimal>() / Decimal::from(self.period));
        log(
            "sma",
            format!("candle #{} close={} -> sma={:?}", idx(c), c.close, r(sma)),
        );
        sma
    }
}

/// Difference between two consecutive SMA values.
struct Slope {
    prev: Option<Decimal>,
}

impl Transformer for Slope {
    type Input = SmaCandle;
    type Value = Decimal;
    type Modifier = NoModifier;

    fn name(&self) -> String {
        "slope".into()
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }

    fn warm_up(&self) -> usize {
        2
    }

    fn on_data(&mut self, item: &SmaCandle) -> Option<Decimal> {
        // No SMA yet -> no slope (warm-up propagates down the chain).
        let slope = item
            .value
            .and_then(|sma| self.prev.replace(sma).map(|p| sma - p));
        log(
            "slope",
            format!(
                "candle #{} sma={:?} -> slope={:?}",
                idx(&item.data),
                r(item.value),
                r(slope)
            ),
        );
        slope
    }
}

/// Triggers when the slope is positive.
struct Rising;

#[derive(Debug)]
#[allow(dead_code)] // read through Debug
struct RisingBy(Decimal);
impl TriggerValue for RisingBy {}

impl Condition for Rising {
    type Input = SlopeCandle;
    type Value = RisingBy;
    type Modifier = NoModifier;

    fn on_data(&mut self, item: &SlopeCandle) -> Option<Detection<RisingBy>> {
        // Every level of the chain is available here.
        let candle = &item.data.data;
        let sma = item.data.value;
        let slope = item.value;
        log(
            "condition",
            format!(
                "candle #{} close={} sma={:?} slope={:?}",
                idx(candle),
                candle.close,
                r(sma),
                r(slope)
            ),
        );
        let slope = slope.filter(|s| *s > Decimal::ZERO)?;
        Some(Detection {
            event_type: TriggerEventType::Other("rising"),
            value: RisingBy(slope),
        })
    }

    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bot = Bot::new();

    let mut src = mock("BTCUSDT", &[10, 11, 12, 11, 10, 11, 13, 14], 100);
    let mut sma = Transformed::new(
        Sma {
            period: 3,
            window: vec![],
        },
        src.get_source_stream()?,
    );
    let mut slope = Transformed::new(Slope { prev: None }, sma.get_source_stream()?);
    let stream = slope.get_source_stream()?;
    log("main", format!("rule source = {}", stream.info()));

    bot.rule(Rule::new("rising", stream, Rising, LogAction));
    bot.source(src);
    bot.source(sma);
    bot.source(slope);

    bot.run().await;
    log("main", "bot.run() returned: source ended, chain ended");
    Ok(())
}
