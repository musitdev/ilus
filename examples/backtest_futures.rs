//! Simulation of the RSI divergence strategy on SOL/USDT **futures** with leverage.
//!
//! ```text
//! BinanceSource(SOLUSDT, tf, replay) ──> Rsi(14) ──> WithTicks(RsiDivergence) ──┬─> Futures (fee 1)
//!                                                                                 └─> Futures (fee 2) ...
//! ```
//!
//! Rules:
//! - before the period, the source bootstraps the candles needed to warm up
//!   the RSI and the divergence detection (no trade during the warm-up);
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
//!   `right` is the confirmation delay; `--min-gap N`: minimum distance between
//!   the two pivots, in candles (5 at 1m, 3 at 5m, 2 from 15m up);
//!   `--rsi-low R` / `--rsi-high R`: RSI level of the first low / high
//!   (default 30 / 70; 100 / 0 for any level); `--lookahead N`: pivots in which
//!   the diverging one is searched (default 3);
//! - RSI exit (`--exit-rsi-high R` / `--exit-rsi-low R`, default 75 / 30): a
//!   long closes at the first high pivot after entry with RSI above R, a short
//!   at the first low pivot with RSI below R, at the close of the candle that
//!   confirms the pivot; the account then stays flat until the next divergence;
//! - funding rates are ignored.
//!
//! The condition is wrapped so that it also emits a "tick" event for every
//! candle: the action needs every candle to check liquidation.
//!
//! One futures account (action) per fee, all on the same condition. The replay
//! runs as fast as possible (`--tick-ms 0`, the default here).
//!
//! Run with:
//! `cargo run --example backtest_futures -- [--days N | --from D [--to D]] [--leverage L] [--stop P] [--left N] [--right N] [--min-gap N] [--rsi-low R] [--rsi-high R] [--lookahead N] [--tick-ms N] [timeframe] [fees %...]`
//! e.g. `cargo run --example backtest_futures -- --days 7 --stop 3 15m 0 0.05 0.2`

#[path = "support/futures.rs"]
mod futures;

use futures::{Futures, WithTicks, parse_args};
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::source::binance::BinanceSource;
use ilus::transformer::rsi::Rsi;
use rust_decimal::Decimal;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    println!("{}", args.header);

    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", args.timeframe).replay(args.from, args.to));
    let sol_rsi = g.transform(Rsi::new(14)?, &sol);
    let signals = g.condition("sol-divergence", WithTicks::new(args.params)?, &sol_rsi);
    for &fee in &args.fees {
        g.action(
            Futures::new(Decimal::from(10), args.leverage, args.stop, fee, ())
                .with_rsi_exit(args.exit_rsi.0, args.exit_rsi.1),
            &signals,
        );
    }
    Executor::new(g).with_tick(args.tick).run()?;
    Ok(())
}
