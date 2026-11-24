//! Test agent (spec F9, F10) on SOL/USDT 15m history from Binance.
//!
//! 1. Fetch the candles once (a small graph: Binance replay -> collect).
//! 2. Simulate the agent with [`agent::simulate`]: five rules (15m and 1h
//!    divergences, EMA cross, trend, candlestick patterns) vote, the
//!    decision trades a simulated futures account.
//!
//! Without `--genome`, hand-written parameters are used: weights open / exit
//! div15 2 / 1, div1h 3 / 1, EMA 9/21 1 / 1, trend EMA 50 1 / 1, patterns
//! 1 / 0.5; thresholds open 3, exit 2; divergences RSI 30 / 70, lookahead 3,
//! RSI exit 75 / 30.
//!
//! `cargo run --release --example backtest_agent -- [--days N] [--leverage L]
//! [--stop P] [--fee F] [--genome g1,g2,...] [--events]`
//! (defaults: 60 days, 2x, stop 3 %, fee 0.05 %).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::Weights;
use agent::account::{AccountParams, EventKind};
use agent::decision::DecisionParams;
use agent::genome::{AgentParams, DivergenceGenes, GENOME_LEN};
use agent::simulate::{SimConfig, history_needed, simulate_params};
use chrono::{DateTime, Days, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::action::{Action, ActionContext};
use trading::candle::Candle;
use trading::condition::Condition;
use trading::error::{ActionError, ModifierError};
use trading::executor::Executor;
use trading::graph::Graph;
use trading::modifier::NoModifier;
use trading::source::binance::BinanceSource;
use trading::source::{Item, Timeframe};
use trading::trigger::{Detection, TriggerEvent, TriggerEventType, TriggerValue};

/// Every candle (to collect them).
struct Every;

#[derive(Debug)]
struct Tick;
impl TriggerValue for Tick {}

impl Condition for Every {
    type Input = Candle;
    type Value = Tick;
    type Modifier = NoModifier;
    fn warm_up(&self) -> usize {
        0
    }
    fn on_data(&mut self, _: Item<'_, Candle>) -> Option<Detection<Tick>> {
        Some(Detection {
            event_type: TriggerEventType::Other("candle"),
            value: Tick,
        })
    }
    fn on_modifier(&mut self, m: NoModifier) -> Result<Option<Detection<Tick>>, ModifierError> {
        match m {}
    }
}

struct Collect(Arc<Mutex<Vec<Candle>>>);

impl Action for Collect {
    type Value = Tick;
    type Input = Candle;
    fn run(
        &mut self,
        event: TriggerEvent<'_, Tick, Candle>,
        _: &mut ActionContext,
    ) -> Result<(), ActionError> {
        if let Ok(mut v) = self.0.lock() {
            v.push(event.data.clone());
        }
        Ok(())
    }
}

/// SOL/USDT 15m candles from `from` to `to` (excluded), from Binance.
fn fetch(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Candle>, Box<dyn std::error::Error>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut g = Graph::new();
    let src = g.source(BinanceSource::new("SOLUSDT", Timeframe::M15).replay(from, to));
    let c = g.condition("every", Every, &src);
    g.action(Collect(out.clone()), &c);
    Executor::new(g).with_tick(Duration::ZERO).run()?;
    let v = out.lock().map_err(|_| "lock poisoned")?.clone();
    Ok(v)
}

/// The hand-written test agent.
fn hand_params() -> AgentParams {
    let w = |open, exit| Weights { open, exit };
    let div = DivergenceGenes {
        rsi_low: dec!(30),
        rsi_high: dec!(70),
        lookahead: 3,
        exit_rsi_high: dec!(75),
        exit_rsi_low: dec!(30),
    };
    AgentParams {
        div15: w(dec!(2), dec!(1)),
        div1h: w(dec!(3), dec!(1)),
        ema: w(dec!(1), dec!(1)),
        trend: w(dec!(1), dec!(1)),
        patterns: w(dec!(1), dec!(0.5)),
        decision: DecisionParams {
            open_threshold: dec!(3),
            exit_threshold: dec!(2),
        },
        div15_rule: div,
        div1h_rule: div,
        ema_fast: 9,
        ema_slow: 21,
        trend_period: 50,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut days: u64 = 60;
    let mut leverage = dec!(2);
    let mut stop = Some(dec!(3));
    let mut fee = dec!(0.05);
    let mut params = hand_params();
    let mut events = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--events" {
            events = true;
            continue;
        }
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--days" => days = value.parse()?,
            "--leverage" => leverage = value.parse()?,
            "--stop" => stop = Some(value.parse()?).filter(|s: &Decimal| !s.is_zero()),
            "--fee" => fee = value.parse()?,
            "--genome" => {
                let genes: Vec<f64> = value
                    .split(',')
                    .map(str::trim)
                    .map(str::parse)
                    .collect::<Result<_, _>>()?;
                if genes.len() != GENOME_LEN {
                    return Err(format!("--genome: {GENOME_LEN} genes expected").into());
                }
                params = AgentParams::decode(&genes)?;
            }
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }

    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or("date")?
        .and_utc();
    let from = today
        .checked_sub_days(Days::new(days))
        .ok_or("--days too large")?;
    // History before `from` for the warm-up (15 min per candle).
    let warm = history_needed(&params) as i64;
    let fetch_from = from - chrono::TimeDelta::minutes(15 * warm);
    let to = Utc::now();
    let candles = fetch(fetch_from, to)?;

    let cfg = SimConfig {
        from,
        to,
        account: AccountParams {
            capital: dec!(1000),
            leverage,
            stop_pct: stop,
            fee_pct: fee,
        },
    };
    let r = simulate_params(&params, &candles, &cfg)?;
    println!("Agent: {}", r.params);
    println!(
        "SOL/USDT 15m {} -> {} (UTC), {}x, stop {}, fee {}% ({} candles)",
        from.format("%Y-%m-%d %H:%M"),
        to.format("%Y-%m-%d %H:%M"),
        leverage.normalize(),
        stop.map_or("none".into(), |s| format!("{}%", s.normalize())),
        fee.normalize(),
        candles.len()
    );
    let a = &r.account;
    if events {
        for e in &a.events {
            let what = match e.kind {
                EventKind::Open(s) => format!("OPEN  {s:?}"),
                EventKind::Close(s, why) => format!("CLOSE {s:?} ({why:?})"),
            };
            println!(
                "{}  {what:<24} @ {:<10} equity {}",
                e.time.format("%Y-%m-%d %H:%M"),
                e.price.normalize(),
                e.equity.round_dp(2)
            );
        }
    }
    println!(
        "=== {} events, {} stops, {} liquidations, fees {} USDT, lowest equity {} USDT",
        a.trades,
        a.stops,
        a.liquidations,
        a.fees_paid.round_dp(2),
        a.min_equity.round_dp(2)
    );
    println!(
        "Gain (fitness): {}%  (equity {} USDT)",
        r.gain_pct().round_dp(2),
        a.final_equity.round_dp(2)
    );
    Ok(())
}
