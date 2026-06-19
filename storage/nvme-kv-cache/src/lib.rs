pub mod workload;

use std::io;
use std::collections::HashMap;
use std::sync::Mutex;

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

pub struct Cache<B: Backend> {
    backend: B,
    capacity: usize,
    hot: HashMap<PageId, CacheEntry>,
    tick: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl<B: Backend> Cache<B> {
    pub fn new(backend: B, capacity: usize) -> Self {
        assert!(capacity >= 1, "Capacity must be at least 1");
        Cache {
            backend,
            capacity,
            hot: HashMap::new(),
            tick: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    pub async fn get(&mut self, id: PageId, out: &mut Page) -> io::Result<()> {
        self.tick += 1;

        // Already in the hot set. Ajust recency and copy out.
        if let Some(entry) = self.hot.get_mut(&id) {
            entry.last_used = self.tick;
            out.data.copy_from_slice(&entry.data);
            self.hits += 1;
            return Ok(());
        }

        let mut temp = Page::zeroed();
        self.backend.get(id, &mut temp).await?;
        out.data.copy_from_slice(&temp.data);

        self.hot.insert(id, CacheEntry { data: temp.data, last_used: self.tick });
        self.ensure_capacity().await?;
        self.misses += 1;
        Ok(())
    }

    pub async fn put(&mut self, id: PageId, page: &Page) -> io::Result<()> {
        self.tick += 1;
        self.hot.insert(
            id,
            CacheEntry { data: page.data.clone(), last_used: self.tick },
        );
        self.ensure_capacity().await?;
        Ok(())
    }

    async fn ensure_capacity(&mut self) -> io::Result<()> {
        while self.hot.len() > self.capacity {
            let victim = self
                .hot
                .iter()
                .min_by_key(|(_, e) | e.last_used)
                .map(|(id, _) | *id)
                .expect("hot is non-empty because len > capacity >= 1");
            let entry = self.hot.remove(&victim).expect("victim was just found");
            let page = Page { data: entry.data };
            self.backend.put(victim, &page).await?;
            self.evictions += 1;
        }
        Ok(())
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
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
    pub hit_rate: f64,
    pub histogram: Histogram<u64>,
}

pub async fn run_workload(
    pattern: Pattern,
    num_pages: u64,
    capacity: usize,
    num_requests: usize,
    seed: u64,
) -> io::Result<BenchResult> {

    let mut cache = Cache::new(DramBackend::new(), capacity);
    let filler = Page::zeroed();
    for id in 0..num_pages {
        cache.put(id, &filler).await?;
    }
    cache.reset_stats();
    let requests = generate(pattern, num_pages, num_requests, seed);
    let mut out = Page::zeroed();
    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)
        .expect("valid histogram bounds");

    for &id in &requests {
        let start = Instant::now();
        cache.get(id, &mut out).await?;
        let ns = start.elapsed().as_nanos() as u64;
        hist.record(ns).expect("latency within histogram bounds");
    }

    Ok(BenchResult {
        hits: cache.hits(),
        misses: cache.misses(),
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
        let mut cache = Cache::new(DramBackend::new(), 2);

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

