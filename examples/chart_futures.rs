//! Futures simulation with a chart window: candles, RSI and every trade.
//!
//! ```text
//!                         ┌──> Chart <───────── ChartModifier::Marker (trades) ─┐
//! BinanceSource ──> Rsi(14) ──> WithTicks(RsiDivergence) ──> Futures ───────────┘
//! ```
//!
//! Same options and rules as `backtest_futures` (one fee: the first one given).
//! Markers: green ▲ long, red ▼ short, grey ● close, orange ✕ stop-loss,
//! purple ✱ liquidation. The RSI is shown in the bottom panel (30 / 70 lines).
//!
//! The window must run on the main thread: the executor runs in a background thread.
//! Close the window to quit.
//!
//! Run with (feature `chart`):
//! `cargo run --features chart --example chart_futures -- --from 2026-09-22 --to 2026-09-29 --stop 3 --right 1 15m 0.05`

#[path = "support/futures.rs"]
mod futures;

use futures::{Futures, Side, Trade, TradeKind, TradeRecorder, WithTicks, fmt_time, parse_args};
use ilus::action::ActionContext;
use ilus::condition::divergence::RsiDivergence;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::ModifierHandle;
use ilus::source::binance::BinanceSource;
use ilus::transformer::chart::{ChartModifier, Marker, MarkerKind, chart};
use ilus::transformer::rsi::{Rsi, RsiCandle};
use rust_decimal::Decimal;

/// Sends every trade of the simulation to the chart, as a modifier.
struct ChartRecorder(ModifierHandle<ChartModifier>);

impl TradeRecorder for ChartRecorder {
    fn record(&mut self, ctx: &mut ActionContext, trade: Trade) {
        let kind = match trade.kind {
            TradeKind::Open(Side::Long) => MarkerKind::Buy,
            TradeKind::Open(Side::Short) => MarkerKind::Sell,
            TradeKind::Close => MarkerKind::Close,
            TradeKind::Stop => MarkerKind::Stop,
            TradeKind::Liquidation => MarkerKind::Liquidation,
        };
        ctx.send(
            &self.0,
            ChartModifier::Marker(Marker {
                time: trade.time,
                price: trade.price,
                kind,
                label: trade.label,
            }),
        );
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

    // Graph: the chart is a side branch of the RSI output (fan-out).
    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", args.timeframe).replay(args.from, args.to));
    let sol_rsi = g.transform(Rsi::new(14)?, &sol);
    let sol_chart = g.transform(chart_transformer, &sol_rsi);
    let markers = g.modifier_handle(&sol_chart);
    let signals = g.condition(
        "sol-divergence",
        WithTicks(RsiDivergence::new(args.params)?),
        &sol_rsi,
    );
    g.action(
        Futures::new(
            Decimal::from(10),
            args.leverage,
            args.stop,
            fee,
            ChartRecorder(markers),
        ),
        &signals,
    );

    // The executor runs in the background; the window owns the main thread.
    let executor = Executor::new(g).with_tick(args.tick);
    std::thread::spawn(move || match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(executor.run()),
        Err(e) => eprintln!("cannot start the runtime: {e}"),
    });
    window.run()?;
    Ok(())
}
