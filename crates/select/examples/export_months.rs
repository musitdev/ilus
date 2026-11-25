//! Training data for the network (spec F12): per month, the best agent
//! evolved with all the rules, and its trades as labels of the candles.
//!
//! For each calendar month:
//! - evolve the agents on that month (month score: gain − activity
//!   penalty, target 1 – 2 positions per day) and keep the best genome;
//! - the best of N random traders (N = the genomes evaluated) gives the
//!   margin of the agent over luck (recorded, not a filter);
//! - write `OUT/YYYY-MM.csv` (the month's candles labelled with the agent's
//!   actions and position), `OUT/YYYY-MM_events.csv` (its trades), and a
//!   line of `OUT/months.csv` (scores, margin, genome, parameters).
//!
//! `cargo run --release -p select --example export_months -- --candles year.csv
//! [--out dataset] [--months 12] [--start 2025-11-01] [--population 32]
//! [--generations 30] [--leverage 2] [--fee 0.05]`

use std::fmt::Write as _;
use std::path::PathBuf;

use agent::account::AccountParams;
use agent::candles::from_csv;
use agent::dataset::{events_csv, label, to_csv};
use agent::genome::GENES;
use agent::simulate::SimConfig;
use chrono::{DateTime, Months, NaiveDate, Utc};
use rust_decimal_macros::dec;
use select::EvolveConfig;
use select::month::{best_random, evolve_month, positions};
use select::score::Activity;
use trading::candle::Candle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut candles_file = None::<String>;
    let mut out = PathBuf::from("dataset");
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
            "--out" => out = PathBuf::from(v),
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
    std::fs::create_dir_all(&out)?;

    let mut summary = String::from(
        "month,score,gain_pct,positions,stops,liquidations,min_equity,random_best_score,margin,evaluations",
    );
    for g in GENES {
        let _ = write!(summary, ",{}", g.name);
    }
    summary.push_str(",params\n");
    println!(
        "{:<8} {:>9} {:>9} {:>5} {:>6} {:>12} {:>9}",
        "month", "score", "gain", "pos", "stops", "random best", "margin"
    );
    for k in 0..months {
        let from = start.checked_add_months(Months::new(k)).ok_or("date")?;
        let to = from.checked_add_months(Months::new(1)).ok_or("date")?;
        let name = from.format("%Y-%m").to_string();
        let cfg = SimConfig { from, to, account };
        let ecfg = EvolveConfig {
            seed: u64::from(k) + 1,
            ..evolve_cfg
        };
        let best = evolve_month(&candles, &cfg, &ecfg, &activity, &[])?;
        let (rand, _) = best_random(
            &candles,
            from,
            to,
            account,
            &activity,
            best.evaluations,
            1000 + u64::from(k),
        )?;
        let a = &best.result.account;
        let margin = best.score - rand.0;

        let month: Vec<Candle> = candles
            .iter()
            .filter(|c| c.open_time >= from && c.open_time < to)
            .cloned()
            .collect();
        std::fs::write(
            out.join(format!("{name}.csv")),
            to_csv(&label(&month, &a.events)),
        )?;
        std::fs::write(
            out.join(format!("{name}_events.csv")),
            events_csv(&a.events),
        )?;

        let _ = write!(
            summary,
            "{name},{},{},{},{},{},{},{},{},{}",
            best.score.round_dp(4),
            best.result.gain_pct().round_dp(4),
            positions(a),
            a.stops,
            a.liquidations,
            a.min_equity.round_dp(2),
            rand.0.round_dp(4),
            margin.round_dp(4),
            best.evaluations
        );
        for g in &best.genome {
            let _ = write!(summary, ",{g:.6}");
        }
        let _ = writeln!(summary, ",\"{}\"", best.result.params);
        println!(
            "{name:<8} {:>9} {:>+8}% {:>5} {:>6} {:>12} {:>+9}",
            best.score.round_dp(1),
            best.result.gain_pct().round_dp(1),
            positions(a),
            a.stops,
            rand.0.round_dp(1),
            margin.round_dp(1)
        );
    }
    std::fs::write(out.join("months.csv"), summary)?;
    println!(
        "\nWritten to {}: months.csv, YYYY-MM.csv (labelled candles), YYYY-MM_events.csv",
        out.display()
    );
    Ok(())
}
