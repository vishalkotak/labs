# nvme-kv-cache

A tiered key-value cache that keeps hot pages in DRAM and spills cold pages to
a slower backing tier, built to study NVMe storage behavior under realistic
access patterns. Inspired by how LLM inference engines (e.g. vLLM's
PagedAttention) offload KV-cache pages from scarce GPU/host memory down a
memory hierarchy.

The central question this project exists to answer: **when is it worth pushing
cache pages down to NVMe and faulting them back, versus recomputing them?**

## Architecture

Three layers, deliberately separated so the storage tier can be swapped without
touching anything above it:

```
Harness   generates access patterns, controls queue depth, records latency
   |
Cache     tiering policy: bounded DRAM hot set, LRU eviction, fault-in on miss
   |
Backend   stores pages by id. Two implementations behind one trait:
          DramBackend (HashMap, baseline) | NvmeBackend (Linux, planned)
```

The `Backend` trait is the seam: the same `Cache` and `Harness` run against
either backend, so any change in the measured numbers is attributable to the
storage tier and not the surrounding code.

## Status

- [x] Core types: `Page` (fixed 4 KiB), `PageId`, `Backend` trait
- [x] `DramBackend`: in-memory baseline
- [x] `Cache`: tiering, LRU eviction, hit/miss/eviction accounting
- [x] Harness: sequential / uniform / zipfian workloads, HDR latency histograms
- [ ] Concurrency / queue depth (in progress)
- [ ] O(1) LRU (replacing the current O(n) victim scan)
- [ ] `NvmeBackend`: `O_DIRECT` + `io_uring`, Linux only (see below)

## Platform note

The NVMe backend requires Linux: `O_DIRECT` and `io_uring` do not exist on
macOS, and Apple Silicon storage sits behind the Apple Fabric controller with
no reachable device queues. The portable layers (everything except
`NvmeBackend`) run anywhere; device experiments target a Linux instance with
**local/instance NVMe** (not network-attached block storage).

## Running

```
cargo run --release   # runs the harness across all three access patterns
cargo test            # unit tests for the backend and tiering logic
```

`--release` matters: timing a debug build measures the absence of the optimizer.

## Page size

`PAGE_SIZE = 4096` matches the common NVMe device block size, which keeps
`O_DIRECT` alignment on the future NVMe backend trivial (one page = one block).
