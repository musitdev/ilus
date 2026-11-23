//! RSI divergence detector on live Binance data.
//!
//! ```text
//! BinanceSource(SOLUSDT, 1m) ──> Rsi(14) ──> RsiDivergence ──> PrintDivergence
//! ```
//!
//! - Bullish divergence (bear phase): price lower low, RSI higher low.
//! - Bearish divergence (bull phase): price higher high, RSI lower high.
//!
//! Bootstraps the last 500 candles (warm-up only: bootstrap produces no event),
//! then prints each divergence detected on the live feed. To see past
//! divergences, use the backtest examples (replay mode).
//!
//! Run with: `cargo run --example rsi_divergence` (Ctrl+C to stop).

use ilus::action::{Action, ActionContext};
use ilus::condition::divergence::{
    Divergence, DivergenceKind, DivergenceParams, Pivot, RsiDivergence,
};
use ilus::error::ActionError;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::source::Timeframe;
use ilus::source::binance::BinanceSource;
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::TriggerEvent;

/// Prints a divergence in a readable form.
struct PrintDivergence;

fn pivot(p: &Pivot) -> String {
    format!(
        "{}  price={:<10} rsi={}",
        p.time.format("%Y-%m-%d %H:%M"),
        p.price.normalize(),
        p.rsi.round_dp(2)
    )
}

impl Action for PrintDivergence {
    type Value = Divergence;
    type Input = RsiCandle;

    async fn run(
        &mut self,
        event: TriggerEvent<'_, Divergence, RsiCandle>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let d = event.value;
        let (title, price, rsi) = match d.kind {
            DivergenceKind::Bullish => {
                ("BULLISH divergence (bear phase)", "lower low", "higher low")
            }
            DivergenceKind::Bearish => (
                "BEARISH divergence (bull phase)",
                "higher high",
                "lower high",
            ),
        };
        println!("=== {title} on {}", event.source);
        println!("  A  {}", pivot(&d.first));
        println!("  B  {}", pivot(&d.second));
        println!("  price: {price}, RSI: {rsi}");
        println!("  confirmed at {}", d.confirmed_at.format("%Y-%m-%d %H:%M"));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut g = Graph::new();
    // Bootstrap: 500 candles to warm up RSI and pivots (no output), then live.
    let sol = g.source(BinanceSource::new("SOLUSDT", Timeframe::M1).with_bootstrap(500));
    let sol_rsi = g.transform(Rsi::new(14)?, &sol);
    let divergence = g.condition(
        "sol-rsi-divergence",
        RsiDivergence::new(DivergenceParams::for_timeframe(Timeframe::M1))?,
        &sol_rsi,
    );
    g.action(PrintDivergence, &divergence);

    Executor::new(g).run().await;
    Ok(())
}
