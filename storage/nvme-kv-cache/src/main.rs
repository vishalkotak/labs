use nvme_kv_cache::workload::Pattern;
use nvme_kv_cache::run_workload;

// AI Generated
#[tokio::main(flavor = "current_thread")]
async fn main() {

    let num_pages = 10_000;
    let capacity = 1_000;
    let num_requests = 200_000;
    let seed = 42;
    for pattern in [
        Pattern::Sequential,
        Pattern::Uniform,
        Pattern::Zipf { skew: 1.0 },
    ] {
        let r = run_workload(pattern, num_pages, capacity, num_requests, seed)
            .await
            .unwrap();
        println!("pattern = {:?}", pattern);
        println!(
            "  hit_rate  = {:.3}  (hits={}, misses={})",
            r.hit_rate, r.hits, r.misses
        );
        println!("  evictions = {}", r.evictions);
        println!("  p50       = {} ns", r.histogram.value_at_quantile(0.50));
        println!("  p99       = {} ns", r.histogram.value_at_quantile(0.99));
        println!("  p999      = {} ns", r.histogram.value_at_quantile(0.999));
        println!();
    }
}

