//! The EMA buy / sell rules on Binance SOL/USDT history, all at once.
//!
//! ```text
//!               ┌──> Ema(20) ───────────────────────> PriceCrossEma ──> Print "1 price x EMA20"
//!               ├──> Combine(Ema 9, Ema 21) ────────> EmaCross ───────> Print "2 EMA9 x EMA21"
//! BinanceSource ┼──> Combine(Ema 20, Ema 50) ───────> EmaPullback ────> Print "4 pullback EMA20/50"
//!   (replay)    ├──> Combine(Ema 8, Ema 13, Ema 21) > EmaRibbon ──────> Print "5 ribbon 8/13/21"
//!               └──> Macd(12, 26, 9) ───────────────> MacdCross ──────> Print "6 MACD x signal"
//! ```
//!
//! One source feeds every rule (fan-out); the bootstrap warms up the longest
//! one (EMA 50). Prints each signal (▲ buy, ▼ sell), then a count per rule.
//!
//! Run with: `cargo run --example ema_signals -- [days] [timeframe]`
//! e.g. `cargo run --example ema_signals -- 2 15m` (defaults: 1 day, 15m).

use std::marker::PhantomData;
use std::time::Duration;

use chrono::Utc;
use trading::action::{Action, ActionContext};
use trading::condition::Signal;
use trading::condition::ema_rules::{
    CrossSignal, EmaCross, EmaPullback, EmaRibbon, MacdCross, PriceCrossEma, PullbackSignal,
    RibbonSignal,
};
use trading::error::ActionError;
use trading::executor::Executor;
use trading::graph::Graph;
use trading::source::binance::BinanceSource;
use trading::source::{SourceData, Timeframe};
use trading::transformer::combine::Combine;
use trading::transformer::ema::Ema;
use trading::transformer::macd::Macd;
use trading::trigger::{TriggerEvent, TriggerValue};

/// A signal value that can be printed.
trait Describe: TriggerValue {
    fn signal(&self) -> Signal;
    fn details(&self) -> String;
}

impl Describe for CrossSignal {
    fn signal(&self) -> Signal {
        self.signal
    }
    fn details(&self) -> String {
        format!(
            "{} vs {}",
            self.value.round_dp(4),
            self.reference.round_dp(4)
        )
    }
}

impl Describe for PullbackSignal {
    fn signal(&self) -> Signal {
        self.signal
    }
    fn details(&self) -> String {
        format!(
            "close {} short {} trend {}",
            self.close.normalize(),
            self.short.round_dp(3),
            self.trend.round_dp(3)
        )
    }
}

impl Describe for RibbonSignal {
    fn signal(&self) -> Signal {
        self.signal
    }
    fn details(&self) -> String {
        let [a, b, c] = self.emas.map(|e| e.round_dp(3));
        format!("{a} / {b} / {c}")
    }
}

/// Prints each signal of one rule and counts them.
struct Print<V, D> {
    rule: &'static str,
    buys: usize,
    sells: usize,
    types: PhantomData<fn(V, D)>,
}

fn print<V, D>(rule: &'static str) -> Print<V, D> {
    Print {
        rule,
        buys: 0,
        sells: 0,
        types: PhantomData,
    }
}

impl<V: Describe, D: SourceData> Action for Print<V, D> {
    type Value = V;
    type Input = D;

    fn run(
        &mut self,
        event: TriggerEvent<'_, V, D>,
        _ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let arrow = match event.value.signal() {
            Signal::Bullish => {
                self.buys += 1;
                "▲ BUY "
            }
            Signal::Bearish => {
                self.sells += 1;
                "▼ SELL"
            }
        };
        println!(
            "{}  {arrow}  {:<22} {}",
            event.time.format("%m-%d %H:%M"),
            self.rule,
            event.value.details()
        );
        Ok(())
    }
}

impl<V, D> Drop for Print<V, D> {
    fn drop(&mut self) {
        println!(
            "=== {:<22} {} buy, {} sell",
            self.rule, self.buys, self.sells
        );
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
        "EMA rules on SOL/USDT {timeframe}, {} -> {} (UTC)\n",
        from.format("%Y-%m-%d %H:%M"),
        to.format("%Y-%m-%d %H:%M")
    );

    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", timeframe).replay(from, to));

    // Rule 1: close crosses EMA 20.
    let ema20 = g.transform(Ema::new(20)?, &sol);
    let r1 = g.condition("price-x-ema20", PriceCrossEma::default(), &ema20);
    g.action(print("1 price x EMA20"), &r1);

    // Rule 2: EMA 9 crosses EMA 21.
    let ema9_21 = g.transform(Combine::new((Ema::new(9)?, Ema::new(21)?)), &sol);
    let r2 = g.condition("ema9-x-ema21", EmaCross::default(), &ema9_21);
    g.action(print("2 EMA9 x EMA21"), &r2);

    // Rule 4: pullback to EMA 20 in an EMA 50 trend.
    let ema20_50 = g.transform(Combine::new((Ema::new(20)?, Ema::new(50)?)), &sol);
    let r4 = g.condition("pullback-20-50", EmaPullback::default(), &ema20_50);
    g.action(print("4 pullback EMA20/50"), &r4);

    // Rule 5: ribbon 8 / 13 / 21.
    let ribbon = g.transform(
        Combine::new((Ema::new(8)?, Ema::new(13)?, Ema::new(21)?)),
        &sol,
    );
    let r5 = g.condition("ribbon-8-13-21", EmaRibbon::default(), &ribbon);
    g.action(print("5 ribbon 8/13/21"), &r5);

    // Rule 6: MACD 12/26/9 crosses its signal line.
    let macd = g.transform(Macd::default(), &sol);
    let r6 = g.condition("macd-x-signal", MacdCross::default(), &macd);
    g.action(print("6 MACD x signal"), &r6);

    Executor::new(g).with_tick(Duration::ZERO).run()?;
    Ok(())
}
