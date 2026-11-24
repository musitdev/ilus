//! The 13 candlestick patterns on Binance SOL/USDT history.
//!
//! ```text
//! BinanceSource(SOLUSDT, tf, replay) ──> PatternCondition ──> PrintPatterns
//! ```
//!
//! Prints every candle on which at least one pattern matches (several
//! patterns can match the same candle), then a count per pattern.
//!
//! Run with: `cargo run --example candle_patterns -- [days] [timeframe]`
//! e.g. `cargo run --example candle_patterns -- 3 15m` (defaults: 1 day, 15m).

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use ilus::action::{Action, ActionContext};
use ilus::candle::Candle;
use ilus::condition::patterns::{Pattern, PatternCondition, PatternMatches, Signal};
use ilus::error::ActionError;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::source::Timeframe;
use ilus::source::binance::BinanceSource;
use ilus::trigger::TriggerEvent;

/// Prints each detection and counts the patterns.
#[derive(Default)]
struct PrintPatterns {
    counts: BTreeMap<Pattern, usize>,
    candles: usize,
}

impl Action for PrintPatterns {
    type Value = PatternMatches;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, PatternMatches, Candle>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let c = event.data;
        let names: Vec<String> = event
            .value
            .0
            .iter()
            .map(|m| {
                *self.counts.entry(m.pattern).or_default() += 1;
                let arrow = match m.signal {
                    Signal::Bullish => "▲",
                    Signal::Bearish => "▼",
                };
                format!("{arrow} {:?}", m.pattern)
            })
            .collect();
        self.candles += 1;
        println!(
            "{}  O={:<8} H={:<8} L={:<8} C={:<8} {}",
            c.open_time.format("%m-%d %H:%M"),
            c.open.normalize(),
            c.high.normalize(),
            c.low.normalize(),
            c.close.normalize(),
            names.join(", ")
        );
        Ok(())
    }
}

/// Summary when the replay ends.
impl Drop for PrintPatterns {
    fn drop(&mut self) {
        println!("\n=== {} candles with at least one pattern", self.candles);
        for (pattern, n) in &self.counts {
            println!("  {pattern:?}: {n}");
        }
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
    println!(
        "Candlestick patterns on SOL/USDT {timeframe}, {} -> {} (UTC)\n",
        from.format("%Y-%m-%d %H:%M"),
        to.format("%Y-%m-%d %H:%M")
    );

    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", timeframe).replay(from, to));
    let patterns = g.condition("patterns", PatternCondition::default(), &sol);
    g.action(PrintPatterns::default(), &patterns);

    Executor::new(g).with_tick(Duration::ZERO).run()?;
    Ok(())
}
