use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::PageId;

#[derive(Clone, Copy, Debug)]
pub enum Pattern {
    Sequential,
    Uniform,
    Zipf { skew: f64 },
}

// AI generated
pub fn generate(
    pattern: Pattern,
    num_pages: u64,
    num_requests: usize,
    seed: u64,
) -> Vec<PageId> {
     let mut rng = StdRng::seed_from_u64(seed);
     match pattern {
         Pattern::Sequential => (0..num_requests)
            .map(|i| (i as u64) % num_pages)
            .collect(),
        Pattern::Uniform => (0..num_requests)
            .map(|_| rng.gen_range(0..num_pages))
            .collect(),
        Pattern::Zipf { skew } => {
            let cdf = zipf_cdf(num_pages, skew);
            (0..num_requests)
                .map(|_| sample_zipf(&cdf, &mut rng))
                .collect()
            }
        }
}

// AI generated
// Precompute the cumulative distribution for a zipf over `n` ranks:
// P(rank k) ∝ 1 / k^skew for k = 1..=n. Returns a normalized CDF.
fn zipf_cdf(n: u64, skew: f64) -> Vec<f64> {
    let mut cdf = Vec::with_capacity(n as usize);
    let mut acc = 0.0;
    for k in 1..=n {
        acc += 1.0 / (k as f64).powf(skew);
        cdf.push(acc);
    }
    let total = *cdf.last().unwrap();
    for c in cdf.iter_mut() {
        *c /= total; // make the last entry exactly 1.0
    }
    cdf
}

// AI Generated
// Inverse-CDF sampling: draw u in [0,1), binary-search for its rank.
// Rank 0 is the hottest page, so PageId 0 is the most frequently requested.
fn sample_zipf(cdf: &[f64], rng: &mut StdRng) -> PageId {
    let u: f64 = rng.r#gen();
    // partition_point returns the first index where the predicate turns false,
    // i.e. the first cdf entry strictly greater than u — exactly the rank.
    cdf.partition_point(|&c| c <= u) as PageId
}
