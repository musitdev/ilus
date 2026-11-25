//! Small deterministic random generator (SplitMix64): same seed, same run.

/// SplitMix64 generator.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in 0..n (n >= 1).
    pub fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n.saturating_sub(1))
    }

    /// Standard normal (Box-Muller).
    pub fn gaussian(&mut self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_and_gaussian_look_right() {
        let mut r = Rng::new(42);
        let n = 20_000;
        let u: Vec<f64> = (0..n).map(|_| r.unit()).collect();
        assert!(u.iter().all(|x| (0.0..1.0).contains(x)));
        let mean = u.iter().sum::<f64>() / n as f64;
        assert!((mean - 0.5).abs() < 0.01, "{mean}");
        let g: Vec<f64> = (0..n).map(|_| r.gaussian()).collect();
        let m = g.iter().sum::<f64>() / n as f64;
        let v = g.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n as f64;
        assert!(m.abs() < 0.03 && (v - 1.0).abs() < 0.05, "{m} {v}");
        assert!((0..1000).all(|_| r.below(10) < 10));
        assert_eq!(Rng::new(7).next_u64(), Rng::new(7).next_u64());
    }
}
