//! Per month: the best random trader vs the best evolved agents (spec F11).
//!
//! For each calendar month, all scored with the month score (gain −
//! activity penalty, target 1 – 2 positions per day):
//! - random: N random traders (1.5 positions per day, random side and exit,
//!   stop-loss drawn in the gene's range), N = the number of genomes the
//!   "all rules" evolution evaluated that month (the same chances);
//! - divergence 15m only: evolution with the other rules' weights at 0;
//! - all rules: evolution with the five rules.
//!
//! `cargo run --release -p select --example compare_months -- --candles year.csv
//! [--months 12] [--start 2025-11-01] [--population 32] [--generations 30]
//! [--leverage 2] [--fee 0.05]`

use agent::account::{AccountParams, AccountSummary, EventKind};
use agent::candles::from_csv;
use agent::genome::GENOME_LEN;
use agent::simulate::{SimConfig, simulate};
use chrono::{DateTime, Months, NaiveDate, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use select::baseline::RandomTrader;
use select::rng::Rng;
use select::score::{Activity, period_score};
use select::{EvolveConfig, Score, evolve};
use trading::candle::Candle;

fn positions(a: &AccountSummary) -> usize {
    a.events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Open(_)))
        .count()
}

/// Best of an evolution: (score, gain %, positions, evaluations).
fn best_evolved(
    candles: &[Candle],
    cfg: &SimConfig,
    days: f64,
    evolve_cfg: &EvolveConfig,
    mask: Option<&[usize]>,
) -> Result<(Decimal, Decimal, usize, usize), String> {
    let activity = Activity::default();
    let fit = |genome: &[f64]| -> Result<Score, String> {
        let mut g = genome.to_vec();
        for &i in mask.unwrap_or(&[]) {
            g[i] = 0.0;
        }
        let r = simulate(&g, candles, cfg).map_err(|e| e.to_string())?;
        let n = positions(&r.account);
        Ok(Score {
            fitness: period_score(r.gain_pct(), n, days, &activity),
            details: vec![
                ("gain".into(), r.gain_pct().to_string()),
                ("positions".into(), n.to_string()),
            ],
        })
    };
    let report = evolve(GENOME_LEN, evolve_cfg, fit)?;
    let best = report.population.first().ok_or("empty population")?;
    let get = |k: &str| {
        best.score
            .details
            .iter()
            .find(|d| d.0 == k)
            .map(|d| d.1.clone())
            .unwrap_or_default()
    };
    Ok((
        best.score.fitness,
        get("gain").parse().unwrap_or_default(),
        get("positions").parse().unwrap_or_default(),
        report.evaluations,
    ))
}

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
    // Weights of div1h, EMA, trend, patterns (genes 2..10): 0 for "divergence only".
    let div_only: Vec<usize> = (2..10).collect();

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
        let days = (to - from).num_seconds() as f64 / 86_400.0;
        let month: Vec<&Candle> = candles
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

        let all = best_evolved(&candles, &cfg, days, &ecfg, None)?;
        let div = best_evolved(&candles, &cfg, days, &ecfg, Some(&div_only))?;

        // N random traders, N = the evaluations of the "all rules" evolution.
        let mut rng = Rng::new(1000 + u64::from(k));
        let (mut best, mut gains) = ((Decimal::MIN, Decimal::ZERO, 0usize), Decimal::ZERO);
        for i in 0..all.3 {
            let stop = dec!(1) + Decimal::from(rng.below(19)) * dec!(0.5); // 1 - 10 %
            let trader = RandomTrader {
                p_open: 1.0 / 32.0,
                p_close: 1.0 / 32.0,
                account: AccountParams {
                    stop_pct: Some(stop),
                    ..account
                },
            };
            let a = trader.run(&candles, from, to, i as u64 + 1)?;
            let n = positions(&a);
            let s = period_score(a.gain_pct, n, days, &activity);
            gains += a.gain_pct;
            if s > best.0 {
                best = (s, a.gain_pct, n);
            }
        }
        let mean = gains / Decimal::from(all.3.max(1) as u64);
        println!(
            "{:<8} {:>6}% | {:>8} {:>+7}% ({:>2}) {:>+6}% | {:>8} {:>+7}% ({:>2}) | {:>8} {:>+7}% ({:>2})",
            from.format("%Y-%m"),
            sol.round_dp(1),
            best.0.round_dp(1),
            best.1.round_dp(1),
            best.2,
            mean.round_dp(1),
            div.0.round_dp(1),
            div.1.round_dp(1),
            div.2,
            all.0.round_dp(1),
            all.1.round_dp(1),
            all.2
        );
        sums[0] += best.0;
        sums[1] += mean;
        sums[2] += div.0;
        sums[3] += all.0;
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
