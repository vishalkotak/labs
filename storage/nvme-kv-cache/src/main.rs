use nvme_kv_cache::workload::Pattern;
use nvme_kv_cache::run_workload;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let num_pages = 10_000;
    let capacity = 1_000;
    let num_requests = 200_000;
    let seed = 42;
    let pattern = Pattern::Zipf { skew: 1.0 }; // the interesting one

    println!("pattern = {:?}, capacity = {}, pages = {}\n",
             pattern, capacity, num_pages);

    for qd in [1, 2, 4, 8, 16, 32] {
        let r = run_workload(pattern, num_pages, capacity, num_requests, qd, seed)
            .await
            .unwrap();

        println!("queue_depth = {:>2}", qd);
        println!("  hit_rate  = {:.3}  coalesced = {}", r.hit_rate, r.coalesced);
        println!("  p50 = {:>8} ns   p99 = {:>9} ns   p999 = {:>9} ns",
                 r.histogram.value_at_quantile(0.50),
                 r.histogram.value_at_quantile(0.99),
                 r.histogram.value_at_quantile(0.999));
        println!();
    }
}
