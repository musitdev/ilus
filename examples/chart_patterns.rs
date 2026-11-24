//! Candlestick patterns on a chart, to inspect the detections.
//!
//! ```text
//!                   ┌──> Chart <────────── ChartModifier::Marker (patterns) ─┐
//! BinanceSource ────┤                                                        │
//!   (replay)        └──> PatternCondition ──> MarkPatterns ──────────────────┘
//! ```
//!
//! Every candle on which patterns match gets a marker on the candle that
//! confirms them (the last candle of the pattern):
//! - blue ▲ under the low: bullish patterns;
//! - pink ▼ above the high: bearish patterns;
//! - the label lists the patterns (several can match the same candle).
//!
//! Zoom with the mouse wheel to read the labels; hover a candle for its time.
//!
//! Run with (feature `chart`):
//! `cargo run --features chart --example chart_patterns -- [days] [timeframe]`
//! e.g. `cargo run --features chart --example chart_patterns -- 2 15m`
//! (defaults: 1 day, 15m).

use std::time::Duration;

use chrono::Utc;
use ilus::action::{Action, ActionContext};
use ilus::candle::Candle;
use ilus::condition::patterns::{Pattern, PatternCondition, PatternMatches, Signal};
use ilus::error::ActionError;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::ModifierHandle;
use ilus::source::Timeframe;
use ilus::source::binance::BinanceSource;
use ilus::transformer::chart::{ChartModifier, Marker, MarkerKind, chart};
use ilus::trigger::TriggerEvent;

/// Short name of a pattern, for the labels.
fn short(p: Pattern) -> &'static str {
    match p {
        Pattern::BullishEngulfing | Pattern::BearishEngulfing => "Engulfing",
        Pattern::Hammer => "Hammer",
        Pattern::HangingMan => "HangingMan",
        Pattern::InvertedHammer => "InvHammer",
        Pattern::ShootingStar => "ShootingStar",
        Pattern::Doji => "Doji",
        Pattern::MorningStar => "MorningStar",
        Pattern::EveningStar => "EveningStar",
        Pattern::BullishHarami | Pattern::BearishHarami => "Harami",
        Pattern::PiercingLine => "Piercing",
        Pattern::DarkCloudCover => "DarkCloud",
    }
}

/// Sends one marker per signal direction to the chart.
struct MarkPatterns(ModifierHandle<ChartModifier>);

impl Action for MarkPatterns {
    type Value = PatternMatches;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, PatternMatches, Candle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let c = event.data;
        for (signal, kind, price) in [
            (Signal::Bullish, MarkerKind::Bullish, c.low),
            (Signal::Bearish, MarkerKind::Bearish, c.high),
        ] {
            let names: Vec<&str> = event
                .value
                .0
                .iter()
                .filter(|m| m.signal == signal)
                .map(|m| short(m.pattern))
                .collect();
            if names.is_empty() {
                continue;
            }
            ctx.send(
                &self.0,
                ChartModifier::Marker(Marker {
                    time: c.open_time,
                    price,
                    kind,
                    label: names.join(", "),
                }),
            );
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let days: u64 = match args.next() {
        Some(d) => d.parse().map_err(|e| format!("invalid days '{d}': {e}"))?,
        None => 1,
    };
    let timeframe = match args.next() {
        Some(tf) => Timeframe::parse(&tf).ok_or(format!("invalid timeframe '{tf}'"))?,
        None => Timeframe::M15,
    };
    let to = Utc::now();
    let from = to - chrono::Days::new(days);
    let title = format!(
        "SOL/USDT {timeframe} candlestick patterns, {} -> {} (UTC)",
        from.format("%Y-%m-%d %H:%M"),
        to.format("%Y-%m-%d %H:%M")
    );
    let (chart_transformer, window) = chart::<Candle>(title, "");

    // The chart and the patterns both read the candles (fan-out).
    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", timeframe).replay(from, to));
    let sol_chart = g.transform(chart_transformer, &sol);
    let markers = g.modifier_handle(&sol_chart);
    let patterns = g.condition("patterns", PatternCondition::default(), &sol);
    g.action(MarkPatterns(markers), &patterns);

    // The executor runs on its own thread; the window owns the main thread.
    let executor = Executor::new(g).with_tick(Duration::ZERO);
    std::thread::spawn(move || {
        if let Err(e) = executor.run() {
            eprintln!("cannot start the executor: {e}");
        }
    });
    window.run()?;
    Ok(())
}
