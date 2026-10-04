//! The one source of randomness: splitmix64 over a 64-bit state, forkable by
//! label (TS `rng.ts`).
//!
//! `fork(label)` hashes the ORIGINAL seed with the label, so a child stream is
//! independent of how many draws its parent took: the label, not the call
//! order, decides it. The arithmetic is the TS `BigInt` arithmetic modulo 2^64,
//! and `next_gaussian` uses V8's own `Math.log`/`Math.cos` (`js_math.rs`),
//! so a seed draws bit-identical streams in both products.

use std::fmt;

use serde::{Deserialize, Serialize};

const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
const MIX1: u64 = 0xbf58_476d_1ce4_e5b9;
const MIX2: u64 = 0x94d0_49bb_1331_11eb;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// 2^53 as a double.
const TWO_POW_53: f64 = 9_007_199_254_740_992.0;
/// 2^-53.
const MIN_POSITIVE_UNIT: f64 = 1.0 / TWO_POW_53;

/// A run seed: a number (the CLI's) or a string (hashed), TS `number | string`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seed {
    Number(i64),
    Text(String),
}

impl From<u64> for Seed {
    fn from(value: u64) -> Self {
        Self::Number(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<&str> for Seed {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl fmt::Display for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::Text(value) => f.write_str(value),
        }
    }
}

/// FNV-1a over the UTF-16 code units (`charCodeAt`), as the TS hashes a label.
fn hash_string(input: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for unit in input.encode_utf16() {
        hash ^= u64::from(unit);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The deterministic, forkable generator every rollout and dreaming step draws from.
#[derive(Debug, Clone)]
pub struct SeededRng {
    seed64: u64,
    state: u64,
}

impl SeededRng {
    /// A generator for `seed`: a number is taken modulo 2^64, a string hashed.
    #[must_use]
    pub fn new(seed: &Seed) -> Self {
        let seed64 = match seed {
            // BigInt(n) & MASK64: two's complement for a negative seed.
            #[allow(clippy::cast_sign_loss)]
            Seed::Number(value) => *value as u64,
            Seed::Text(value) => hash_string(value),
        };
        Self::from_u64(seed64)
    }

    fn from_u64(seed64: u64) -> Self {
        Self {
            seed64,
            state: seed64,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(MIX1);
        z = (z ^ (z >> 27)).wrapping_mul(MIX2);
        z ^ (z >> 31)
    }

    /// Uniform double in `[0, 1)` (TS `next`, not an iterator).
    #[allow(clippy::should_implement_trait)]
    // The 53-bit integer converts to a double exactly.
    #[allow(clippy::cast_precision_loss)]
    pub fn next(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / TWO_POW_53
    }

    /// Uniform integer in `[0, max_exclusive)`; `max_exclusive` must be positive.
    ///
    /// # Panics
    ///
    /// When `max_exclusive` is 0 (the TS throws a `RangeError`): every caller
    /// passes a non-empty length.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn next_int(&mut self, max_exclusive: usize) -> usize {
        assert!(max_exclusive > 0, "next_int requires a positive bound");
        (self.next() * max_exclusive as f64).floor() as usize
    }

    /// A standard-normal draw (Box-Muller, finite).
    pub fn next_gaussian(&mut self) -> f64 {
        let mut u1 = self.next();
        if u1 < MIN_POSITIVE_UNIT {
            u1 = MIN_POSITIVE_UNIT;
        }
        let u2 = self.next();
        (-2.0 * crate::js_math::log(u1)).sqrt()
            * crate::js_math::cos(2.0 * std::f64::consts::PI * u2)
    }

    /// A child generator whose stream depends on `label` and the seed only.
    #[must_use]
    pub fn fork(&self, label: &str) -> Self {
        Self::from_u64(hash_string(&format!("{:x}:{label}", self.seed64)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fork_ignores_the_parents_draws_and_differs_by_label() {
        let mut parent = SeededRng::new(&Seed::Number(7));
        let before: Vec<f64> = {
            let mut child = parent.fork("a");
            (0..4).map(|_| child.next()).collect()
        };
        for _ in 0..10 {
            parent.next();
        }
        let mut after_draws = parent.fork("a");
        let after: Vec<f64> = (0..4).map(|_| after_draws.next()).collect();
        assert_eq!(before, after);
        let mut other = parent.fork("b");
        assert_ne!(before[0].to_bits(), other.next().to_bits());
    }

    #[test]
    fn draws_stay_in_range() {
        let mut rng = SeededRng::new(&Seed::Text("seed".to_string()));
        for _ in 0..1000 {
            let value = rng.next();
            assert!((0.0..1.0).contains(&value));
            assert!(rng.next_int(7) < 7);
            assert!(rng.next_gaussian().is_finite());
        }
    }
}
