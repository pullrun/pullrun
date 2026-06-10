use sha2::{Digest, Sha256};

const DEFAULT_FP_RATE: f64 = 0.01;

pub struct BloomFilter {
    bits: Vec<u64>,
    k: u32,
    m: u64,
    num_items: usize,
}

impl BloomFilter {
    pub fn new(m: u64, k: u32) -> Self {
        let word_count = m.div_ceil(64);
        Self {
            bits: vec![0u64; word_count as usize],
            k,
            m,
            num_items: 0,
        }
    }

    pub fn optimal(expected_items: usize) -> Self {
        let m = optimal_m(expected_items, DEFAULT_FP_RATE);
        let k = optimal_k(expected_items, m);
        Self::new(m, k)
    }

    pub fn with_fp_rate(expected_items: usize, fp_rate: f64) -> Self {
        let m = optimal_m(expected_items, fp_rate);
        let k = optimal_k(expected_items, m);
        Self::new(m, k)
    }

    pub fn insert(&mut self, item: &str) {
        let (h1, h2) = hash_pair(item);
        for i in 0..self.k {
            let bit = g_i(h1, h2, i, self.m);
            let word = (bit / 64) as usize;
            let offset = bit % 64;
            self.bits[word] |= 1u64 << offset;
        }
        self.num_items += 1;
    }

    pub fn contains(&self, item: &str) -> bool {
        let (h1, h2) = hash_pair(item);
        for i in 0..self.k {
            let bit = g_i(h1, h2, i, self.m);
            let word = (bit / 64) as usize;
            let offset = bit % 64;
            if self.bits[word] & (1u64 << offset) == 0 {
                return false;
            }
        }
        true
    }

    pub fn merge(&mut self, other: &Self) {
        assert_eq!(self.bits.len(), other.bits.len());
        assert_eq!(self.k, other.k);
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= b;
        }
        self.num_items = self.num_items.max(other.num_items);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.bits.len() * 8);
        out.extend_from_slice(&self.m.to_le_bytes());
        out.extend_from_slice(&self.k.to_le_bytes());
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(data: &[u8]) -> Option<(Self, usize)> {
        if data.len() < 12 {
            return None;
        }
        let m = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let k = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let word_count = (m.div_ceil(64)) as usize;
        let expected_len = 12 + word_count * 8;
        if data.len() < expected_len {
            return None;
        }
        let mut bits = vec![0u64; word_count];
        for (i, w) in bits.iter_mut().enumerate() {
            let off = 12 + i * 8;
            *w = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
        }
        let filter = Self { bits, k, m, num_items: 0 };
        Some((filter, expected_len))
    }

    pub fn k(&self) -> u32 { self.k }
    pub fn m(&self) -> u64 { self.m }
    pub fn num_items(&self) -> usize { self.num_items }
    pub fn clear(&mut self) {
        for w in &mut self.bits {
            *w = 0;
        }
        self.num_items = 0;
    }
}

fn hash_pair(item: &str) -> (u64, u64) {
    let h1 = hash_seed(item, 0);
    let h2 = hash_seed(item, 1);
    (h1, h2)
}

fn hash_seed(item: &str, seed: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(item.as_bytes());
    hasher.update(seed.to_le_bytes());
    let result = hasher.finalize();
    u64::from_le_bytes(result[..8].try_into().unwrap())
}

fn g_i(h1: u64, h2: u64, i: u32, m: u64) -> u64 {
    // Standard Kirsch-Mitzenmacker hash: g_i = (h1 + i * h2) % m
    h1.wrapping_add(i as u64).wrapping_mul(h2) % m
}

fn optimal_m(n: usize, p: f64) -> u64 {
    let ln2 = std::f64::consts::LN_2;
    let m = -(n as f64) * p.ln() / (ln2 * ln2);
    (m.ceil() as u64).max(1)
}

fn optimal_k(n: usize, m: u64) -> u32 {
    let k = (m as f64 / n as f64) * std::f64::consts::LN_2;
    (k.ceil() as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_contains() {
        let mut bf = BloomFilter::optimal(100);
        bf.insert("sha256:abc123");
        bf.insert("sha256:def456");
        assert!(bf.contains("sha256:abc123"));
        assert!(bf.contains("sha256:def456"));
    }

    #[test]
    fn test_false_positive_rate() {
        let mut bf = BloomFilter::with_fp_rate(1000, 0.01);
        for i in 0..1000 {
            bf.insert(&format!("item_{i}"));
        }
        let mut fp = 0;
        let trials = 10000;
        for i in 0..trials {
            if bf.contains(&format!("not_present_{i}")) {
                fp += 1;
            }
        }
        let fp_rate = fp as f64 / trials as f64;
        assert!(fp_rate < 0.05, "FP rate {} > 0.05", fp_rate);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut bf = BloomFilter::optimal(100);
        bf.insert("sha256:aaa");
        bf.insert("sha256:bbb");
        let bytes = bf.to_bytes();
        let (restored, _) = BloomFilter::from_bytes(&bytes).unwrap();
        assert!(restored.contains("sha256:aaa"));
        assert!(restored.contains("sha256:bbb"));
        assert!(!restored.contains("sha256:ccc"));
    }

    #[test]
    fn test_merge() {
        let mut bf1 = BloomFilter::new(640, 4);
        bf1.insert("a");

        let mut bf2 = BloomFilter::new(640, 4);
        bf2.insert("b");

        bf1.merge(&bf2);
        assert!(bf1.contains("a"));
        assert!(bf1.contains("b"));
    }
}
