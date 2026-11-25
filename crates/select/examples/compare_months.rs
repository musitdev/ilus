//! Per month: the best random trader vs the best evolved agents (spec F11).
//!
//! For each calendar month, all scored with the month score (gain −
//! activity penalty, target 1 – 2 positions per day):
//! - random: N random traders (1.5 positions per day, random side and exit,
//!   stop-loss drawn in the gene's range), N = the number of genomes the
//!   "all rules" evolution evaluated that month (the same chances);
//! - divergence 15m only: evolution with the other rules' weights at 0;
//! - all rules: evolution with the seven rules.
//!
//! `cargo run --release -p select --example compare_months -- --candles year.csv
//! [--months 12] [--start 2025-11-01] [--population 32] [--generations 30]
//! [--leverage 2] [--fee 0.05]`

use agent::account::AccountParams;
use agent::candles::from_csv;
use agent::simulate::SimConfig;
use chrono::{DateTime, Months, NaiveDate, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use select::EvolveConfig;
use select::month::{best_random, evolve_month, positions};
use select::score::Activity;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut candles_file = None::<String>;
    let mut months = 12u32;
    let mut start = NaiveDate::from_ymd_opt(2025, 11, 1).ok_or("date")?;
    let mut evolve_cfg = EvolveConfig {
        population: 32,
        generations: 30,
        elite: 4,
        patience: 0,
        ..EvolveConfig::default()
    };
    let mut account = AccountParams {
        capital: dec!(1000),
        leverage: dec!(2),
        stop_pct: None,
        fee_pct: dec!(0.05),
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let v = args.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--candles" => candles_file = Some(v),
            "--months" => months = v.parse()?,
            "--start" => start = NaiveDate::parse_from_str(&v, "%Y-%m-%d")?,
            "--population" => evolve_cfg.population = v.parse()?,
            "--generations" => evolve_cfg.generations = v.parse()?,
            "--leverage" => account.leverage = v.parse()?,
            "--fee" => account.fee_pct = v.parse()?,
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    let candles = from_csv(&std::fs::read_to_string(
        candles_file.ok_or("--candles file.csv needed (see evolve --save)")?,
    )?)?;
    let start: DateTime<Utc> = start.and_hms_opt(0, 0, 0).ok_or("date")?.and_utc();
    let activity = Activity::default();
    // Weights of every rule but the 15m divergence (genes 2..14): 0 for
    // "divergence only".
    let div_only: Vec<usize> = (2..14).collect();

    println!(
        "Per month: best of N random traders vs evolution ({} agents x {} generations), {}x, fee {}%",
        evolve_cfg.population,
        evolve_cfg.generations,
        account.leverage.normalize(),
        account.fee_pct.normalize()
    );
    println!(
        "score = gain - 20 x (1 - activity), activity 1 at 1-2 positions/day; gain % (positions)\n"
    );
    println!(
        "{:<8} {:>7} | {:>26} | {:>22} | {:>22}",
        "month", "SOL", "random: best (mean gain)", "divergence 15m only", "all rules"
    );
    let mut sums = [Decimal::ZERO; 4];
    for k in 0..months {
        let from = start.checked_add_months(Months::new(k)).ok_or("date")?;
        let to = from.checked_add_months(Months::new(1)).ok_or("date")?;
        let month: Vec<_> = candles
            .iter()
            .filter(|c| c.open_time >= from && c.open_time < to)
            .collect();
        let (first, last) = (
            month.first().ok_or("no candle")?,
            month.last().ok_or("no candle")?,
        );
        let sol = (last.close - first.open) / first.open * dec!(100);
        let cfg = SimConfig { from, to, account };
        let ecfg = EvolveConfig {
            seed: u64::from(k) + 1,
            ..evolve_cfg
        };
        let all = evolve_month(&candles, &cfg, &ecfg, &activity, &[])?;
        let div = evolve_month(&candles, &cfg, &ecfg, &activity, &div_only)?;
        let (rand, mean) = best_random(
            &candles,
            from,
            to,
            account,
            &activity,
            all.evaluations,
            1000 + u64::from(k),
        )?;
        println!(
            "{:<8} {:>6}% | {:>8} {:>+7}% ({:>2}) {:>+6}% | {:>8} {:>+7}% ({:>2}) | {:>8} {:>+7}% ({:>2})",
            from.format("%Y-%m"),
            sol.round_dp(1),
            rand.0.round_dp(1),
            rand.1.round_dp(1),
            rand.2,
            mean.round_dp(1),
            div.score.round_dp(1),
            div.result.gain_pct().round_dp(1),
            positions(&div.result.account),
            all.score.round_dp(1),
            all.result.gain_pct().round_dp(1),
            positions(&all.result.account)
        );
        sums[0] += rand.0;
        sums[1] += mean;
        sums[2] += div.score;
        sums[3] += all.score;
    }
    let m = Decimal::from(months.max(1));
    println!(
        "\nmean score: random best {}, random mean gain {}%, divergence only {}, all rules {}",
        (sums[0] / m).round_dp(1),
        (sums[1] / m).round_dp(1),
        (sums[2] / m).round_dp(1),
        (sums[3] / m).round_dp(1)
    );
    Ok(())
}
