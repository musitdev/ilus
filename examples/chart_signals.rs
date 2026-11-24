//! Every curve and signal on one chart: candles, EMA 9 / EMA 21, RSI,
//! EMA buy / sell rules, RSI divergences and candlestick patterns.
//!
//! ```text
//!                 ┌──> Combine(Ema 9, Ema 21, Rsi 14) ──> Chart (curves, RSI panel)
//!                 ├──> Ema 9 ──> PriceCrossEma ─────────> ▲▼ "Px EMA9"
//! BinanceSource ──┼──> Combine(Ema 9, Ema 21) ─┬─> EmaCross ────> ▲▼ "EMA 9x21"
//!   (replay)      │                            └─> EmaPullback ─> ▲▼ "Pullback"
//!                 ├──> Rsi 14 ──> RsiDivergence ────────> segments on price and RSI
//!                 └──> PatternCondition ────────────────> small ▲▼ with the patterns
//! ```
//!
//! On the chart:
//! - curves: EMA 9 (yellow), EMA 21 (purple); RSI in the bottom panel;
//! - EMA rules: big green ▲ buy / red ▼ sell at the close, labelled with the rule;
//! - divergences: a segment between the two pivots on the price **and** on
//!   the RSI (they go in opposite directions), blue = bullish, pink = bearish;
//!   the two pivots A and B are marked on the RSI curve with their value;
//! - candlestick patterns: small blue ▲ under the low / pink ▼ above the high.
//!
//! Run with (feature `chart`):
//! `cargo run --features chart --example chart_signals -- [days] [timeframe]`
//! e.g. `cargo run --features chart --example chart_signals -- 2 15m`
//! (defaults: 2 days, 15m).

use std::marker::PhantomData;
use std::time::Duration;

use chrono::Utc;
use ilus::action::{Action, ActionContext};
use ilus::candle::Candle;
use ilus::condition::Signal;
use ilus::condition::divergence::{Divergence, DivergenceKind, DivergenceParams, RsiDivergence};
use ilus::condition::ema_rules::{
    CrossSignal, EmaCross, EmaPullback, PriceCrossEma, PullbackSignal,
};
use ilus::condition::patterns::{Pattern, PatternCondition, PatternMatches};
use ilus::error::ActionError;
use ilus::executor::Executor;
use ilus::graph::Graph;
use ilus::modifier::ModifierHandle;
use ilus::source::binance::BinanceSource;
use ilus::source::{SourceData, Timeframe};
use ilus::transformer::Derived;
use ilus::transformer::chart::{
    ChartFrame, ChartModifier, ChartPanel, Marker, MarkerKind, PanelPoint, Segment, chart_with,
};
use ilus::transformer::combine::Combine;
use ilus::transformer::ema::Ema;
use ilus::transformer::rsi::{Rsi, RsiCandle};
use ilus::trigger::{TriggerEvent, TriggerValue};
use rust_decimal::Decimal;

/// Item drawn by the chart: a candle with EMA 9, EMA 21 and RSI 14.
type View = Derived<Candle, (Option<Decimal>, Option<Decimal>, Option<Decimal>)>;

fn view_frame(d: &View) -> ChartFrame<'_> {
    let (ema9, ema21, rsi) = d.value.unwrap_or((None, None, None));
    ChartFrame {
        candle: &d.data,
        overlays: vec![ema9, ema21],
        indicator: rsi,
    }
}

type Markers = ModifierHandle<ChartModifier>;

// ------------------------------------------------------------- EMA rules

/// A rule value with a buy / sell signal.
trait HasSignal: TriggerValue {
    fn signal(&self) -> Signal;
}
impl HasSignal for CrossSignal {
    fn signal(&self) -> Signal {
        self.signal
    }
}
impl HasSignal for PullbackSignal {
    fn signal(&self) -> Signal {
        self.signal
    }
}

/// Input items carrying a candle.
trait HasCandle: SourceData {
    fn candle(&self) -> &Candle;
}
impl<V: std::fmt::Debug + Clone + Send + Sync + 'static> HasCandle for Derived<Candle, V> {
    fn candle(&self) -> &Candle {
        &self.data
    }
}

/// Draws a buy / sell marker at the close, labelled with the rule.
struct MarkRule<V, D> {
    rule: &'static str,
    chart: Markers,
    types: PhantomData<fn(V, D)>,
}

fn mark_rule<V, D>(rule: &'static str, chart: Markers) -> MarkRule<V, D> {
    MarkRule {
        rule,
        chart,
        types: PhantomData,
    }
}

impl<V: HasSignal, D: HasCandle> Action for MarkRule<V, D> {
    type Value = V;
    type Input = D;

    fn run(
        &mut self,
        event: TriggerEvent<'_, V, D>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let c = event.data.candle();
        let kind = match event.value.signal() {
            Signal::Bullish => MarkerKind::Buy,
            Signal::Bearish => MarkerKind::Sell,
        };
        ctx.send(
            &self.chart,
            ChartModifier::Marker(Marker {
                time: c.open_time,
                price: c.close,
                kind,
                label: self.rule.into(),
            }),
        );
        Ok(())
    }
}

// ------------------------------------------------------------ divergences

/// Draws a divergence: a segment between its pivots on the price and on the RSI.
struct MarkDivergence(Markers);

impl Action for MarkDivergence {
    type Value = Divergence;
    type Input = RsiCandle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, Divergence, RsiCandle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let d = event.value;
        let (kind, label) = match d.kind {
            DivergenceKind::Bullish => (MarkerKind::Bullish, "bull div"),
            DivergenceKind::Bearish => (MarkerKind::Bearish, "bear div"),
        };
        // A and B on the RSI curve, with their RSI value.
        for (name, pivot) in [("A", &d.first), ("B", &d.second)] {
            ctx.send(
                &self.0,
                ChartModifier::Point(PanelPoint {
                    panel: ChartPanel::Indicator,
                    time: pivot.time,
                    value: pivot.rsi,
                    kind,
                    label: format!("{name} {}", pivot.rsi.round_dp(2)),
                }),
            );
        }
        for (panel, from, to) in [
            (ChartPanel::Price, d.first.price, d.second.price),
            (ChartPanel::Indicator, d.first.rsi, d.second.rsi),
        ] {
            ctx.send(
                &self.0,
                ChartModifier::Segment(Segment {
                    panel,
                    from: (d.first.time, from),
                    to: (d.second.time, to),
                    kind,
                    label: label.into(),
                }),
            );
        }
        Ok(())
    }
}

// --------------------------------------------------------------- patterns

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

/// Draws the patterns: under the low (bullish) / above the high (bearish).
struct MarkPatterns(Markers);

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
            if !names.is_empty() {
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
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- main

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let days: u64 = match args.next() {
        Some(d) => d.parse().map_err(|e| format!("invalid days '{d}': {e}"))?,
        None => 2,
    };
    let timeframe = match args.next() {
        Some(tf) => Timeframe::parse(&tf).ok_or(format!("invalid timeframe '{tf}'"))?,
        None => Timeframe::M15,
    };
    let to = Utc::now();
    let from = to - chrono::Days::new(days);
    let title = format!(
        "SOL/USDT {timeframe}: EMA 9 / 21, RSI 14, EMA rules, divergences, patterns — {} -> {} (UTC)",
        from.format("%m-%d %H:%M"),
        to.format("%m-%d %H:%M")
    );
    let (chart_t, window) = chart_with::<View>(title, &["EMA 9", "EMA 21"], "RSI(14)", view_frame);

    let mut g = Graph::new();
    let sol = g.source(BinanceSource::new("SOLUSDT", timeframe).replay(from, to));

    // Chart: candles + EMA 9 + EMA 21 + RSI 14.
    let view = g.transform(
        Combine::new((Ema::new(9)?, Ema::new(21)?, Rsi::new(14)?)),
        &sol,
    );
    let chart = g.transform(chart_t, &view);
    let markers = g.modifier_handle(&chart);

    // EMA rule 1: close crosses EMA 9.
    let ema9 = g.transform(Ema::new(9)?, &sol);
    let r1 = g.condition("price-x-ema9", PriceCrossEma::default(), &ema9);
    g.action(mark_rule("Px EMA9", markers), &r1);

    // EMA rules 2 and 4 on EMA 9 / EMA 21.
    let ema9_21 = g.transform(Combine::new((Ema::new(9)?, Ema::new(21)?)), &sol);
    let r2 = g.condition("ema9-x-ema21", EmaCross::default(), &ema9_21);
    g.action(mark_rule("EMA 9x21", markers), &r2);
    let r4 = g.condition("pullback-9-21", EmaPullback::default(), &ema9_21);
    g.action(mark_rule("Pullback", markers), &r4);

    // RSI divergences.
    let rsi = g.transform(Rsi::new(14)?, &sol);
    let div = g.condition(
        "rsi-divergence",
        RsiDivergence::new(DivergenceParams::for_timeframe(timeframe))?,
        &rsi,
    );
    g.action(MarkDivergence(markers), &div);

    // Candlestick patterns.
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
