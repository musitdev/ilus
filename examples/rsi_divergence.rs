//! RSI divergence detector on live Binance data.
//!
//! ```text
//! BinanceSource(SOLUSDT, 1m) ──> Rsi(14) ──> RsiDivergence ──> PrintDivergence
//! ```
//!
//! - Bullish divergence (bear phase): price lower low, RSI higher low.
//! - Bearish divergence (bull phase): price higher high, RSI lower high.
//!
//! Loads the last 500 candles (past divergences are printed immediately),
//! then follows the live feed.
//!
//! Run with: `cargo run --example rsi_divergence` (Ctrl+C to stop).

use ilus::action::{Action, EventOf};
use ilus::bot::Bot;
use ilus::condition::divergence::{DivergenceKind, DivergenceParams, Pivot, RsiDivergence};
use ilus::error::ActionError;
use ilus::modifier::{ModifierSender, NoModifier};
use ilus::rule::Rule;
use ilus::source::binance::BinanceSource;
use ilus::source::{Source, Timeframe};
use ilus::transformer::Transformed;
use ilus::transformer::rsi::Rsi;

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

impl Action<RsiDivergence> for PrintDivergence {
    async fn run(
        &mut self,
        event: EventOf<RsiDivergence>,
        _modifiers: &ModifierSender<NoModifier>,
    ) -> Result<(), ActionError> {
        let d = &event.value;
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
    let mut bot = Bot::new();

    let mut sol = BinanceSource::new("SOLUSDT", Timeframe::M1).with_history(500);
    let mut sol_rsi = Transformed::new(Rsi::new(14)?, sol.get_source_stream()?);

    bot.rule(Rule::new(
        "sol-rsi-divergence",
        sol_rsi.get_source_stream()?,
        RsiDivergence::new(DivergenceParams::default())?,
        PrintDivergence,
    ));
    bot.source(sol);
    bot.source(sol_rsi);

    bot.run().await;
    Ok(())
}
