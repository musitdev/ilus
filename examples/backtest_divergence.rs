//! Simulation of the RSI divergence strategy on Binance SOL/USDT history,
//! from `days` days ago (00:00 UTC, default 1 = yesterday) until now.
//!
//! ```text
//! BinanceSource(SOLUSDT, timeframe, yesterday..now) ──> Rsi(14) ──> RsiDivergence ──> Simulator
//! ```
//!
//! Rules:
//! - start with 10 SOL;
//! - a fee (in %, default 0) is taken on each transaction, from what it produces;
//! - bearish divergence (price maximum): sell all SOL;
//! - bullish divergence (price minimum): buy SOL with all USDT;
//! - the price is the close of the candle that confirms the divergence.
//!
//! At each sell the simulator records how many SOL it had; when the stream
//! ends it prints its log and a summary (the SOL held at the last bearish
//! divergence).
//!
//! Several fees can be given: one simulation runs per fee, on the same time
//! window (each with its own source, since a source has one consumer).
//!
//! Run with: `cargo run --example backtest_divergence -- [--days N] [timeframe] [fees %...]`
//! e.g. `cargo run --example backtest_divergence -- --days 7 15m 0 0.2`
//! (timeframe: 1m 5m 15m 1h 4h 1d, default 1m; fees default 0).

use chrono::{DateTime, Utc};
use ilus::action::{Action, EventOf};
use ilus::bot::Bot;
use ilus::condition::divergence::{DivergenceKind, DivergenceParams, RsiDivergence};
use ilus::error::ActionError;
use ilus::modifier::{ModifierSender, NoModifier};
use ilus::rule::Rule;
use ilus::source::binance::BinanceSource;
use ilus::source::{Source, Timeframe};
use ilus::transformer::Transformed;
use ilus::transformer::rsi::Rsi;
use rust_decimal::Decimal;
use std::fmt::Write;
use std::str::FromStr;

/// Simulated wallet: all in SOL or all in USDT.
struct Simulator {
    /// Fee rate per transaction (0.002 = 0.2 %).
    fee: Decimal,
    /// Total fees paid, in USDT.
    fees_paid: Decimal,
    /// Output, printed at once when the simulation ends.
    log: String,
    start_sol: Decimal,
    sol: Decimal,
    usdt: Decimal,
    trades: u32,
    skipped: u32,
    /// Time, price and SOL amount of the last sell (last bearish divergence).
    last_sell: Option<(DateTime<Utc>, Decimal, Decimal)>,
}

impl Simulator {
    fn new(sol: Decimal, fee_pct: Decimal) -> Self {
        Self {
            fee: fee_pct / Decimal::ONE_HUNDRED,
            fees_paid: Decimal::ZERO,
            log: format!("##### Fee {}% per transaction\n", fee_pct.normalize()),
            start_sol: sol,
            sol,
            usdt: Decimal::ZERO,
            trades: 0,
            skipped: 0,
            last_sell: None,
        }
    }

    fn pct(&self, sol: Decimal) -> Decimal {
        ((sol - self.start_sol) / self.start_sol * Decimal::ONE_HUNDRED).round_dp(2)
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

impl Action<RsiDivergence> for Simulator {
    async fn run(
        &mut self,
        event: EventOf<RsiDivergence>,
        _modifiers: &ModifierSender<NoModifier>,
    ) -> Result<(), ActionError> {
        let candle = &event.data.data; // candle that confirmed the divergence
        let price = candle.close;
        let when = fmt_time(candle.open_time);

        match event.value.kind {
            DivergenceKind::Bearish if self.sol > Decimal::ZERO => {
                let sold = self.sol;
                let gross = sold * price;
                let fee = gross * self.fee;
                self.fees_paid += fee;
                self.usdt = gross - fee;
                self.sol = Decimal::ZERO;
                self.trades += 1;
                self.last_sell = Some((candle.open_time, price, sold));
                let _ = writeln!(
                    self.log,
                    "{when}  SELL {:>10} SOL @ {:<8} -> {:>10} USDT | had {} SOL ({:+}%)",
                    sold.round_dp(4),
                    price.normalize(),
                    self.usdt.round_dp(2),
                    sold.round_dp(4),
                    self.pct(sold)
                );
            }
            DivergenceKind::Bullish if self.usdt > Decimal::ZERO => {
                let fee = self.usdt * self.fee;
                self.fees_paid += fee;
                self.sol = (self.usdt - fee) / price;
                self.trades += 1;
                let _ = writeln!(
                    self.log,
                    "{when}  BUY  {:>10} SOL @ {:<8} <- {:>10} USDT",
                    self.sol.round_dp(4),
                    price.normalize(),
                    self.usdt.round_dp(2)
                );
                self.usdt = Decimal::ZERO;
            }
            kind => {
                self.skipped += 1;
                let holding = if self.sol > Decimal::ZERO {
                    "SOL"
                } else {
                    "USDT"
                };
                let _ = writeln!(
                    self.log,
                    "{when}  skip {kind:?} divergence @ {} (already in {holding})",
                    price.normalize()
                );
            }
        }
        Ok(())
    }
}

/// The action is dropped when its rule ends (end of the stream): print the
/// log and the summary in one block.
impl Drop for Simulator {
    fn drop(&mut self) {
        let mut out = std::mem::take(&mut self.log);
        let _ = writeln!(
            out,
            "=== Summary: {} trades, {} skipped signals, fees paid {} USDT",
            self.trades,
            self.skipped,
            self.fees_paid.round_dp(2)
        );
        let _ = match self.last_sell {
            Some((time, price, sol)) => writeln!(
                out,
                "At the last bearish divergence ({}, price {}): {} SOL (start {} SOL, {:+}%)",
                fmt_time(time),
                price.normalize(),
                sol.round_dp(4),
                self.start_sol,
                self.pct(sol)
            ),
            None => writeln!(
                out,
                "No bearish divergence: still {} SOL",
                self.sol.round_dp(4)
            ),
        };
        if self.sol > Decimal::ZERO && self.last_sell.is_some() {
            let _ = writeln!(
                out,
                "(bought back afterwards: wallet ends with {} SOL, {:+}%)",
                self.sol.round_dp(4),
                self.pct(self.sol)
            );
        }
        println!("{out}");
    }
}

/// Parse a timeframe argument (`1m`, `5m`, `15m`, `1h`, `4h`, `1d`).
fn timeframe(arg: &str) -> Option<Timeframe> {
    [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
    ]
    .into_iter()
    .find(|tf| tf.to_string() == arg)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Arguments: optional `--days N`, optional timeframe, then fees in %.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut days: i64 = 1;
    if args.first().is_some_and(|a| a == "--days") {
        let n = args.get(1).ok_or("--days needs a value")?;
        days = n
            .parse()
            .map_err(|e| format!("invalid --days '{n}': {e}"))?;
        if days < 1 {
            return Err("--days must be >= 1".into());
        }
        args.drain(..2);
    }
    let now = Utc::now();
    let from = (now.date_naive() - chrono::Days::new(days.unsigned_abs()))
        .and_hms_opt(0, 0, 0)
        .ok_or("cannot compute start date")?
        .and_utc();
    let timeframe = match args.first().and_then(|a| timeframe(a)) {
        Some(tf) => {
            args.remove(0);
            tf
        }
        None => Timeframe::M1,
    };
    let mut fees = args
        .iter()
        .map(|a| Decimal::from_str(a).map_err(|e| format!("invalid fee '{a}': {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    if fees.is_empty() {
        fees.push(Decimal::ZERO);
    }
    println!(
        "Simulation SOL/USDT {timeframe} from {} to {} (UTC)\n",
        fmt_time(from),
        fmt_time(now)
    );

    let mut bot = Bot::new();
    for fee in fees {
        // Same time window for every simulation.
        let mut sol = BinanceSource::new("SOLUSDT", timeframe)
            .with_start(from)
            .with_end(now);
        let mut sol_rsi = Transformed::new(Rsi::new(14)?, sol.get_source_stream()?);
        bot.rule(Rule::new(
            format!("sol-divergence-fee-{fee}"),
            sol_rsi.get_source_stream()?,
            RsiDivergence::new(DivergenceParams::for_timeframe(timeframe))?,
            Simulator::new(Decimal::from(10), fee),
        ));
        bot.source(sol);
        bot.source(sol_rsi);
    }

    bot.run().await;
    Ok(())
}
