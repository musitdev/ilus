//! Test agent (spec F9): five rules vote, a decision condition trades.
//!
//! ```text
//! SOLUSDT 15m ─┬─> Decision(open 3, exit 2) ──> Account (futures sim) ──┐ Closed
//!              │      ^ Vote modifiers                                   │
//!              ├─> Rsi(14) ─> DivergenceVoter 15m ─> SendVotes ──────────┤
//!              ├─> Aggregate 1h ─> OnValue(Rsi 14) ─> Projected(DivergenceVoter 1h) ─> SendVotes
//!              ├─> Combine(Ema 9, Ema 21) ─> EmaCrossVoter ─> SendVotes
//!              ├─> Trend(EMA 50) ─> TrendVoter ─> SendVotes
//!              └─> PatternVoter (13 patterns) ─> SendVotes
//! ```
//!
//! Fixed genes (weights open / exit): divergence 15m 2 / 1, divergence 1h
//! 3 / 1, EMA 9/21 cross 1 / 1, trend EMA 50 1 / 1, patterns 1 / 0.5;
//! thresholds open 3, exit 2. Divergences: the `--rsi-low` / `--rsi-high`,
//! `--lookahead`, `--left` / `--right` options (both timeframes); RSI exits
//! `--exit-rsi-high` / `--exit-rsi-low`. Account: 10 SOL, `--leverage`,
//! `--stop` (safety net), one simulation per fee. Always 15m.
//!
//! e.g. `cargo run --release --example backtest_agent -- --days 60 --leverage 2 --stop 3 15m 0.05`

#[path = "../../trading/examples/support/futures.rs"]
mod futures;

use agent::decision::{Decision, DecisionEvent, DecisionKind, DecisionParams};
use agent::voters::{DivergenceVoter, EmaCrossVoter, PatternVoter, TrendVoter};
use agent::{DecisionModifier, SendVotes, Side, Weights};
use futures::{Futures, Side as AccountSide, fmt_time, parse_args};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::divergence::DivergenceParams;
use trading::condition::patterns::{PatternCondition, ThresholdSet};
use trading::condition::projected::{Projected, aggregated};
use trading::error::ActionError;
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::ModifierHandle;
use trading::source::Timeframe;
use trading::source::binance::BinanceSource;
use trading::transformer::aggregate::Aggregate;
use trading::transformer::combine::{Combine, OnValue};
use trading::transformer::ema::Ema;
use trading::transformer::rsi::Rsi;
use trading::transformer::trend::Trend;
use trading::trigger::TriggerEvent;

/// Bootstrap candles: covers the 1h chain (RSI 14 + pivots, in 1h candles).
const BOOTSTRAP: usize = 4 * (15 + 10) + 4;

/// The account: trades the decisions, tells the decision when the
/// stop-loss or a liquidation closed the position.
struct Account {
    futures: Futures<(), Candle>,
    decision: ModifierHandle<DecisionModifier>,
}

impl Action for Account {
    type Value = DecisionEvent;
    type Input = Candle;

    fn run(
        &mut self,
        event: TriggerEvent<'_, DecisionEvent, Candle>,
        ctx: &mut ActionContext,
    ) -> Result<(), ActionError> {
        let c = event.data;
        let e = event.value;
        match e.kind {
            DecisionKind::Tick => {
                if self.futures.agent_candle(c) {
                    ctx.send(&self.decision, DecisionModifier::Closed);
                }
            }
            DecisionKind::Open(side) => {
                self.futures.agent_log(&format!(
                    "{}  decision: open {side:?} (long {}, short {})",
                    fmt_time(c.open_time),
                    e.long,
                    e.short
                ));
                let side = match side {
                    Side::Long => AccountSide::Long,
                    Side::Short => AccountSide::Short,
                };
                self.futures.agent_open(side, c);
            }
            DecisionKind::Exit(side) => {
                self.futures.agent_log(&format!(
                    "{}  decision: exit {side:?} (exit score {})",
                    fmt_time(c.open_time),
                    e.exit
                ));
                self.futures.agent_exit(c);
            }
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let m15 = Timeframe::M15;
    let p15 = DivergenceParams {
        min_gap: DivergenceParams::for_timeframe(m15).min_gap,
        ..args.params
    };
    let p1h = DivergenceParams {
        min_gap: DivergenceParams::for_timeframe(Timeframe::H1).min_gap,
        ..args.params
    };
    let (exit_high, exit_low) = args.exit_rsi;
    let w = |open, exit| Weights { open, exit };
    println!(
        "Test agent SOL/USDT 15m, {}x, stop-loss {}, divergences RSI {} / {}, lookahead {}, RSI exit {} / {}, from {} to {} (UTC)\n",
        args.leverage.normalize(),
        args.stop
            .map_or("none".to_string(), |p| format!("{}%", p.normalize())),
        args.params.rsi_low,
        args.params.rsi_high,
        args.params.lookahead,
        exit_high,
        exit_low,
        fmt_time(args.from),
        fmt_time(args.to)
    );

    let mut g = Graph::new();
    let sol = g.source(
        BinanceSource::new("SOLUSDT", m15)
            .replay(args.from, args.to)
            .with_bootstrap(BOOTSTRAP),
    );

    // Decision and accounts (first: they see each candle before the votes).
    let decision = g.condition(
        "decision",
        Decision::<Candle>::new(DecisionParams {
            open_threshold: dec!(3),
            exit_threshold: dec!(2),
        })?,
        &sol,
    );
    let to = g.modifier_handle(&decision);
    for &fee in &args.fees {
        let futures = Futures::new(Decimal::from(10), args.leverage, args.stop, fee, ());
        g.action(
            Account {
                futures,
                decision: to,
            },
            &decision,
        );
    }

    // Rule 1: 15m divergence.
    let rsi = g.transform(Rsi::new(14)?, &sol);
    let v = g.condition(
        "div-15m",
        DivergenceVoter::new(p15, exit_high, exit_low)?,
        &rsi,
    );
    g.action(SendVotes::new("div-15m", w(dec!(2), dec!(1)), to), &v);

    // Rule 2: 1h divergence, on the 1h candles aggregated from the 15m ones.
    let h1 = g.transform(Aggregate::new(m15, Timeframe::H1)?, &sol);
    let h1_rsi = g.transform(OnValue::new(Rsi::new(14)?), &h1);
    let voter = Projected::new(
        DivergenceVoter::new(p1h, exit_high, exit_low)?,
        aggregated::<Decimal>,
    );
    let v = g.condition("div-1h", voter, &h1_rsi);
    g.action(SendVotes::new("div-1h", w(dec!(3), dec!(1)), to), &v);

    // Rule 3: fast / slow EMA cross.
    let emas = g.transform(Combine::new((Ema::new(9)?, Ema::new(21)?)), &sol);
    let v = g.condition("ema-9-21", EmaCrossVoter::default(), &emas);
    g.action(SendVotes::new("ema-9-21", w(dec!(1), dec!(1)), to), &v);

    // Rule 4: trend (close vs EMA 50).
    let trend = g.transform(Trend::<Candle>::new(50)?, &sol);
    let v = g.condition("trend-50", TrendVoter::default(), &trend);
    g.action(SendVotes::new("trend-50", w(dec!(1), dec!(1)), to), &v);

    // Rule 5: the 13 candlestick patterns.
    let patterns = PatternVoter::new(PatternCondition::new(ThresholdSet::default())?);
    let v = g.condition("patterns", patterns, &sol);
    g.action(SendVotes::new("patterns", w(dec!(1), dec!(0.5)), to), &v);

    Executor::new(g).with_tick(args.tick).run()?;
    Ok(())
}
