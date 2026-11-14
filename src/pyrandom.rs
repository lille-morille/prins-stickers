//! CPython's `random.Random`: MT19937 with `init_by_array` seeding, so the same seed gives the same
//! numbers as the Python design engine. Only the methods the engine calls (and their obvious
//! neighbours) are here. Behaviour follows CPython 3.12.

use sha2::{Digest, Sha512};

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

/// What Python would raise as `ValueError` or `IndexError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RandomError(pub String);

#[derive(Clone)]
pub struct PyRandom {
    mt: [u32; N],
    index: usize,
    gauss_next: Option<f64>,
}

impl std::fmt::Debug for PyRandom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PyRandom")
    }
}

impl PyRandom {
    fn blank() -> PyRandom {
        PyRandom {
            mt: [0; N],
            index: N + 1,
            gauss_next: None,
        }
    }

    /// `random.Random(n)` for an int. A negative seed uses its absolute value, as in Python.
    pub fn from_int(seed: i64) -> PyRandom {
        PyRandom::from_u128(seed.unsigned_abs() as u128)
    }

    /// `random.Random(n)` for an int that does not fit in an i64.
    pub fn from_u128(seed: u128) -> PyRandom {
        let mut words: Vec<u32> = Vec::new();
        let mut rest = seed;
        while rest > 0 {
            words.push((rest & 0xffff_ffff) as u32);
            rest >>= 32;
        }
        PyRandom::from_key(&words)
    }

    /// `random.Random(s)` for a str: the UTF-8 bytes followed by their SHA-512 digest, read as one big-endian int.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(seed: &str) -> PyRandom {
        let mut bytes = seed.as_bytes().to_vec();
        bytes.extend_from_slice(&Sha512::digest(seed.as_bytes()));
        PyRandom::from_big_endian(&bytes)
    }

    /// `random.Random(bytes)`: seeded like a str, from the raw bytes.
    pub fn from_bytes(seed: &[u8]) -> PyRandom {
        let mut bytes = seed.to_vec();
        bytes.extend_from_slice(&Sha512::digest(seed));
        PyRandom::from_big_endian(&bytes)
    }

    fn from_big_endian(bytes: &[u8]) -> PyRandom {
        // Little-endian 32-bit words of the big int, least significant first.
        let mut words: Vec<u32> = Vec::new();
        let mut end = bytes.len();
        while end > 0 {
            let start = end.saturating_sub(4);
            let mut word = 0u32;
            for &b in &bytes[start..end] {
                word = (word << 8) | b as u32;
            }
            words.push(word);
            end = start;
        }
        while words.last() == Some(&0) {
            words.pop();
        }
        PyRandom::from_key(&words)
    }

    fn from_key(key: &[u32]) -> PyRandom {
        let mut rng = PyRandom::blank();
        if key.is_empty() {
            rng.init_by_array(&[0]);
        } else {
            rng.init_by_array(key);
        }
        rng
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..N {
            self.mt[i] = 1_812_433_253u32
                .wrapping_mul(self.mt[i - 1] ^ (self.mt[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19_650_218);
        let mut i = 1usize;
        let mut j = 0usize;
        let mut k = N.max(key.len());
        while k > 0 {
            self.mt[i] = (self.mt[i]
                ^ (self.mt[i - 1] ^ (self.mt[i - 1] >> 30)).wrapping_mul(1_664_525))
            .wrapping_add(key[j])
            .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = N - 1;
        while k > 0 {
            self.mt[i] = (self.mt[i]
                ^ (self.mt[i - 1] ^ (self.mt[i - 1] >> 30)).wrapping_mul(1_566_083_941))
            .wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                self.mt[0] = self.mt[N - 1];
                i = 1;
            }
            k -= 1;
        }
        self.mt[0] = 0x8000_0000;
        self.index = N;
        self.gauss_next = None;
    }

    fn genrand_u32(&mut self) -> u32 {
        if self.index >= N {
            for kk in 0..N {
                let y = (self.mt[kk] & UPPER_MASK) | (self.mt[(kk + 1) % N] & LOWER_MASK);
                let mut v = self.mt[(kk + M) % N] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= MATRIX_A;
                }
                self.mt[kk] = v;
            }
            self.index = 0;
        }
        let mut y = self.mt[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `random.random()`: a float in [0, 1) with 53 bits.
    pub fn random(&mut self) -> f64 {
        let a = (self.genrand_u32() >> 5) as f64;
        let b = (self.genrand_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `random.getrandbits(k)` for k up to 128.
    pub fn getrandbits(&mut self, k: u32) -> u128 {
        if k == 0 {
            return 0;
        }
        let k = k.min(128);
        if k <= 32 {
            return (self.genrand_u32() >> (32 - k)) as u128;
        }
        let words = k.div_ceil(32);
        let mut left = k;
        let mut value: u128 = 0;
        for i in 0..words {
            let mut r = self.genrand_u32();
            if left < 32 {
                r >>= 32 - left;
            }
            value |= (r as u128) << (32 * i);
            left = left.saturating_sub(32);
        }
        value
    }

    /// `Random._randbelow(n)`: an int in [0, n). Returns 0 for n of 0.
    pub fn randbelow(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.getrandbits(k) as u64;
            if r < n {
                return r;
            }
        }
    }

    fn randbelow_wide(&mut self, n: u128) -> u128 {
        if n == 0 {
            return 0;
        }
        let k = 128 - n.leading_zeros();
        loop {
            let r = self.getrandbits(k);
            if r < n {
                return r;
            }
        }
    }

    /// `random.randrange(stop)`.
    pub fn randrange(&mut self, stop: i64) -> Result<i64, RandomError> {
        if stop > 0 {
            Ok(self.randbelow(stop as u64) as i64)
        } else {
            Err(RandomError("empty range for randrange()".into()))
        }
    }

    /// `random.randrange(start, stop)`.
    pub fn randrange2(&mut self, start: i64, stop: i64) -> Result<i64, RandomError> {
        self.randrange3(start, stop, 1)
    }

    /// `random.randrange(start, stop, step)`.
    pub fn randrange3(&mut self, start: i64, stop: i64, step: i64) -> Result<i64, RandomError> {
        let width = stop as i128 - start as i128;
        let step_wide = step as i128;
        let empty = || RandomError(format!("empty range in randrange({start}, {stop}, {step})"));
        let n: i128 = if step == 1 {
            if width > 0 {
                let pick = self.randbelow_wide(width as u128) as i128;
                return Ok((start as i128 + pick) as i64);
            }
            return Err(RandomError(format!(
                "empty range in randrange({start}, {stop})"
            )));
        } else if step > 0 {
            (width + step_wide - 1).div_euclid(step_wide)
        } else if step < 0 {
            (width + step_wide + 1).div_euclid(step_wide)
        } else {
            return Err(RandomError("zero step for randrange()".into()));
        };
        if n <= 0 {
            return Err(empty());
        }
        let pick = self.randbelow_wide(n as u128) as i128;
        Ok((start as i128 + step_wide * pick) as i64)
    }

    /// `random.randint(a, b)`: an int in [a, b].
    pub fn randint(&mut self, a: i64, b: i64) -> Result<i64, RandomError> {
        match b.checked_add(1) {
            Some(stop) => self.randrange2(a, stop),
            None => {
                let width = b as i128 + 1 - a as i128;
                if width <= 0 {
                    return Err(RandomError(format!(
                        "empty range in randrange({a}, {})",
                        b as i128 + 1
                    )));
                }
                let pick = self.randbelow_wide(width as u128) as i128;
                Ok((a as i128 + pick) as i64)
            }
        }
    }

    /// `random.choice(seq)`.
    pub fn choice<'a, T>(&mut self, seq: &'a [T]) -> Result<&'a T, RandomError> {
        if seq.is_empty() {
            return Err(RandomError("Cannot choose from an empty sequence".into()));
        }
        let i = self.randbelow(seq.len() as u64) as usize;
        Ok(&seq[i])
    }

    /// `random.shuffle(x)`, in place.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.randbelow(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// `random.uniform(a, b)`.
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.random()
    }

    /// `random.sample(population, k)`: k unique items, in selection order.
    pub fn sample<T: Clone>(&mut self, population: &[T], k: usize) -> Result<Vec<T>, RandomError> {
        let n = population.len();
        if k > n {
            return Err(RandomError(
                "Sample larger than population or is negative".into(),
            ));
        }
        let mut result: Vec<T> = Vec::with_capacity(k);
        let mut setsize: u64 = 21;
        if k > 5 {
            let exponent = (((k * 3) as f64).ln() / 4f64.ln()).ceil();
            setsize += 4u64.saturating_pow(exponent as u32);
        }
        if n as u64 <= setsize {
            let mut pool: Vec<T> = population.to_vec();
            for i in 0..k {
                let j = self.randbelow((n - i) as u64) as usize;
                result.push(pool[j].clone());
                pool[j] = pool[n - i - 1].clone();
            }
        } else {
            let mut selected = std::collections::HashSet::new();
            for _ in 0..k {
                let mut j = self.randbelow(n as u64) as usize;
                while selected.contains(&j) {
                    j = self.randbelow(n as u64) as usize;
                }
                selected.insert(j);
                result.push(population[j].clone());
            }
        }
        Ok(result)
    }

    /// `random.gauss(mu, sigma)`.
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        let z = match self.gauss_next.take() {
            Some(z) => z,
            None => {
                let x2pi = self.random() * std::f64::consts::TAU;
                let g2rad = (-2.0 * (1.0 - self.random()).ln()).sqrt();
                self.gauss_next = Some(x2pi.sin() * g2rad);
                x2pi.cos() * g2rad
            }
        };
        mu + z * sigma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values come from CPython 3.12: random.Random(seed).random() and friends.
    #[test]
    fn random_matches_python_for_int_seeds() {
        let mut r = PyRandom::from_int(0);
        assert_eq!(r.random(), 0.8444218515250481);
        assert_eq!(r.random(), 0.7579544029403025);
        let mut r = PyRandom::from_int(1);
        assert_eq!(r.random(), 0.13436424411240122);
        assert_eq!(r.random(), 0.8474337369372327);
        let mut r = PyRandom::from_int(42);
        assert_eq!(r.random(), 0.6394267984578837);
        assert_eq!(r.random(), 0.025010755222666936);
    }

    #[test]
    fn negative_seed_uses_absolute_value() {
        let mut a = PyRandom::from_int(-7);
        let mut b = PyRandom::from_int(7);
        assert_eq!(a.random(), b.random());
    }

    #[test]
    fn big_int_seed() {
        let mut r = PyRandom::from_int(123_456_789_012_345);
        assert_eq!(r.random(), REF_BIG_SEED);
        let mut r = PyRandom::from_u128(340_282_366_920_938_463_463_374_607_431_768_211_455);
        assert_eq!(r.random(), REF_U128_SEED);
    }

    #[test]
    fn str_seed_matches_python() {
        let mut r = PyRandom::from_str("abc");
        assert_eq!(r.random(), REF_ABC);
        let mut r = PyRandom::from_str("");
        assert_eq!(r.random(), REF_EMPTY);
        let mut r = PyRandom::from_str("håndverk \u{1F680}");
        assert_eq!(r.random(), REF_UNICODE);
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut r = PyRandom::from_str(hex);
        assert_eq!(r.random(), REF_HEX);
    }

    #[test]
    fn getrandbits_matches_python() {
        let mut r = PyRandom::from_int(5);
        r.getrandbits(1);
        assert_eq!(r.getrandbits(8), REF_BITS[0]);
        assert_eq!(r.getrandbits(32), REF_BITS[1]);
        assert_eq!(r.getrandbits(33), REF_BITS[2]);
        assert_eq!(r.getrandbits(64), REF_BITS[3]);
        assert_eq!(r.getrandbits(100), REF_BITS[4]);
    }

    #[test]
    fn randrange_and_randint_match_python() {
        let mut r = PyRandom::from_int(7);
        let got: Vec<i64> = (0..8).map(|_| r.randrange(312).unwrap()).collect();
        assert_eq!(got, REF_RANGE_312);
        let got: Vec<i64> = (0..8).map(|_| r.randrange2(120, 192).unwrap()).collect();
        assert_eq!(got, REF_RANGE_120_192);
        let got: Vec<i64> = (0..8).map(|_| r.randint(-2, 2).unwrap()).collect();
        assert_eq!(got, REF_RANDINT);
        let got: Vec<i64> = (0..8).map(|_| r.randrange3(0, 100, 7).unwrap()).collect();
        assert_eq!(got, REF_STEP);
        let got: Vec<i64> = (0..8).map(|_| r.randrange3(50, 0, -5).unwrap()).collect();
        assert_eq!(got, REF_NEG_STEP);
        let got: Vec<i64> = (0..4).map(|_| r.randrange(1).unwrap()).collect();
        assert_eq!(got, vec![0, 0, 0, 0]);
        let got: Vec<i64> = (0..4).map(|_| r.randrange(1 << 40).unwrap()).collect();
        assert_eq!(got, REF_BIG_RANGE);
    }

    #[test]
    fn randrange_errors_have_python_messages() {
        let mut r = PyRandom::from_int(1);
        assert_eq!(r.randrange(0).unwrap_err().0, "empty range for randrange()");
        assert_eq!(
            r.randrange2(5, 5).unwrap_err().0,
            "empty range in randrange(5, 5)"
        );
        assert_eq!(
            r.randrange3(0, 5, 0).unwrap_err().0,
            "zero step for randrange()"
        );
        assert!(r.randint(3, 2).is_err());
        let empty: [i32; 0] = [];
        assert_eq!(
            r.choice(&empty).unwrap_err().0,
            "Cannot choose from an empty sequence"
        );
    }

    #[test]
    fn shuffle_choice_uniform_match_python() {
        let mut r = PyRandom::from_str("shuffle");
        let mut list: Vec<u32> = (0..10).collect();
        r.shuffle(&mut list);
        assert_eq!(list, REF_SHUFFLE);
        let faces = ["black", "white", "white", "black", "gray"];
        let got: Vec<&str> = (0..8).map(|_| *r.choice(&faces).unwrap()).collect();
        assert_eq!(got, REF_CHOICE);
        let got: Vec<f64> = (0..3).map(|_| r.uniform(-7.0, 7.0)).collect();
        assert_eq!(got, REF_UNIFORM);
    }

    #[test]
    fn sample_matches_python() {
        let mut r = PyRandom::from_int(3);
        let small: Vec<u32> = (0..10).collect();
        assert_eq!(r.sample(&small, 4).unwrap(), REF_SAMPLE_SMALL);
        let big: Vec<u32> = (0..1000).collect();
        assert_eq!(r.sample(&big, 5).unwrap(), REF_SAMPLE_BIG);
        let mid: Vec<u32> = (0..60).collect();
        assert_eq!(r.sample(&mid, 8).unwrap(), REF_SAMPLE_MID);
        assert!(r.sample(&small, 11).is_err());
    }

    #[test]
    fn gauss_matches_python() {
        let mut r = PyRandom::from_int(11);
        let got: Vec<f64> = (0..3).map(|_| r.gauss(0.0, 1.0)).collect();
        assert_eq!(got, REF_GAUSS);
    }

    // Generated with CPython 3.12 (random.Random(seed) ...).
    const REF_BIG_SEED: f64 = 0.6744750084019356;
    const REF_U128_SEED: f64 = 0.23507302092285776;
    const REF_ABC: f64 = 0.7720246314157545;
    const REF_EMPTY: f64 = 0.9602256525641875;
    const REF_UNICODE: f64 = 0.9813983721154228;
    const REF_HEX: f64 = 0.02372704697169903;
    const REF_BITS: [u128; 5] = [
        65,
        3185950873,
        5834865596,
        17385139117288923102,
        1160853548457195330571808359216,
    ];
    const REF_RANGE_312: [i64; 8] = [165, 77, 202, 24, 37, 274, 48, 187];
    const REF_RANGE_120_192: [i64; 8] = [127, 184, 147, 124, 131, 175, 173, 128];
    const REF_RANDINT: [i64; 8] = [-1, -2, 2, 1, -2, 2, -2, -1];
    const REF_STEP: [i64; 8] = [70, 70, 63, 0, 63, 63, 42, 0];
    const REF_NEG_STEP: [i64; 8] = [35, 50, 10, 40, 30, 20, 40, 10];
    const REF_BIG_RANGE: [i64; 4] = [415060972871, 212052832771, 1091806279135, 1023551468271];
    const REF_SHUFFLE: [u32; 10] = [3, 7, 5, 4, 0, 9, 1, 6, 8, 2];
    const REF_CHOICE: [&str; 8] = [
        "white", "gray", "black", "black", "gray", "black", "white", "black",
    ];
    const REF_UNIFORM: [f64; 3] = [-3.0613335026666224, 0.6793597077685716, -4.912968125846675];
    const REF_SAMPLE_SMALL: [u32; 4] = [3, 8, 2, 7];
    const REF_SAMPLE_BIG: [u32; 5] = [937, 618, 485, 640, 594];
    const REF_SAMPLE_MID: [u32; 8] = [4, 38, 0, 53, 30, 16, 35, 14];
    const REF_GAUSS: [f64; 3] = [-1.224072675713965, 0.3775881983015979, 0.9949996276709397];
}
