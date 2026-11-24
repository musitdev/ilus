//! Evolution of agent genomes (spec F11) on SOL/USDT 15m.
//!
//! 1. Periods: `--train FROM:TO` (repeatable; the fitness is computed on
//!    them) and `--test FROM:TO` (repeatable; only measured on the final
//!    population, never used for selection). Without `--train`: the last
//!    `--days` (30) days.
//! 2. Candles: fetched once from Binance (every period + the longest warm-up
//!    any genome can need), or read from `--candles file.csv`; saved with
//!    `--save file.csv`.
//! 3. `select::evolve`: each generation, every new genome is simulated
//!    (`agent::simulate`, in parallel) on every training period; fitness =
//!    the mean (`--combine mean`, default) or the worst (`--combine min`)
//!    of the gains in %.
//! 4. Prints the best and mean fitness of each generation and the final
//!    population with the gain on each training and test period; every
//!    evaluation goes to `--history` (CSV, default `evolve_history.csv`).
//!
//! `cargo run --release -p select --example evolve -- [--train 2026-09-01:2026-10-01]...
//! [--test 2026-06-01:2026-07-01]... [--combine mean|min] [--days 30]
//! [--candles f.csv] [--save f.csv] [--history h.csv] [--population 10]
//! [--generations 50] [--elite 2] [--mutation-rate 0.3] [--sigma-min 0.02]
//! [--sigma-max 0.25] [--crossover 0.7] [--tournament 3] [--patience 10]
//! [--min-improvement 0.5] [--seed 1] [--leverage 2] [--stop 3] [--fee 0.05]`

use std::fmt::Write as _;

use agent::account::AccountParams;
use agent::candles::{fetch_binance, from_csv, to_csv};
use agent::genome::{AgentParams, GENES, GENOME_LEN};
use agent::simulate::{SimConfig, SimResult, history_needed, simulate};
use chrono::{DateTime, Days, NaiveDate, TimeDelta, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use select::{EvolveConfig, Report, Score, evolve};
use trading::candle::Candle;
use trading::source::Timeframe;

/// A simulation period: name (dates) and bounds.
#[derive(Debug, Clone)]
struct Period {
    name: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

fn parse_period(s: &str) -> Result<Period, String> {
    let (a, b) = s
        .split_once(':')
        .ok_or(format!("period {s}: FROM:TO expected"))?;
    let day = |d: &str| -> Result<DateTime<Utc>, String> {
        NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|e| format!("period {s}: {e}"))?
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc())
            .ok_or(format!("period {s}: bad date"))
    };
    let (from, to) = (day(a)?, day(b)?);
    if from >= to {
        return Err(format!("period {s}: FROM must be before TO"));
    }
    Ok(Period {
        name: format!("{a}:{b}"),
        from,
        to,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Combine {
    Mean,
    Min,
}

fn run(
    genome: &[f64],
    candles: &[Candle],
    p: &Period,
    a: AccountParams,
) -> Result<SimResult, String> {
    let cfg = SimConfig {
        from: p.from,
        to: p.to,
        account: a,
    };
    simulate(genome, candles, &cfg).map_err(|e| format!("{}: {e}", p.name))
}

fn genome_text(g: &[f64]) -> String {
    g.iter()
        .map(|x| format!("{x:.4}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn history_csv(r: &Report) -> String {
    let mut s = String::from("generation,rank,origin,sigma,evaluated,fitness");
    let detail_names: Vec<String> = r
        .history
        .first()
        .map(|h| h.scored.score.details.iter().map(|d| d.0.clone()).collect())
        .unwrap_or_default();
    for d in &detail_names {
        let _ = write!(s, ",{d}");
    }
    for g in GENES {
        let _ = write!(s, ",{}", g.name);
    }
    s.push_str(",params\n");
    for h in &r.history {
        let _ = write!(
            s,
            "{},{},{},{:.4},{},{}",
            h.generation,
            h.rank,
            h.origin,
            h.sigma,
            h.evaluated,
            h.scored.score.fitness.round_dp(4)
        );
        for (_, v) in &h.scored.score.details {
            let _ = write!(s, ",{v}");
        }
        for x in &h.scored.genome {
            let _ = write!(s, ",{x:.6}");
        }
        let params = AgentParams::decode(&h.scored.genome)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let _ = writeln!(s, ",\"{params}\"");
    }
    s
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = EvolveConfig::default();
    let mut days: u64 = 30;
    let (mut train, mut test) = (Vec::<Period>::new(), Vec::<Period>::new());
    let mut combine = Combine::Mean;
    let (mut candles_file, mut save_file) = (None::<String>, None::<String>);
    let mut history_file = "evolve_history.csv".to_string();
    let mut account = AccountParams {
        capital: dec!(1000),
        leverage: dec!(2),
        stop_pct: Some(dec!(3)),
        fee_pct: dec!(0.05),
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let v = args.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--train" => train.push(parse_period(&v)?),
            "--test" => test.push(parse_period(&v)?),
            "--combine" => {
                combine = match v.as_str() {
                    "mean" => Combine::Mean,
                    "min" => Combine::Min,
                    _ => return Err("--combine: mean or min".into()),
                }
            }
            "--days" => days = v.parse()?,
            "--candles" => candles_file = Some(v),
            "--save" => save_file = Some(v),
            "--history" => history_file = v,
            "--population" => cfg.population = v.parse()?,
            "--generations" => cfg.generations = v.parse()?,
            "--elite" => cfg.elite = v.parse()?,
            "--mutation-rate" => cfg.mutation_rate = v.parse()?,
            "--sigma-min" => cfg.sigma_min = v.parse()?,
            "--sigma-max" => cfg.sigma_max = v.parse()?,
            "--crossover" => cfg.crossover = v.parse()?,
            "--tournament" => cfg.tournament = v.parse()?,
            "--patience" => cfg.patience = v.parse()?,
            "--min-improvement" => cfg.min_improvement = v.parse()?,
            "--seed" => cfg.seed = v.parse()?,
            "--leverage" => account.leverage = v.parse()?,
            "--stop" => account.stop_pct = Some(v.parse()?).filter(|s: &Decimal| !s.is_zero()),
            "--fee" => account.fee_pct = v.parse()?,
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    if train.is_empty() {
        let today = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .ok_or("date")?
            .and_utc();
        let from = today.checked_sub_days(Days::new(days)).ok_or("--days")?;
        train.push(Period {
            name: format!("last {days} days"),
            from,
            to: today,
        });
    }

    // Candles: every period + the longest warm-up any genome can need.
    let warm = history_needed(&AgentParams::decode(&[1.0; GENOME_LEN])?);
    let all: Vec<&Period> = train.iter().chain(&test).collect();
    let start = all.iter().map(|p| p.from).min().ok_or("no period")?;
    let end = all.iter().map(|p| p.to).max().ok_or("no period")?;
    let candles = match &candles_file {
        Some(f) => from_csv(&std::fs::read_to_string(f)?)?,
        None => fetch_binance(
            "SOLUSDT",
            Timeframe::M15,
            start - TimeDelta::minutes(15 * warm as i64),
            end,
        )?,
    };
    if let Some(f) = &save_file {
        std::fs::write(f, to_csv(&candles))?;
    }

    println!(
        "Evolution: {} genomes x {} generations max (elite {}, mutation {} at sigma {}..{}, crossover {}, seed {}), fitness: {:?} gain of the training periods",
        cfg.population,
        cfg.generations,
        cfg.elite,
        cfg.mutation_rate,
        cfg.sigma_min,
        cfg.sigma_max,
        cfg.crossover,
        cfg.seed,
        combine
    );
    let names = |ps: &[Period]| {
        ps.iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "Train: {}; test: {}; {} candles; {}x, stop {}, fee {}%\n",
        names(&train),
        if test.is_empty() {
            "none".into()
        } else {
            names(&test)
        },
        candles.len(),
        account.leverage.normalize(),
        account
            .stop_pct
            .map_or("none".into(), |s| format!("{}%", s.normalize())),
        account.fee_pct.normalize()
    );

    let report = evolve(
        GENOME_LEN,
        &cfg,
        |genome: &[f64]| -> Result<Score, String> {
            let mut gains = Vec::new();
            let mut details = Vec::new();
            for p in &train {
                let r = run(genome, &candles, p, account)?;
                details.push((
                    format!("gain {}", p.name),
                    r.gain_pct().round_dp(2).to_string(),
                ));
                details.push((format!("events {}", p.name), r.account.trades.to_string()));
                details.push((format!("stops {}", p.name), r.account.stops.to_string()));
                gains.push(r.gain_pct());
            }
            let fitness = match combine {
                Combine::Mean => gains.iter().sum::<Decimal>() / Decimal::from(gains.len() as u64),
                Combine::Min => gains.iter().copied().min().unwrap_or(Decimal::MIN),
            };
            Ok(Score { fitness, details })
        },
    )?;

    for (g, best) in report.best_per_generation.iter().enumerate() {
        let gen_: Vec<Decimal> = report
            .history
            .iter()
            .filter(|h| h.generation == g)
            .map(|h| h.scored.score.fitness)
            .collect();
        let mean = gen_.iter().sum::<Decimal>() / Decimal::from(gen_.len().max(1) as u64);
        println!(
            "generation {g:>3}: best {:>8}%  mean {:>8}%",
            best.round_dp(2),
            mean.round_dp(2)
        );
    }
    println!(
        "\nStopped after {} generations ({:?}), {} simulations\n",
        report.generations, report.stop, report.evaluations
    );
    println!("Final population (gain % per period; test periods not used for selection):");
    for (rank, s) in report.population.iter().enumerate() {
        let mut line = format!(
            "#{rank} fitness {:>7}%  train:",
            s.score.fitness.round_dp(1)
        );
        for p in &train {
            let r = run(&s.genome, &candles, p, account)?;
            let _ = write!(
                line,
                " {:>7}% ({} ev, {} st)",
                r.gain_pct().round_dp(1),
                r.account.trades,
                r.account.stops
            );
        }
        if !test.is_empty() {
            line.push_str("  test:");
            for p in &test {
                let r = run(&s.genome, &candles, p, account)?;
                let _ = write!(
                    line,
                    " {:>7}% ({} ev, {} st)",
                    r.gain_pct().round_dp(1),
                    r.account.trades,
                    r.account.stops
                );
            }
        }
        println!("{line}");
        println!("   {}", AgentParams::decode(&s.genome)?);
        println!("   --genome {}", genome_text(&s.genome));
    }
    std::fs::write(&history_file, history_csv(&report))?;
    println!(
        "\nHistory: {} evaluations in {history_file}",
        report.history.len()
    );
    Ok(())
}
