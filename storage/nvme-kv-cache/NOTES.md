# Notes: things I had to work through

Not code documentation. Just the concepts I questioned and the decisions
whose *reasons* I want to remember, in the order they came up.

## Why get() fills a caller's buffer instead of returning a Page

Three possible shapes: return a borrow (`&[u8]`), return an owned `Page`, or
fill a caller-provided `&mut Page`.

- Borrow: rejected. The NVMe backend has no in-memory bytes to point at until
  it reads from disk, so there's nothing to borrow. Correctness problem.
- Owned: works, but allocates a fresh 4 KiB page every call, polluting latency
  measurement with allocator time. Cost problem.
- Fill caller's buffer: chosen. Caller allocates one buffer and reuses it, so
  zero allocation in the timed loop. The hard backend (NVMe) dictated the
  contract.

## copy_from_slice / clone() are real byte copies, not reference bumps

memcpy of all 4 KiB. Two independent regions afterward. Must be a copy, not a
shared reference, or the caller would hold something tied to the backend's
internals that breaks when the map mutates. The copy is unavoidable anyway: a
disk read physically lands bytes in your buffer. (Rc/Arc::clone share and bump
a refcount; moving a Box moves the pointer, not the bytes, which are different
things.)

## .lock().unwrap()

`.lock()` acquires the mutex (waits if someone holds it), returns a guard that
gives temporary &mut access through a shared &self, which is interior
mutability. Guard auto-releases the lock when it drops. `.unwrap()` panics if
the lock was "poisoned" (a thread panicked while holding it); fine for us since
poisoned state means our cache is already suspect.

## &self (Backend) vs &mut self (Cache)

- Backend is `&self` because it's the concurrency surface: queue depth = many
  I/Os in flight = many concurrent futures, and `&mut` is exclusive so it would
  serialize them. Backend uses interior mutability to pay for this.
- Cache is `&mut self` because it's single-threaded for now: trivial field
  mutation, no locks. BUT this makes the cache the current serialization point:
  even though the backend can run N I/Os at once, only one cache.get() runs at
  a time. That's what the queue-depth step has to fix.
- The trap: don't "just add a Mutex to the cache". If you hold a lock across
  the backend's `.await` (a 100us NVMe read), every other request stalls for
  that whole read. The real design releases the lock BEFORE awaiting the slow
  read. The suspension point is the thing you must not straddle with a lock.

## async / await / ? : what they actually do

- `async fn` returns a *future*: an inert computation that does nothing until a
  runtime (tokio) drives it.
- `.await` runs the future and, if it can't finish immediately (waiting on
  disk), *suspends this function* and lets the thread do other work, resuming
  from the same line when data is ready. Queue depth = many functions suspended
  at their awaits, all waiting on the device at once. One thread can hold N
  in-flight I/Os; the blocking model would need N threads.
- DRAM awaits are ~free (finish on first poll, never suspend). NVMe awaits are
  where suspension actually happens.
- `?` is unrelated to await: it's error propagation. On `Err`, return early from
  this function and hand the error up. `.await?` = wait, then bail if it failed.
  Only usable in a function that itself returns Result.

## Logical vs device block size (and why PAGE_SIZE = 4096)

`O_DIRECT` alignment must use the *device* block size, not the advertised
logical one. Setting PAGE_SIZE = 4096 makes one page = one device block, so
alignment is trivial. Confirm the real number on Linux via
/sys/block/<dev>/queue/{logical,physical}_block_size before trusting any
O_DIRECT math.

## Mac NVMe discovery

My M4 MacBook Pro HAS an NVMe SSD (system_profiler SPNVMeDataType shows
"APPLE SSD AP0512Z" under NVMExpress), but it's unusable for this project.
`diskutil info disk0` reports `Protocol: Apple Fabric` (not PCI-Express): the
flash is fused into Apple's controller behind APFS, with no O_DIRECT, no
io_uring, no reachable submission/completion queues. macOS has neither O_DIRECT
nor io_uring at all (closest is the F_NOCACHE fcntl, which only nudges the page
cache aside, not the same guarantee).

Bonus: the same output showed capacity in 512-byte units but a Device Block
Size of 4096, a "512e" drive (512-byte emulation over 4K physical). This is
exactly the logical-vs-device split the alignment rule above exists to handle,
seen in the wild.

Conclusion: NVMe device work happens on a Linux box with local/instance NVMe
(not network-attached storage), which is also why the device tier got built
behind a trait: only NvmeBackend is Linux-bound.
