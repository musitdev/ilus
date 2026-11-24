//! Evolution of agent genomes (spec F11) on the last month of SOL/USDT 15m.
//!
//! 1. Candles: fetched once from Binance (the month + the warm-up history),
//!    or read from `--candles file.csv`; saved with `--save file.csv`.
//! 2. `select::evolve`: each generation, every new genome is simulated
//!    (`agent::simulate`, in parallel) on the same month; fitness = gain %.
//! 3. Prints the best of each generation and the final population with
//!    their scores and genomes; every evaluation goes to `--history`
//!    (CSV, default `evolve_history.csv`) for analysis.
//!
//! `cargo run --release -p select --example evolve -- [--days 30]
//! [--candles f.csv] [--save f.csv] [--history h.csv] [--population 10]
//! [--generations 50] [--elite 2] [--mutation-rate 0.3] [--sigma-min 0.02]
//! [--sigma-max 0.25] [--crossover 0.7] [--tournament 3] [--patience 10]
//! [--min-improvement 0.5] [--seed 1] [--leverage 2] [--stop 3] [--fee 0.05]`

use std::fmt::Write as _;

use agent::account::AccountParams;
use agent::candles::{fetch_binance, from_csv, to_csv};
use agent::genome::{AgentParams, GENES, GENOME_LEN};
use agent::simulate::{SimConfig, history_needed, simulate};
use chrono::{Days, TimeDelta, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use select::{EvolveConfig, Report, Score, evolve};
use trading::source::Timeframe;

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

    // Candles: the month + the longest warm-up any genome can need.
    let warm = history_needed(&AgentParams::decode(&[1.0; GENOME_LEN])?);
    let candles = match &candles_file {
        Some(f) => from_csv(&std::fs::read_to_string(f)?)?,
        None => {
            let today = Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .ok_or("date")?
                .and_utc();
            let from = today.checked_sub_days(Days::new(days)).ok_or("--days")?;
            let fetch_from = from - TimeDelta::minutes(15 * warm as i64);
            fetch_binance("SOLUSDT", Timeframe::M15, fetch_from, today)?
        }
    };
    if let Some(f) = &save_file {
        std::fs::write(f, to_csv(&candles))?;
    }
    let first = candles.get(warm).ok_or("not enough candles")?.open_time;
    let last = candles.last().ok_or("no candles")?.open_time;
    let sim = SimConfig {
        from: first,
        to: last + TimeDelta::minutes(15),
        account,
    };
    println!(
        "Evolution: {} genomes x {} generations max (elite {}, mutation {} at sigma {}..{}, crossover {}, seed {})",
        cfg.population,
        cfg.generations,
        cfg.elite,
        cfg.mutation_rate,
        cfg.sigma_min,
        cfg.sigma_max,
        cfg.crossover,
        cfg.seed
    );
    println!(
        "Period {} -> {} (UTC), {} candles ({} warm-up), {}x, stop {}, fee {}%\n",
        sim.from.format("%Y-%m-%d %H:%M"),
        sim.to.format("%Y-%m-%d %H:%M"),
        candles.len(),
        warm,
        account.leverage.normalize(),
        account
            .stop_pct
            .map_or("none".into(), |s| format!("{}%", s.normalize())),
        account.fee_pct.normalize()
    );

    let report = evolve(GENOME_LEN, &cfg, |genome: &[f64]| {
        simulate(genome, &candles, &sim).map(|r| Score {
            fitness: r.gain_pct(),
            details: vec![
                ("events".into(), r.account.trades.to_string()),
                ("stops".into(), r.account.stops.to_string()),
                ("liquidations".into(), r.account.liquidations.to_string()),
                (
                    "min_equity".into(),
                    r.account.min_equity.round_dp(2).to_string(),
                ),
            ],
        })
    })?;

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
    println!("Final population:");
    for (rank, s) in report.population.iter().enumerate() {
        let d: Vec<String> = s
            .score
            .details
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        println!(
            "#{rank} gain {}%  ({})",
            s.score.fitness.round_dp(2),
            d.join(", ")
        );
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
