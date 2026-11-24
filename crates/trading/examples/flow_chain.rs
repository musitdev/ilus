//! Data propagation through a transformer chain, inside one tick.
//!
//! ```text
//! MockSource ──Candle──> Sma(3) ──Derived<Candle, sma>──> Slope ──Derived<Derived<Candle, sma>, slope>──> Rising ──> LogAction
//! ```
//!
//! What to check in the output:
//! - each tick runs the whole chain for one candle: sma, slope, condition, action;
//! - the first candles are *bootstrap* (warm-up of SMA + slope + condition):
//!   they go through the chain but produce no action event;
//! - the condition can read every level: candle, sma, slope.
//!
//! Run with: `cargo run --example flow_chain`

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
use trading::transformer::{Derived, Transformer};
use trading::trigger::{Detection, TriggerEventType, TriggerValue};

type SmaCandle = Derived<Candle, Decimal>;
type SlopeCandle = Derived<SmaCandle, Decimal>;

/// Round for display only.
fn r(v: Option<Decimal>) -> Option<Decimal> {
    v.map(|v| v.round_dp(3))
}

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
    fn warm_up(&self) -> usize {
        self.period
    }
    fn on_data(&mut self, item: Item<'_, Candle>) -> Option<Decimal> {
        let c = item.data;
        self.window.push(c.close);
        if self.window.len() > self.period {
            self.window.remove(0);
        }
        let sma = (self.window.len() == self.period)
            .then(|| self.window.iter().sum::<Decimal>() / Decimal::from(self.period));
        log(
            "sma",
            format!(
                "candle #{} close={} -> sma={:?} ({:?})",
                idx(c),
                c.close,
                r(sma),
                item.mode
            ),
        );
        sma
    }
    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
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
    fn warm_up(&self) -> usize {
        1
    }
    fn on_data(&mut self, item: Item<'_, SmaCandle>) -> Option<Decimal> {
        // No SMA yet -> no slope (warm-up propagates down the chain).
        let sma = item.data.value;
        let slope = sma.and_then(|s| self.prev.replace(s).map(|p| s - p));
        log("slope", format!("sma={:?} -> slope={:?}", r(sma), r(slope)));
        slope
    }
    fn on_modifier(&mut self, m: NoModifier) -> Result<(), ModifierError> {
        match m {}
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

    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, item: Item<'_, SlopeCandle>) -> Option<Detection<RisingBy>> {
        // Every level of the chain is available here.
        let candle = &item.data.data.data;
        let (sma, slope) = (item.data.data.value, item.data.value);
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
            value: RisingBy(slope.round_dp(3)),
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
    let src = g.source(common::replay(
        "BTCUSDT",
        &[10, 11, 12, 11, 10, 11, 13, 14, 15],
    ));
    let sma = g.transform(
        Sma {
            period: 3,
            window: vec![],
        },
        &src,
    );
    let slope = g.transform(Slope { prev: None }, &sma);
    let rising = g.condition("rising", Rising, &slope);
    g.action(LogAction::new("action"), &rising);

    log(
        "main",
        "bootstrap = 3 (sma) + 1 (slope) + 0 (condition) = 4 candles",
    );
    Executor::new(g).run()?;
    log("main", "executor stopped: the source has ended");
    Ok(())
}
