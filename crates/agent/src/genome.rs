//! Genome of the test agent (spec F10): a vector of genes in [0, 1], decoded
//! into the agent's parameters with fixed ranges.
//!
//! - Each gene `g` maps to `min + round(g × steps) × step`, `steps =
//!   (max − min) / step`: the values are exact (`Decimal`), only the gene is
//!   a float.
//! - Genes outside [0, 1] are clamped (a mutation cannot make a genome
//!   invalid); NaN is rejected.
//! - Fixed (not genes): RSI 14, pivots `left` / `right` 3, the divergence
//!   `min_gap` (2 candles of its timeframe), the pattern thresholds.

use std::fmt;

use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal_macros::dec;

use crate::Weights;
use crate::decision::DecisionParams;

/// One gene: its name and the range it maps to.
#[derive(Debug, Clone, Copy)]
pub struct Gene {
    /// Name (for logs).
    pub name: &'static str,
    /// Value for gene 0.
    pub min: Decimal,
    /// Value for gene 1.
    pub max: Decimal,
    /// Resolution.
    pub step: Decimal,
}

const fn gene(name: &'static str, min: Decimal, max: Decimal, step: Decimal) -> Gene {
    Gene {
        name,
        min,
        max,
        step,
    }
}

const W: (Decimal, Decimal, Decimal) = (dec!(0), dec!(5), dec!(0.25));
const T: (Decimal, Decimal, Decimal) = (dec!(0.25), dec!(10), dec!(0.25));

/// The genes, in genome order.
pub const GENES: [Gene; 26] = [
    // Weights (open / exit) of the five rules.
    gene("div15.open_weight", W.0, W.1, W.2),
    gene("div15.exit_weight", W.0, W.1, W.2),
    gene("div1h.open_weight", W.0, W.1, W.2),
    gene("div1h.exit_weight", W.0, W.1, W.2),
    gene("ema.open_weight", W.0, W.1, W.2),
    gene("ema.exit_weight", W.0, W.1, W.2),
    gene("trend.open_weight", W.0, W.1, W.2),
    gene("trend.exit_weight", W.0, W.1, W.2),
    gene("patterns.open_weight", W.0, W.1, W.2),
    gene("patterns.exit_weight", W.0, W.1, W.2),
    // Decision thresholds.
    gene("open_threshold", T.0, T.1, T.2),
    gene("exit_threshold", T.0, T.1, T.2),
    // 15m divergence.
    gene("div15.rsi_low", dec!(20), dec!(50), dec!(1)),
    gene("div15.rsi_high", dec!(50), dec!(80), dec!(1)),
    gene("div15.lookahead", dec!(1), dec!(6), dec!(1)),
    gene("div15.exit_rsi_high", dec!(60), dec!(90), dec!(1)),
    gene("div15.exit_rsi_low", dec!(10), dec!(40), dec!(1)),
    // 1h divergence.
    gene("div1h.rsi_low", dec!(20), dec!(50), dec!(1)),
    gene("div1h.rsi_high", dec!(50), dec!(80), dec!(1)),
    gene("div1h.lookahead", dec!(1), dec!(6), dec!(1)),
    gene("div1h.exit_rsi_high", dec!(60), dec!(90), dec!(1)),
    gene("div1h.exit_rsi_low", dec!(10), dec!(40), dec!(1)),
    // EMAs: the slow period is the fast one plus a gap (always slower).
    gene("ema.fast", dec!(5), dec!(50), dec!(1)),
    gene("ema.slow_gap", dec!(1), dec!(150), dec!(1)),
    gene("trend.period", dec!(20), dec!(300), dec!(1)),
    // Account: stop-loss in % of the entry price.
    gene("stop_pct", dec!(1), dec!(10), dec!(0.5)),
];

/// Number of genes.
pub const GENOME_LEN: usize = GENES.len();

/// Parameters of a divergence rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DivergenceGenes {
    /// RSI level of the first low (bullish).
    pub rsi_low: Decimal,
    /// RSI level of the first high (bearish).
    pub rsi_high: Decimal,
    /// Pivots in which the diverging one is searched.
    pub lookahead: usize,
    /// RSI exit of a long (high pivot above).
    pub exit_rsi_high: Decimal,
    /// RSI exit of a short (low pivot below).
    pub exit_rsi_low: Decimal,
}

/// The agent's parameters, decoded from a genome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentParams {
    /// 15m divergence weights.
    pub div15: Weights,
    /// 1h divergence weights.
    pub div1h: Weights,
    /// EMA cross weights.
    pub ema: Weights,
    /// Trend weights.
    pub trend: Weights,
    /// Candlestick patterns weights.
    pub patterns: Weights,
    /// Decision thresholds.
    pub decision: DecisionParams,
    /// 15m divergence parameters.
    pub div15_rule: DivergenceGenes,
    /// 1h divergence parameters.
    pub div1h_rule: DivergenceGenes,
    /// Fast EMA period.
    pub ema_fast: usize,
    /// Slow EMA period (> fast).
    pub ema_slow: usize,
    /// Trend EMA period.
    pub trend_period: usize,
    /// Stop-loss in % of the entry price.
    pub stop_pct: Decimal,
}

/// Why a genome cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenomeError {
    /// Wrong number of genes.
    Length {
        /// Genes given.
        got: usize,
    },
    /// A gene is NaN.
    NotANumber {
        /// Its name.
        gene: &'static str,
    },
}

impl fmt::Display for GenomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { got } => write!(f, "genome: {got} genes, {GENOME_LEN} expected"),
            Self::NotANumber { gene } => write!(f, "genome: gene {gene} is NaN"),
        }
    }
}

impl std::error::Error for GenomeError {}

impl Gene {
    /// Value of `g` (clamped to [0, 1]).
    pub fn decode(&self, g: f64) -> Result<Decimal, GenomeError> {
        if g.is_nan() {
            return Err(GenomeError::NotANumber { gene: self.name });
        }
        let steps = ((self.max - self.min) / self.step).trunc();
        // Index of the step, an integer in 0..=steps.
        let k = (g.clamp(0.0, 1.0) * steps.to_f64().unwrap_or(0.0)).round();
        let k = Decimal::from_f64(k).unwrap_or(Decimal::ZERO);
        Ok(self.min + k.min(steps) * self.step)
    }

    /// Gene in [0, 1] for `value` (the inverse of [`decode`](Self::decode),
    /// for genomes written by hand).
    pub fn encode(&self, value: Decimal) -> f64 {
        let span = self.max - self.min;
        if span.is_zero() {
            return 0.0;
        }
        ((value - self.min) / span)
            .to_f64()
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }
}

/// An integer gene value (periods, counts).
fn count(d: Decimal) -> usize {
    d.trunc().to_usize().unwrap_or(1)
}

impl AgentParams {
    /// Decode a genome of [`GENOME_LEN`] genes.
    pub fn decode(genome: &[f64]) -> Result<Self, GenomeError> {
        if genome.len() != GENOME_LEN {
            return Err(GenomeError::Length { got: genome.len() });
        }
        let mut v = [Decimal::ZERO; GENOME_LEN];
        for (i, (gene, &g)) in GENES.iter().zip(genome).enumerate() {
            v[i] = gene.decode(g)?;
        }
        let w = |i: usize| Weights {
            open: v[i],
            exit: v[i + 1],
        };
        let div = |i: usize| DivergenceGenes {
            rsi_low: v[i],
            rsi_high: v[i + 1],
            lookahead: count(v[i + 2]),
            exit_rsi_high: v[i + 3],
            exit_rsi_low: v[i + 4],
        };
        let ema_fast = count(v[22]);
        Ok(Self {
            div15: w(0),
            div1h: w(2),
            ema: w(4),
            trend: w(6),
            patterns: w(8),
            decision: DecisionParams {
                open_threshold: v[10],
                exit_threshold: v[11],
            },
            div15_rule: div(12),
            div1h_rule: div(17),
            ema_fast,
            ema_slow: ema_fast + count(v[23]),
            trend_period: count(v[24]),
            stop_pct: v[25],
        })
    }

    /// The genome of these parameters (values off the grid are rounded when
    /// decoded again).
    pub fn encode(&self) -> Vec<f64> {
        let d = |n: usize| Decimal::from(n as u64);
        let div = |r: &DivergenceGenes| {
            [
                r.rsi_low,
                r.rsi_high,
                d(r.lookahead),
                r.exit_rsi_high,
                r.exit_rsi_low,
            ]
        };
        let values: Vec<Decimal> = [
            self.div15.open,
            self.div15.exit,
            self.div1h.open,
            self.div1h.exit,
            self.ema.open,
            self.ema.exit,
            self.trend.open,
            self.trend.exit,
            self.patterns.open,
            self.patterns.exit,
            self.decision.open_threshold,
            self.decision.exit_threshold,
        ]
        .into_iter()
        .chain(div(&self.div15_rule))
        .chain(div(&self.div1h_rule))
        .chain([
            d(self.ema_fast),
            d(self.ema_slow.saturating_sub(self.ema_fast)),
            d(self.trend_period),
            self.stop_pct,
        ])
        .collect();
        GENES.iter().zip(values).map(|(g, v)| g.encode(v)).collect()
    }
}

impl fmt::Display for AgentParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = |w: &Weights| format!("{}/{}", w.open.normalize(), w.exit.normalize());
        let d = |r: &DivergenceGenes| {
            format!(
                "RSI {}/{}, lookahead {}, exit {}/{}",
                r.rsi_low, r.rsi_high, r.lookahead, r.exit_rsi_high, r.exit_rsi_low
            )
        };
        write!(
            f,
            "weights open/exit: div15 {}, div1h {}, ema {}, trend {}, patterns {}; \
             thresholds open {} exit {}; div15 {}; div1h {}; EMA {}/{}; trend EMA {}; stop {}%",
            w(&self.div15),
            w(&self.div1h),
            w(&self.ema),
            w(&self.trend),
            w(&self.patterns),
            self.decision.open_threshold.normalize(),
            self.decision.exit_threshold.normalize(),
            d(&self.div15_rule),
            d(&self.div1h_rule),
            self.ema_fast,
            self.ema_slow,
            self.trend_period,
            self.stop_pct.normalize()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_and_ones_give_the_range_ends() {
        let min = AgentParams::decode(&[0.0; GENOME_LEN]).unwrap();
        assert_eq!(min.div15.open, dec!(0));
        assert_eq!(min.decision.open_threshold, dec!(0.25));
        assert_eq!(min.div15_rule.rsi_low, dec!(20));
        assert_eq!(min.div15_rule.lookahead, 1);
        assert_eq!((min.ema_fast, min.ema_slow, min.trend_period), (5, 6, 20));
        assert_eq!(min.stop_pct, dec!(1));
        let max = AgentParams::decode(&[1.0; GENOME_LEN]).unwrap();
        assert_eq!(max.patterns.exit, dec!(5));
        assert_eq!(max.decision.exit_threshold, dec!(10));
        assert_eq!(max.div1h_rule.exit_rsi_high, dec!(90));
        assert_eq!(max.div1h_rule.lookahead, 6);
        assert_eq!(
            (max.ema_fast, max.ema_slow, max.trend_period),
            (50, 200, 300)
        );
        assert_eq!(max.stop_pct, dec!(10));
    }

    #[test]
    fn genes_map_to_steps_and_are_clamped() {
        let g = gene("w", dec!(0), dec!(5), dec!(0.25)); // 20 steps
        assert_eq!(g.decode(0.5).unwrap(), dec!(2.5));
        assert_eq!(g.decode(0.51).unwrap(), dec!(2.5)); // 10.2 -> step 10
        assert_eq!(g.decode(0.53).unwrap(), dec!(2.75)); // 10.6 -> step 11
        assert_eq!(g.decode(-3.0).unwrap(), dec!(0));
        assert_eq!(g.decode(7.0).unwrap(), dec!(5));
        assert_eq!(
            g.decode(f64::NAN),
            Err(GenomeError::NotANumber { gene: "w" })
        );
        assert_eq!(
            AgentParams::decode(&[0.5; 3]),
            Err(GenomeError::Length { got: 3 })
        );
    }

    #[test]
    fn encode_then_decode_gives_the_same_parameters() {
        let genome: Vec<f64> = (0..GENOME_LEN).map(|i| (i as f64 * 0.37) % 1.0).collect();
        let p = AgentParams::decode(&genome).unwrap();
        assert_eq!(AgentParams::decode(&p.encode()).unwrap(), p);
        assert!(p.ema_slow > p.ema_fast);
    }
}
