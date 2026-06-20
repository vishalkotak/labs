# Benchmarks

Baseline run against `DramBackend` (no NVMe tier yet), 200,000 accesses per
pattern, single-threaded, with the current O(n) victim scan. Latencies are
per `cache.get()`, measured with `--release`. These are a DRAM-only baseline
and not yet a storage result; the hit rates, however, are real and are the
signal that matters at this stage.

| Pattern | Hit rate | Hits | Misses | Evictions | p50 | p99 | p999 |
|---------|----------|------|--------|-----------|-----|-----|------|
| Sequential | 0.000 | 0 | 200,000 | 200,000 | 2,000 ns | 5,375 ns | 16,959 ns |
| Uniform | 0.100 | 20,027 | 179,973 | 179,973 | 1,958 ns | 3,333 ns | 9,215 ns |
| Zipf (skew 1.0) | 0.675 | 135,069 | 64,931 | 64,931 | 125 ns | 3,125 ns | 6,543 ns |

## Reading

- Sequential never revisits a page before it is evicted, so the hot set never
  helps: 0% hits, one eviction per access.
- Uniform random over the key space lands a 10% hit rate, roughly the hot-set
  size as a fraction of the key space.
- Zipf (skew 1.0) concentrates accesses on a small set of keys, so most
  requests are served from DRAM: 67.5% hits.
- The p50 split is the headline. Zipf p50 is 125 ns (the hot-set fast path, no
  eviction), while Sequential and Uniform sit around 2,000 ns because almost
  every access misses and pays the eviction plus fault-in cost. The tail
  (p99/p999) stays in the same band across patterns because the miss path
  dominates it.

This is the setup the NVMe tier will stress: once misses fault in from a real
device instead of a HashMap, the gap between the Zipf fast path and the miss
path is what decides when spilling to NVMe beats recomputing.
