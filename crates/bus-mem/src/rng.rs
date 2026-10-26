//! Deterministic SplitMix64 generator. Every fault decision draws from one
//! generator seeded from the bus seed, so the same seed and operation order
//! reproduce the same fault trace (RFC-0011 [0011-061]).

#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `[0, 1)` built from 53 random bits.
    pub(crate) fn next_unit(&mut self) -> f64 {
        const SCALE: f64 = (1_u64 << 53) as f64;
        (self.next_u64() >> 11) as f64 / SCALE
    }

    /// Uniform index in `0..n`. The caller guarantees `n >= 1`.
    pub(crate) fn next_below(&mut self, n: usize) -> usize {
        let modulus = u64::try_from(n).unwrap_or(u64::MAX).max(1);
        usize::try_from(self.next_u64() % modulus).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    // [0011-061] same seed gives the same sequence; a different seed does not.
    #[test]
    fn req_0011_061_same_seed_same_sequence() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let mut c = SplitMix64::new(43);
        let xs: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
    }

    #[test]
    fn unit_values_stay_in_half_open_interval() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..10_000 {
            let v = rng.next_unit();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn next_below_stays_in_range() {
        let mut rng = SplitMix64::new(9);
        for _ in 0..1_000 {
            assert!(rng.next_below(3) < 3);
        }
        assert_eq!(rng.next_below(1), 0);
    }
}
