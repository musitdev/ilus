//! ilus selection (spec F11): evolutionary search of agent genomes.
//!
//! - [`evolve`] runs a genetic algorithm on genomes of `len` genes in [0, 1]
//!   with any fitness function (the agent's simulation in practice, see
//!   [`agent::simulate`]).
//! - Each generation: every new genome is evaluated (in parallel threads);
//!   the population is ranked by fitness; the `elite` best are kept
//!   unchanged; each other slot `i` (rank order) gets a child of the
//!   genome ranked `i` and a tournament winner (uniform crossover), mutated
//!   with a Gaussian whose strength grows with the rank: the best mutate
//!   little, the worst a lot.
//! - Stops after `generations`, or when the best fitness improved by less
//!   than `min_improvement` over `patience` generations.
//! - Every evaluation is kept in the [`Report`] history (for analysis).

use std::collections::HashMap;
use std::thread;

use rust_decimal::Decimal;

pub mod baseline;
pub mod month;
pub mod rng;
pub mod score;

use rng::Rng;

/// Settings of the genetic algorithm (several are to be tested).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvolveConfig {
    /// Genomes per generation (>= 2).
    pub population: usize,
    /// Maximum number of generations (>= 1).
    pub generations: usize,
    /// Best genomes kept unchanged (< population).
    pub elite: usize,
    /// Probability that a gene mutates (0..=1).
    pub mutation_rate: f64,
    /// Mutation strength (Gaussian sigma) of the best non-elite genome.
    pub sigma_min: f64,
    /// Mutation strength of the worst genome.
    pub sigma_max: f64,
    /// Probability that a child mixes two parents (else: copy of its parent).
    pub crossover: f64,
    /// Tournament size to choose the second parent (>= 1).
    pub tournament: usize,
    /// Early stop: generations without enough improvement of the best.
    pub patience: usize,
    /// Early stop: minimum improvement of the best fitness (fitness units).
    pub min_improvement: Decimal,
    /// Seed of the random generator (same seed, same run).
    pub seed: u64,
}

impl Default for EvolveConfig {
    fn default() -> Self {
        Self {
            population: 10,
            generations: 50,
            elite: 2,
            mutation_rate: 0.3,
            sigma_min: 0.02,
            sigma_max: 0.25,
            crossover: 0.7,
            tournament: 3,
            patience: 10,
            min_improvement: Decimal::new(5, 1), // 0.5
            seed: 1,
        }
    }
}

impl EvolveConfig {
    fn validate(&self) -> Result<(), String> {
        let p = |x: f64| (0.0..=1.0).contains(&x);
        if self.population < 2 {
            return Err("population must be >= 2".into());
        }
        if self.generations == 0 {
            return Err("generations must be >= 1".into());
        }
        if self.elite >= self.population {
            return Err("elite must be < population".into());
        }
        if !p(self.mutation_rate) || !p(self.crossover) {
            return Err("mutation rate and crossover must be within 0..=1".into());
        }
        if !(self.sigma_min >= 0.0 && self.sigma_max >= self.sigma_min) {
            return Err("need 0 <= sigma_min <= sigma_max".into());
        }
        if self.tournament == 0 {
            return Err("tournament must be >= 1".into());
        }
        Ok(())
    }
}

/// What the fitness function returns for a genome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Score {
    /// The fitness (higher is better), e.g. the gain in %.
    pub fitness: Decimal,
    /// Extra values kept in the history (e.g. trades, stops), as text.
    pub details: Vec<(String, String)>,
}

/// One evaluated genome.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    /// The genes.
    pub genome: Vec<f64>,
    /// Its score.
    pub score: Score,
}

/// One line of the history: a genome in a generation.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Generation (0: the random start).
    pub generation: usize,
    /// Rank in its generation (0: best).
    pub rank: usize,
    /// How it was made: "random", "elite" or "child".
    pub origin: &'static str,
    /// Mutation strength used to make it (0 for random and elite).
    pub sigma: f64,
    /// `true` if it was simulated in this generation (else: known score).
    pub evaluated: bool,
    /// The genome and its score.
    pub scored: Scored,
}

/// Why the evolution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The maximum number of generations was run.
    Generations,
    /// The best fitness stopped improving.
    NoImprovement,
}

/// Result of an evolution.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Generations run (including generation 0).
    pub generations: usize,
    /// Why it stopped.
    pub stop: StopReason,
    /// Best fitness of each generation.
    pub best_per_generation: Vec<Decimal>,
    /// The last population, best first.
    pub population: Vec<Scored>,
    /// Every genome of every generation, with its score.
    pub history: Vec<Record>,
    /// Number of simulations run.
    pub evaluations: usize,
}

/// A genome being built for the next generation.
struct Candidate {
    genome: Vec<f64>,
    origin: &'static str,
    sigma: f64,
}

/// Bit-exact key of a genome (known genomes are not evaluated again).
fn key(genome: &[f64]) -> Vec<u64> {
    genome.iter().map(|g| g.to_bits()).collect()
}

/// Run the evolution on genomes of `len` genes with `fitness` (called from
/// several threads at once).
pub fn evolve<F, E>(len: usize, cfg: &EvolveConfig, fitness: F) -> Result<Report, String>
where
    F: Fn(&[f64]) -> Result<Score, E> + Sync,
    E: std::fmt::Display,
{
    cfg.validate()?;
    if len == 0 {
        return Err("genomes need at least one gene".into());
    }
    let mut rng = Rng::new(cfg.seed);
    let mut known: HashMap<Vec<u64>, Score> = HashMap::new();
    let mut history = Vec::new();
    let mut best_per_generation = Vec::new();
    let mut evaluations = 0;

    let mut candidates: Vec<Candidate> = (0..cfg.population)
        .map(|_| Candidate {
            genome: (0..len).map(|_| rng.unit()).collect(),
            origin: "random",
            sigma: 0.0,
        })
        .collect();

    let mut generation = 0;
    loop {
        // Evaluate the new genomes in parallel.
        let new: Vec<usize> = (0..candidates.len())
            .filter(|&i| !known.contains_key(&key(&candidates[i].genome)))
            .collect();
        let results: Vec<(usize, Result<Score, String>)> = thread::scope(|s| {
            let handles: Vec<_> = new
                .iter()
                .map(|&i| {
                    let genome = &candidates[i].genome;
                    let fitness = &fitness;
                    (
                        i,
                        s.spawn(move || fitness(genome).map_err(|e| e.to_string())),
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(i, h)| {
                    (
                        i,
                        h.join()
                            .unwrap_or_else(|_| Err("fitness function panicked".into())),
                    )
                })
                .collect()
        });
        for (i, r) in results {
            known.insert(key(&candidates[i].genome), r?);
            evaluations += 1;
        }

        // Rank (best first; ties keep the candidate order).
        let mut ranked: Vec<(Candidate, Score, bool)> = candidates
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let score = known.get(&key(&c.genome)).cloned().unwrap_or(Score {
                    fitness: Decimal::MIN,
                    details: Vec::new(),
                });
                (c, score, new.contains(&i))
            })
            .collect();
        ranked.sort_by_key(|r| std::cmp::Reverse(r.1.fitness));
        for (rank, (c, score, evaluated)) in ranked.iter().enumerate() {
            history.push(Record {
                generation,
                rank,
                origin: c.origin,
                sigma: c.sigma,
                evaluated: *evaluated,
                scored: Scored {
                    genome: c.genome.clone(),
                    score: score.clone(),
                },
            });
        }
        let best = ranked[0].1.fitness;
        best_per_generation.push(best);

        let population: Vec<Scored> = ranked
            .into_iter()
            .map(|(c, score, _)| Scored {
                genome: c.genome,
                score,
            })
            .collect();

        // Stop?
        let stop = if generation + 1 >= cfg.generations {
            Some(StopReason::Generations)
        } else if cfg.patience > 0
            && generation >= cfg.patience
            && best - best_per_generation[generation - cfg.patience] < cfg.min_improvement
        {
            Some(StopReason::NoImprovement)
        } else {
            None
        };
        if let Some(stop) = stop {
            return Ok(Report {
                generations: generation + 1,
                stop,
                best_per_generation,
                population,
                history,
                evaluations,
            });
        }

        candidates = next_generation(&population, cfg, &mut rng);
        generation += 1;
    }
}

/// The next generation from a population ranked best first.
fn next_generation(ranked: &[Scored], cfg: &EvolveConfig, rng: &mut Rng) -> Vec<Candidate> {
    let n = ranked.len();
    let mut next: Vec<Candidate> = ranked[..cfg.elite]
        .iter()
        .map(|s| Candidate {
            genome: s.genome.clone(),
            origin: "elite",
            sigma: 0.0,
        })
        .collect();
    let others = n - cfg.elite;
    for (k, parent) in ranked[cfg.elite..].iter().enumerate() {
        // Strength from sigma_min (best non-elite) to sigma_max (worst).
        let t = if others > 1 {
            k as f64 / (others - 1) as f64
        } else {
            0.0
        };
        let sigma = cfg.sigma_min + t * (cfg.sigma_max - cfg.sigma_min);
        let mut genome = parent.genome.clone();
        if rng.unit() < cfg.crossover {
            let mate = &ranked[tournament(n, cfg.tournament, rng)].genome;
            for (g, m) in genome.iter_mut().zip(mate) {
                if rng.unit() < 0.5 {
                    *g = *m;
                }
            }
        }
        for g in genome.iter_mut() {
            if rng.unit() < cfg.mutation_rate {
                *g = (*g + sigma * rng.gaussian()).clamp(0.0, 1.0);
            }
        }
        next.push(Candidate {
            genome,
            origin: "child",
            sigma,
        });
    }
    next
}

/// Index of the best of `size` random picks among `n` ranked genomes.
fn tournament(n: usize, size: usize, rng: &mut Rng) -> usize {
    (0..size).map(|_| rng.below(n)).min().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    /// Fitness: closeness to the target 0.7 on every gene (max 0).
    fn sphere(g: &[f64]) -> Result<Score, String> {
        let d: f64 = g.iter().map(|x| (x - 0.7) * (x - 0.7)).sum();
        Ok(Score {
            fitness: Decimal::from_f64(-d * 1000.0).unwrap_or(Decimal::MIN),
            details: vec![("d".into(), format!("{d:.4}"))],
        })
    }

    fn cfg() -> EvolveConfig {
        EvolveConfig {
            generations: 60,
            patience: 0,
            ..EvolveConfig::default()
        }
    }

    #[test]
    fn the_best_improves_and_never_gets_worse() {
        let r = evolve(5, &cfg(), sphere).unwrap();
        assert_eq!(r.generations, 60);
        assert_eq!(r.stop, StopReason::Generations);
        // Elitism: the best never decreases.
        assert!(r.best_per_generation.windows(2).all(|w| w[1] >= w[0]));
        let (first, last) = (r.best_per_generation[0], r.best_per_generation[59]);
        assert!(last > first, "{first} -> {last}");
        assert!(last > Decimal::from(-20), "close to the target: {last}");
        // The final population is ranked.
        assert!(
            r.population
                .windows(2)
                .all(|w| w[0].score.fitness >= w[1].score.fitness)
        );
    }

    #[test]
    fn history_has_every_genome_of_every_generation() {
        let c = EvolveConfig {
            generations: 5,
            ..cfg()
        };
        let r = evolve(3, &c, sphere).unwrap();
        assert_eq!(r.history.len(), 5 * 10);
        for g in 0..5 {
            let gen_: Vec<&Record> = r.history.iter().filter(|h| h.generation == g).collect();
            assert_eq!(gen_.len(), 10);
            assert_eq!(
                gen_.iter().map(|h| h.rank).collect::<Vec<_>>(),
                (0..10).collect::<Vec<_>>()
            );
            let elites = gen_.iter().filter(|h| h.origin == "elite").count();
            assert_eq!(elites, if g == 0 { 0 } else { 2 });
        }
        // Elites are not evaluated again.
        assert!(
            r.history
                .iter()
                .filter(|h| h.origin == "elite")
                .all(|h| !h.evaluated)
        );
        assert_eq!(
            r.evaluations,
            r.history.iter().filter(|h| h.evaluated).count()
        );
    }

    #[test]
    fn mutation_grows_with_the_rank() {
        let r = evolve(
            3,
            &EvolveConfig {
                generations: 2,
                ..cfg()
            },
            sphere,
        )
        .unwrap();
        let mut children: Vec<f64> = r
            .history
            .iter()
            .filter(|h| h.generation == 1 && h.origin == "child")
            .map(|h| h.sigma)
            .collect();
        children.sort_by(f64::total_cmp);
        assert_eq!(children.len(), 8);
        assert!((children[0] - 0.02).abs() < 1e-12);
        assert!((children[7] - 0.25).abs() < 1e-12);
    }

    #[test]
    fn same_seed_same_run_and_early_stop() {
        let a = evolve(4, &cfg(), sphere).unwrap();
        let b = evolve(4, &cfg(), sphere).unwrap();
        assert_eq!(a, b);
        // A flat fitness never improves: stop after `patience` generations.
        let flat = |_: &[f64]| -> Result<Score, String> {
            Ok(Score {
                fitness: Decimal::ONE,
                details: Vec::new(),
            })
        };
        let c = EvolveConfig {
            patience: 4,
            ..EvolveConfig::default()
        };
        let r = evolve(4, &c, flat).unwrap();
        assert_eq!((r.generations, r.stop), (5, StopReason::NoImprovement));
    }

    #[test]
    fn invalid_settings_and_fitness_errors() {
        let bad = EvolveConfig {
            elite: 10,
            ..EvolveConfig::default()
        };
        assert!(evolve(3, &bad, sphere).is_err());
        let failing = |_: &[f64]| -> Result<Score, String> { Err("no data".into()) };
        assert_eq!(
            evolve(3, &EvolveConfig::default(), failing).unwrap_err(),
            "no data"
        );
    }
}
