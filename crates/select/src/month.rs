//! One month (spec F12): the best evolved agent and the best random trader,
//! with the same month score.

use agent::account::{AccountParams, AccountSummary, EventKind};
use agent::genome::GENOME_LEN;
use agent::simulate::{SimConfig, SimResult, simulate};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use trading::candle::Candle;

use crate::baseline::RandomTrader;
use crate::rng::Rng;
use crate::score::{Activity, period_score};
use crate::{EvolveConfig, Score, evolve};

/// Positions opened in an account summary.
pub fn positions(a: &AccountSummary) -> usize {
    a.events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Open(_)))
        .count()
}

/// Days in `[from, to)`.
pub fn days(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_seconds() as f64 / 86_400.0
}

/// The best agent of a month.
#[derive(Debug, Clone)]
pub struct MonthBest {
    /// Its genome (masked genes at 0).
    pub genome: Vec<f64>,
    /// Its month score.
    pub score: Decimal,
    /// Its simulation (parameters, gain, events).
    pub result: SimResult,
    /// Genomes evaluated by the evolution.
    pub evaluations: usize,
}

/// Evolve the agents on `[cfg.from, cfg.to)` with the month score; genes of
/// `mask` are forced to 0 (e.g. the weights of unused rules).
pub fn evolve_month(
    candles: &[Candle],
    cfg: &SimConfig,
    evolve_cfg: &EvolveConfig,
    activity: &Activity,
    mask: &[usize],
) -> Result<MonthBest, String> {
    let d = days(cfg.from, cfg.to);
    let masked = |genome: &[f64]| {
        let mut g = genome.to_vec();
        for &i in mask {
            if let Some(x) = g.get_mut(i) {
                *x = 0.0;
            }
        }
        g
    };
    let report = evolve(
        GENOME_LEN,
        evolve_cfg,
        |genome: &[f64]| -> Result<Score, String> {
            let r = simulate(&masked(genome), candles, cfg).map_err(|e| e.to_string())?;
            Ok(Score {
                fitness: period_score(r.gain_pct(), positions(&r.account), d, activity),
                details: Vec::new(),
            })
        },
    )?;
    let best = report.population.first().ok_or("empty population")?;
    let genome = masked(&best.genome);
    let result = simulate(&genome, candles, cfg).map_err(|e| e.to_string())?;
    Ok(MonthBest {
        genome,
        score: best.score.fitness,
        result,
        evaluations: report.evaluations,
    })
}

/// The best of `n` random traders (1.5 positions per day, stop-loss drawn
/// in the gene's range 1 – 10 %): (score, gain %, positions), and the mean
/// gain of all of them.
pub fn best_random(
    candles: &[Candle],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    account: AccountParams,
    activity: &Activity,
    n: usize,
    seed: u64,
) -> Result<((Decimal, Decimal, usize), Decimal), String> {
    let d = days(from, to);
    let mut rng = Rng::new(seed);
    let mut best = (Decimal::MIN, Decimal::ZERO, 0usize);
    let mut gains = Decimal::ZERO;
    for i in 0..n {
        let stop = dec!(1) + Decimal::from(rng.below(19) as u64) * dec!(0.5);
        let trader = RandomTrader {
            p_open: 1.0 / 32.0,
            p_close: 1.0 / 32.0,
            account: AccountParams {
                stop_pct: Some(stop),
                ..account
            },
        };
        let a = trader.run(candles, from, to, seed.wrapping_mul(1_000_003) + i as u64)?;
        let p = positions(&a);
        let s = period_score(a.gain_pct, p, d, activity);
        gains += a.gain_pct;
        if s > best.0 {
            best = (s, a.gain_pct, p);
        }
    }
    Ok((best, gains / Decimal::from(n.max(1) as u64)))
}
