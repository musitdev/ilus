//! Simulation of the RSI divergence strategy on SOL/USDT **futures** with leverage.
//!
//! ```text
//! BinanceSource(SOLUSDT, tf, period) ──> Rsi(14) ──> WithTicks(RsiDivergence) ──> Futures
//! ```
//!
//! Rules:
//! - capital: 10 SOL, converted to USDT at the first candle close; start flat;
//! - bullish divergence (price minimum): close the short (if any), open a LONG;
//! - bearish divergence (price maximum): close the long (if any), open a SHORT;
//! - every position uses the whole equity as margin, with `leverage` (default 10x);
//! - prices: close of the candle that confirms the divergence;
//! - fee (in %) on the position notional, at each open and each close;
//! - liquidation checked on every candle (low for a long, high for a short):
//!   when equity falls to the maintenance margin (0.5 % of notional), the
//!   margin is lost; with no equity left the simulation stops trading;
//! - optional stop-loss (`--stop P`, in %): the position is closed when the
//!   price moves P % against the entry (low for a long, high for a short);
//!   filled at the stop price, or at the candle open if it opens beyond it;
//!   after a stop, the account stays flat until the next divergence;
//! - period: `--days N` (from N days ago, 00:00 UTC, until now; default 1), or
//!   `--from YYYY-MM-DD [--to YYYY-MM-DD]` (00:00 UTC; `--to` excluded, default now);
//! - divergence pivots: `--left N` / `--right N` candles (default 3 / 3);
//!   `right` is the confirmation delay; `--min-gap N` / `--max-gap N`: distance
//!   between two compared pivots, in candles (default: 60 max; min 5 at 1m,
//!   3 at 5m, 2 from 15m up);
//! - funding rates are ignored.
//!
//! The condition is wrapped so that it also emits a "tick" event for every
//! candle: the action needs every candle to check liquidation.
//!
//! Run with:
//! `cargo run --example backtest_futures -- [--days N | --from D [--to D]] [--leverage L] [--stop P] [--left N] [--right N] [--min-gap N] [--max-gap N] [timeframe] [fees %...]`
//! e.g. `cargo run --example backtest_futures -- --days 7 --stop 3 15m 0 0.05 0.2`

#[path = "support/futures.rs"]
mod futures;

use futures::{Futures, WithTicks, parse_args};
use ilus::bot::Bot;
use ilus::condition::divergence::RsiDivergence;
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::source::binance::BinanceSource;
use ilus::transformer::Transformed;
use ilus::transformer::rsi::Rsi;
use rust_decimal::Decimal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    println!("{}", args.header);

    let mut bot = Bot::new();
    for &fee in &args.fees {
        let mut sol = BinanceSource::new("SOLUSDT", args.timeframe)
            .with_start(args.from)
            .with_end(args.to);
        let mut sol_rsi = Transformed::new(Rsi::new(14)?, sol.get_source_stream()?);
        bot.rule(Rule::new(
            format!("futures-fee-{fee}"),
            sol_rsi.get_source_stream()?,
            WithTicks::new(RsiDivergence::new(args.params)?),
            Futures::new(Decimal::from(10), args.leverage, args.stop, fee, ()),
        ));
        bot.source(sol);
        bot.source(sol_rsi);
    }
    bot.run().await;
    Ok(())
}
