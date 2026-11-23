//! Futures simulation with a chart window: candles, RSI and every trade.
//!
//! ```text
//! BinanceSource ──> Rsi(14) ──> Chart ──> WithTicks(RsiDivergence) ──> Futures
//!                                 ^                                      │
//!                                 └──── ChartModifier::Marker (trades) ──┘
//! ```
//!
//! Same options and rules as `backtest_futures` (one fee: the first one given).
//! Markers: green ▲ long, red ▼ short, grey ● close, orange ✕ stop-loss,
//! purple ✱ liquidation. The RSI is shown in the bottom panel (30 / 70 lines).
//!
//! The window must run on the main thread: the bot runs in a background thread.
//! Close the window to quit.
//!
//! Run with (feature `chart`):
//! `cargo run --features chart --example chart_futures -- --from 2026-09-22 --to 2026-09-29 --stop 3 --right 1 15m 0.05`

#[path = "support/futures.rs"]
mod futures;

use chrono::{DateTime, Utc};
use futures::{Futures, Side, TradeKind, TradeRecorder, WithTicks, fmt_time, parse_args};
use ilus::bot::Bot;
use ilus::condition::divergence::RsiDivergence;
use ilus::modifier::ModifierSender;
use ilus::rule::Rule;
use ilus::source::Source;
use ilus::source::binance::BinanceSource;
use ilus::transformer::chart::{ChartModifier, Marker, MarkerKind, chart};
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::transformer::{Derived, Transformed};
use rust_decimal::Decimal;

/// Sends every trade of the simulation to the chart, as a modifier.
struct ChartRecorder(ModifierSender<ChartModifier>);

impl TradeRecorder for ChartRecorder {
    fn record(&mut self, time: DateTime<Utc>, price: Decimal, kind: TradeKind, label: String) {
        let kind = match kind {
            TradeKind::Open(Side::Long) => MarkerKind::Buy,
            TradeKind::Open(Side::Short) => MarkerKind::Sell,
            TradeKind::Close => MarkerKind::Close,
            TradeKind::Stop => MarkerKind::Stop,
            TradeKind::Liquidation => MarkerKind::Liquidation,
        };
        // If the window is closed, the marker is simply dropped.
        let _ = self.0.send(ChartModifier::Marker(Marker {
            time,
            price,
            kind,
            label,
        }));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let fee = args.fees.first().copied().unwrap_or_default();
    println!("{}", args.header);

    let title = format!(
        "SOL/USDT {} {} → {} | {}x, stop {}, right {}, fee {}%",
        args.timeframe,
        fmt_time(args.from),
        fmt_time(args.to),
        args.leverage.normalize(),
        args.stop
            .map_or("none".to_string(), |p| format!("{}%", p.normalize())),
        args.params.right,
        fee.normalize()
    );
    let (chart_transformer, window) = chart::<RsiCandle>(title, "RSI(14)");

    // Pipeline: Binance -> RSI -> Chart -> condition -> futures account.
    let mut sol = BinanceSource::new("SOLUSDT", args.timeframe)
        .with_start(args.from)
        .with_end(args.to);
    let mut sol_rsi = Transformed::new(Rsi::new(14)?, sol.get_source_stream()?);
    let mut sol_chart = Transformed::new(chart_transformer, sol_rsi.get_source_stream()?);
    let recorder = ChartRecorder(sol_chart.modifier_sender());

    let mut bot = Bot::new();
    bot.rule(Rule::new(
        "futures-chart",
        sol_chart.get_source_stream()?,
        WithTicks::<Derived<RsiCandle, ()>>::new(RsiDivergence::new(args.params)?),
        Futures::new(Decimal::from(10), args.leverage, args.stop, fee, recorder),
    ));
    bot.source(sol);
    bot.source(sol_rsi);
    bot.source(sol_chart);

    // The bot runs in the background; the window owns the main thread.
    std::thread::spawn(move || match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(bot.run()),
        Err(e) => eprintln!("cannot start the runtime: {e}"),
    });
    window.run()?;
    Ok(())
}
