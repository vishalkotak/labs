pub mod workload;

use std::io;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::Semaphore;

// One KV-cache chunk; a fixed-size run of bytes.
// 4096 because that's the native block size of
// an NVME device so one of our pages maps to one
// device block with no awakard fragmentation.
pub const PAGE_SIZE: usize = 4906;

pub type PageId = u64;

pub struct Page {
    pub data: Box<[u8]>,
}

impl Page {
    pub fn zeroed() -> Self {
        
        Page { data: vec![0u8; PAGE_SIZE].into_boxed_slice() }
    }
}

// This is our baseline, everything else is measured against.
pub struct DramBackend {
    map: Mutex<HashMap<PageId, Box<[u8]>>>,
}

// DRAM backend serializes under a Mutex, so it can't handle 
// multiple operations in parallel (does not explore queue 
// depth). This is acceptable for me because this in memory
// processing is going to be fast. But will not work for NvME.
impl DramBackend {
    pub fn new() -> Self {
        DramBackend { map: Mutex::new(HashMap::new()) }
    }
}

// We will have a harness whose job includes controlling queue
// depth - how many read and writes are in flight at once. We 
// will have multiple get futures against the backend.
pub trait Backend {
    
    async fn put(&self, id: PageId, page: &Page) -> io::Result<()>;

    async fn get(&self, id: PageId, out: &mut Page) -> io::Result<()>;

    async fn delete(&self, id: PageId) -> io::Result<()>;
}

impl Backend for DramBackend {
    
    async fn put(&self, id: PageId, page: &Page) -> io::Result<()> {
        let mut map = self.map.lock().unwrap();
        map.insert(id, page.data.clone());
        Ok(())
    }

    async fn get(&self, id: PageId, out: &mut Page) -> io::Result<()> {
        let map = self.map.lock().unwrap();
        match map.get(&id) {
            Some(bytes) => {
                out.data.copy_from_slice(bytes);
                Ok(())
            }   
            None => Err(io::Error::new(io::ErrorKind::NotFound, "page not found")),
        }
    }

    async fn delete(&self, id: PageId) -> io::Result<()> {
        let mut map = self.map.lock().unwrap();
        map.remove(&id);
        Ok(())
    }
}

struct CacheEntry {
    data: Box<[u8]>,
    last_used: u64,
}

// A fetch currently in progress for some page. Followers clone the Arc and
// wait on `ready`; the leader fills `slot` then calls notify_waiters().
struct InFlight {
    slot: Mutex<Option<Box<[u8]>>>,
    ready: Notify,
}

struct Inner {
    capacity: usize,
    hot: HashMap<PageId, CacheEntry>,
    inflight: HashMap<PageId, Arc<InFlight>>,
    tick: u64,
    hits: u64,
    misses: u64,
    coalesced: u64,
    evictions: u64,
}

// State mutated only while the lock is held.
pub struct Cache<B: Backend> {
    backend: B,
    inner: Mutex<Inner>,
}

impl<B: Backend> Cache<B> {
    pub fn new(backend: B, capacity: usize) -> Self {
        assert!(capacity >= 1, "Capacity must be at least 1");
        Cache {
            backend,
            inner: Mutex::new(Inner {
                capacity,
                hot: HashMap::new(),
                inflight: HashMap::new(),
                tick: 0,
                hits: 0,
                misses: 0,
                coalesced: 0,
                evictions: 0,
            }),
        }
    }

    pub async fn get(&self, id: PageId, out: &mut Page) -> io::Result<()> {
        let leader = {
            let mut inner = self.inner.lock().unwrap();
            inner.tick += 1;
            let now = inner.tick;

            if let Some(entry) = inner.hot.get_mut(&id) {
                entry.last_used = now;
                out.data.copy_from_slice(&entry.data);
                inner.hits += 1;
                return Ok(())
            }

            if let Some(inflight) = inner.inflight.get(&id) {
                let handle = Arc::clone(inflight);
                inner.coalesced += 1;
                Some((false, handle))
            } else {
                let inflight = Arc::new(InFlight {
                    slot: Mutex::new(None),
                    ready: Notify::new(),
                });
                inner.inflight.insert(id, Arc::clone(&inflight));
                inner.misses += 1;
                Some((true, inflight))
            }
        };
        let (is_leader, inflight) = leader.unwrap();
        if !is_leader {
            // Subtle ordering: check the slot BEFORE awaiting. If the leader
            // already filled it and fired notify_waiters() before we started
            // waiting, the notification is lost — so we must not block on a
            // notify that already happened.
            loop {
                {
                    let slot = inflight.slot.lock().unwrap();
                    if let Some(bytes) = slot.as_ref() {
                        out.data.copy_from_slice(bytes);
                        return Ok(());
                    }
                }
                    inflight.ready.notified().await;
            }
        }

        let mut tmp = Page::zeroed();
        let read_result = self.backend.get(id, &mut tmp).await;
        match read_result {
            Ok(()) => {
                {
                    let mut slot = inflight.slot.lock().unwrap();
                    *slot = Some(tmp.data.clone());
                }
                inflight.ready.notify_waiters();
                out.data.copy_from_slice(&tmp.data);
                let victims = {
                    let mut inner = self.inner.lock().unwrap();
                    let now = inner.tick;
                    inner.hot.insert(id, CacheEntry { data: tmp.data, last_used: now });
                    inner.inflight.remove(&id);
                    Cache::<B>::take_eviction_victims(&mut inner)
                };
                self.spill(victims).await?;
                Ok(())
            }
            Err(e) => {
                inflight.ready.notify_waiters();
                let mut inner = self.inner.lock().unwrap();
                inner.inflight.remove(&id);
                Err(e)
            }
        }
    }

    pub async fn put(&self, id: PageId, page: &Page) -> io::Result<()> {
        let victims = {
            let mut inner = self.inner.lock().unwrap();
            inner.tick += 1;
            let now = inner.tick;
            inner.hot.insert(id, CacheEntry { data: page.data.clone(), last_used: now });
            Cache::<B>::take_eviction_victims(&mut inner)
        };
        self.spill(victims).await
    }

    fn take_eviction_victims(
        inner: &mut Inner,
    ) -> Vec<(PageId, Box<[u8]>)> {
        let mut victims = Vec::new();
        while inner.hot.len() > inner.capacity {
            let victim = inner
                .hot
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(id, _)| *id)
                .expect("hot is non-empty because len > capacity >= 1");
            let entry = inner.hot.remove(&victim).expect("victim was just found");
            victims.push((victim, entry.data));
            inner.evictions += 1;
        }
        victims
    }

    async fn spill(&self, victims: Vec<(PageId, Box<[u8]>)>) -> io::Result<()> {
        for (id, data) in victims {
            let page = Page { data };
            self.backend.put(id, &page).await?;
        }
        Ok(())
    }

    pub fn hits(&self) -> u64 {
        self.inner.lock().unwrap().hits
    }

    pub fn misses(&self) -> u64 {
        self.inner.lock().unwrap().misses
    }

    pub fn evictions(&self) -> u64 {
        self.inner.lock().unwrap().evictions
    }

    pub fn coalesced(&self) -> u64 {
        self.inner.lock().unwrap().coalesced
    }

    pub fn reset_stats(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        inner.hits = 0;
        inner.misses = 0;
        inner.coalesced = 0;
        inner.evictions = 0;
    }

    pub fn hit_rate(&self) -> f64 {
        let inner = self.inner.lock().unwrap();
        let total = inner.hits + inner.misses;
        if total == 0 {
            0.0
        } else {
            inner.hits as f64 / total as f64
        }

    }
}

use hdrhistogram::Histogram;
use std::time::Instant;
use crate::workload::{generate, Pattern};

pub struct BenchResult {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub coalesced: u64,
    pub hit_rate: f64,
    pub histogram: Histogram<u64>,
}

pub async fn run_workload(
    pattern: Pattern,
    num_pages: u64,
    capacity: usize,
    num_requests: usize,
    queue_depth: usize,
    seed: u64,
) -> io::Result<BenchResult> {

    assert!(queue_depth >= 1, "queue_depth must be at least 1");
    let mut cache = Cache::new(DramBackend::new(), capacity);
    let filler = Page::zeroed();
    for id in 0..num_pages {
        cache.put(id, &filler).await?;
    }
    cache.reset_stats();
    let cache = Arc::new(cache);
    let requests = generate(pattern, num_pages, num_requests, seed);
    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
        .expect("valid histogram bounds");

    if queue_depth == 1 {
        let mut out = Page::zeroed();
        for &id in &requests {
            let start = Instant::now();
            cache.get(id, &mut out).await?;
            let ns = start.elapsed().as_nanos() as u64;
            hist.record(ns).expect("latency within histogram bounds");
        }
    } else {
        let sem = Arc::new(Semaphore::new(queue_depth));
        let hist_shared = Arc::new(Mutex::new(hist));
        let mut tasks = Vec::with_capacity(requests.len());
        for &id in &requests {
            let cache = Arc::clone(&cache);
            let sem = Arc::clone(&sem);
            let hist_shared = Arc::clone(&hist_shared);
            tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore not closed");
                let mut out = Page::zeroed();
                let start = Instant::now();
                let res = cache.get(id, &mut out).await;
                let ns = start.elapsed().as_nanos() as u64;
                if res.is_ok() {
                    hist_shared.lock().unwrap()
                        .record(ns).expect("latency within bounds");
                }
                res
            }));

        }
        for t in tasks {
            t.await.expect("task panicked")?;
        }
        hist = Arc::try_unwrap(hist_shared)
            .expect("all tasks done, sole owner")
            .into_inner()
            .unwrap();
        
        }
        Ok(BenchResult {
            hits: cache.hits(),
            misses: cache.misses(),
            coalesced: cache.coalesced(),
            evictions: cache.evictions(),
            hit_rate: cache.hit_rate(),
            histogram: hist,
        })
}
    


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dram_put_get_delete() {
        let backend = DramBackend::new();

        let mut page = Page::zeroed();
        page.data.fill(0x42);

        backend.put(1, &page).await.unwrap();
        let mut out = Page::zeroed();
        backend.get(1, &mut out).await.unwrap();
        assert_eq!(out.data.as_ref(), page.data.as_ref());

        let mut out2 = Page::zeroed();
        let err = backend.get(999, &mut out2).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        backend.delete(1).await.unwrap();
        let err = backend.get(1, &mut out).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn page_filled(byte: u8) -> Page {
        let mut p = Page::zeroed();
        p.data.fill(byte);
        p
    }

    #[tokio::test]
    async fn tiering_evicts_and_faults_back() {
        // Cold tier is a DramBackend; hot set holds only 2 pages.
        let cache = Cache::new(DramBackend::new(), 2);

        cache.put(1, &page_filled(0xA1)).await.unwrap(); // hot: {1}
        cache.put(2, &page_filled(0xB2)).await.unwrap(); // hot: {1,2}
        cache.put(3, &page_filled(0xC3)).await.unwrap(); // over cap -> evict LRU (1)

        assert_eq!(cache.evictions(), 1); // page 1 spilled to backend

        // Touch 2 so it's more recent than 3.
        let mut out = Page::zeroed();
        cache.get(2, &mut out).await.unwrap();
        assert_eq!(cache.hits(), 1);
        assert_eq!(out.data[0], 0xB2);

        // Get 1: it was evicted, so this is a miss -> fault in from backend.
        // Promoting 1 overflows the hot set, evicting the now-LRU page (3).
        cache.get(1, &mut out).await.unwrap();
        assert_eq!(cache.misses(), 1);
        assert_eq!(out.data[0], 0xA1);     // we got page 1's bytes back
        assert_eq!(cache.evictions(), 2);  // page 3 spilled on the way in
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A backend that wraps DRAM but counts get() calls and adds a small delay,
    /// so concurrent followers actually overlap with the leader's read.
    struct CountingBackend {
        inner: DramBackend,
        reads: AtomicU64,
    }
    impl CountingBackend {
        fn new() -> Self {
            CountingBackend { inner: DramBackend::new(), reads: AtomicU64::new(0) }
        }
    }
    impl Backend for CountingBackend {
        async fn put(&self, id: PageId, page: &Page) -> io::Result<()> {
            self.inner.put(id, page).await
        }
        async fn get(&self, id: PageId, out: &mut Page) -> io::Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            // Yield + tiny sleep so concurrent callers pile up behind one read.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            self.inner.get(id, out).await
        }
        async fn delete(&self, id: PageId) -> io::Result<()> {
            self.inner.delete(id).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_coalesce_to_one_read() {
        // Seed page 7 into the backend only (not the hot set), so gets miss.
        let backend = CountingBackend::new();
        let mut seed = Page::zeroed();
        seed.data.fill(0x77);
        backend.put(7, &seed).await.unwrap();

        let cache = Arc::new(Cache::new(backend, 4));

        // Fire 16 concurrent gets for the same cold page.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let c = Arc::clone(&cache);
            handles.push(tokio::spawn(async move {
                let mut out = Page::zeroed();
                c.get(7, &mut out).await.unwrap();
                out.data[0]
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), 0x77); // everyone got the right bytes
        }

        // The whole point: 16 concurrent misses, but only ONE backend read.
        let reads = cache.backend.reads.load(Ordering::SeqCst);
        assert_eq!(reads, 1, "single-flight should coalesce to one read, got {reads}");
        assert_eq!(cache.coalesced(), 15); // 1 leader + 15 followers
    }
}
