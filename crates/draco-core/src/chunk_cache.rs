//! Immutable-chunk cache (RAM LRU over an on-disk store) for the Tier 2 engine.
//!
//! # Why this exists
//!
//! Web bundlers (webpack, Vite, esbuild, ...) give code chunks content-hashed
//! filenames such as `vendor.3f9c2b1a.js`. That naming scheme means a chunk
//! URL identifies immutable bytes: once we have fetched a chunk, refetching
//! the same URL can never legitimately yield different content. This cache
//! exploits that immutability to make repeat scrapes of a site sub-second —
//! chunks we have already seen are served from RAM or disk instead of the
//! network.
//!
//! # Layering
//!
//! * **RAM layer** — a least-recently-used map bounded by total payload bytes
//!   (`RAM_BUDGET_BYTES`, 32 MiB). Keys are exact URL strings, values are
//!   `Arc<[u8]>`. A hit marks the entry most-recently-used; an insert evicts
//!   least-recently-used entries until the byte and entry caps are respected.
//! * **Disk layer** — one file per chunk in `$HOME/.cache/draco/chunks`
//!   (falling back to `<system temp>/draco/chunks` when `HOME` is unset),
//!   each named with the hex of a 64-bit hash of the URL. Usage is capped at
//!   ~2 GiB (`DISK_CAP_BYTES`) best-effort: an approximate byte counter is
//!   seeded by a one-time directory scan at open and bumped on writes; when
//!   it trips the cap, an eviction pass deletes the oldest files (by mtime)
//!   until usage is under ~90% of the cap.
//!
//! `ChunkCache::get` checks RAM first, then disk; a disk hit is promoted back
//! into RAM. `ChunkCache::put` writes through to both layers.
//!
//! # Collision safety
//!
//! Disk filenames are only a 64-bit hash, so two distinct URLs can collide.
//! Every record is therefore self-describing: magic `DCC1`, a little-endian
//! `u32` URL byte length, the URL bytes, then the chunk bytes. `get()` only
//! returns the chunk when the URL stored *inside* the file is byte-for-byte
//! the requested URL — a filename collision degrades to a cache miss and can
//! never serve the wrong bytes.
//!
//! # Defensive contract: never worse than no cache
//!
//! The cache must never be the reason the engine crashes. There are no
//! `unwrap`/`expect` calls on filesystem or lock-poison paths: every I/O
//! error is swallowed (`put` degrades to a no-op, `get` to a miss), and
//! poisoned mutexes are recovered via `PoisonError::into_inner` rather than
//! propagating a panic. The worst possible outcome of any failure here is
//! that the caller refetches a chunk from the network.
//!
//! All I/O is synchronous and blocking by design; callers invoke this from
//! `spawn_blocking` / current-thread contexts, never directly on an async
//! reactor thread.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::SystemTime;

/// Magic prefix of every on-disk record (record format version 1).
const MAGIC: &[u8] = b"DCC1";

/// RAM layer budget: total cached payload bytes kept in memory.
const RAM_BUDGET_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

/// Maximum number of RAM-resident keys, independent of payload size.
const MAX_RAM_ENTRIES: usize = 4096;

/// Disk layer cap (approximate, best-effort).
const DISK_CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// A single RAM-cached chunk plus its LRU bookkeeping.
struct RamEntry {
    bytes: Arc<[u8]>,
    /// Value of `LruInner::tick` when this entry was last touched.
    /// Larger means more recently used.
    last_used: u64,
}

/// Mutex-protected state of the RAM LRU.
///
/// Recency is tracked with a monotonically increasing tick stamped onto an
/// entry on every touch; eviction scans for the minimum tick. That makes an
/// eviction O(n) rather than O(1), which is deliberate: chunk counts are
/// modest, evictions are rare, and the simplicity keeps this obviously
/// correct (no hand-rolled intrusive list to get wrong).
struct LruInner {
    map: HashMap<String, RamEntry>,
    /// Sum of `bytes.len()` across all entries (payload bytes only).
    total_bytes: usize,
    /// Monotonic usage clock.
    tick: u64,
}

/// Logical ownership counters for the in-process RAM chunk cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RamCacheStats {
    pub entries: usize,
    pub payload_bytes: usize,
    pub key_bytes: usize,
    pub capacity: usize,
}

/// Two-tier (RAM + disk) cache for immutable, content-addressed web chunks.
///
/// Shared across threads as `Arc<ChunkCache>` (it is `Send + Sync`); all
/// methods take `&self` and are safe to call concurrently. See the module
/// docs for the full behavioral contract.
pub(crate) struct ChunkCache {
    ram: Mutex<LruInner>,
    ram_budget: usize,
    /// Directory holding one record file per chunk.
    dir: PathBuf,
    disk_cap: u64,
    /// Approximate bytes currently on disk. Seeded by a one-time directory
    /// scan at open, incremented on writes, and resynced to actual usage by
    /// each eviction pass.
    disk_bytes: AtomicU64,
    /// True while some thread is running a disk eviction pass; concurrent
    /// puts skip eviction instead of piling up behind it.
    disk_evicting: AtomicBool,
    /// Disambiguates temp files written concurrently by this process.
    tmp_counter: AtomicU64,
}

static SHARED: OnceLock<Arc<ChunkCache>> = OnceLock::new();

impl ChunkCache {
    /// Process-global singleton, lazily opened on first use.
    ///
    /// Opening never fails: directory creation and the initial size scan are
    /// best-effort, and if the disk is unusable the cache simply degrades to
    /// its RAM layer (and ultimately to a pass-through miss).
    pub(crate) fn shared() -> Arc<ChunkCache> {
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(ChunkCache::open(
                ChunkCache::default_dir(),
                RAM_BUDGET_BYTES,
                DISK_CAP_BYTES,
            ))
        }))
    }

    /// RAM -> disk lookup. Any error or miss returns `None`.
    ///
    /// A disk hit is promoted into the RAM layer so the next lookup is a
    /// memory hit.
    pub(crate) fn get(&self, url: &str) -> Option<Arc<[u8]>> {
        if let Some(bytes) = self.ram_get(url) {
            return Some(bytes);
        }
        let chunk: Arc<[u8]> = self.disk_get(url)?.into();
        self.ram_put(url, Arc::clone(&chunk));
        Some(chunk)
    }

    /// Write-through to RAM + disk. Never panics; all errors are swallowed.
    pub(crate) fn put(&self, url: &str, bytes: Arc<[u8]>) {
        self.ram_put(url, Arc::clone(&bytes));
        self.disk_put(url, &bytes);
    }

    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Opens a cache over `dir` with the given budgets. Infallible: any
    /// filesystem trouble just leaves the disk layer inert.
    fn open(dir: PathBuf, ram_budget: usize, disk_cap: u64) -> ChunkCache {
        // Best-effort; if this fails, every disk read/write below will also
        // fail (silently) and the cache runs RAM-only.
        let _ = fs::create_dir_all(&dir);
        // One-time scan seeding the approximate on-disk usage counter.
        let initial_bytes = scan_dir_bytes(&dir);
        ChunkCache {
            ram: Mutex::new(LruInner {
                map: HashMap::new(),
                total_bytes: 0,
                tick: 0,
            }),
            ram_budget,
            dir,
            disk_cap,
            disk_bytes: AtomicU64::new(initial_bytes),
            disk_evicting: AtomicBool::new(false),
            tmp_counter: AtomicU64::new(0),
        }
    }

    /// Test-only constructor: inject a private directory and small budgets so
    /// tests never touch the real `$HOME` cache directory.
    #[cfg(test)]
    fn with_dir_for_test(dir: PathBuf, ram_budget: usize, disk_cap: u64) -> ChunkCache {
        ChunkCache::open(dir, ram_budget, disk_cap)
    }

    /// `$HOME/.cache/draco/chunks`, or `<system temp>/draco/chunks` when
    /// `HOME` is unset (an empty `HOME` is treated as unset).
    fn default_dir() -> PathBuf {
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home)
                .join(".cache")
                .join("draco")
                .join("chunks"),
            _ => std::env::temp_dir().join("draco").join("chunks"),
        }
    }

    // ------------------------------------------------------------------
    // RAM layer
    // ------------------------------------------------------------------

    /// Locks the RAM LRU, recovering from poison.
    ///
    /// A poisoned mutex means some thread panicked mid-operation; the worst
    /// case is slightly stale LRU bookkeeping, which only affects eviction
    /// order. Degrade, don't crash.
    fn lock_ram(&self) -> MutexGuard<'_, LruInner> {
        self.ram.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Snapshot RAM ownership counters while holding the cache lock once.
    pub(crate) fn ram_stats(&self) -> RamCacheStats {
        let inner = self.lock_ram();
        RamCacheStats {
            entries: inner.map.len(),
            payload_bytes: inner.total_bytes,
            key_bytes: inner.map.keys().map(String::len).sum(),
            capacity: inner.map.capacity(),
        }
    }

    /// RAM lookup; marks the entry most-recently-used on hit.
    fn ram_get(&self, url: &str) -> Option<Arc<[u8]>> {
        let mut inner = self.lock_ram();
        inner.tick = inner.tick.wrapping_add(1);
        let now = inner.tick;
        let entry = inner.map.get_mut(url)?;
        entry.last_used = now;
        Some(Arc::clone(&entry.bytes))
    }

    /// RAM insert; evicts least-recently-used entries until the total payload
    /// size is back under budget.
    fn ram_put(&self, url: &str, bytes: Arc<[u8]>) {
        let len = bytes.len();
        if len == 0 || len > self.ram_budget {
            // Larger than the entire budget: admitting it would evict
            // everything and still not fit. Empty values consume metadata but
            // no payload budget. Neither belongs in the RAM layer.
            return;
        }
        let mut inner = self.lock_ram();
        inner.tick = inner.tick.wrapping_add(1);
        let now = inner.tick;
        let previous = inner.map.insert(
            url.to_owned(),
            RamEntry {
                bytes,
                last_used: now,
            },
        );
        if let Some(old) = previous {
            inner.total_bytes = inner.total_bytes.saturating_sub(old.bytes.len());
        }
        inner.total_bytes = inner.total_bytes.saturating_add(len);

        // The entry just inserted carries the highest tick, so it is always
        // the last candidate; because `len <= ram_budget` the loop stops
        // before ever reaching it.
        let mut evicted_count = 0usize;
        while inner.total_bytes > self.ram_budget || inner.map.len() > MAX_RAM_ENTRIES {
            let victim = inner
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else { break };
            match inner.map.remove(&victim) {
                Some(evicted) => {
                    inner.total_bytes = inner.total_bytes.saturating_sub(evicted.bytes.len());
                    evicted_count += 1;
                }
                None => break,
            }
        }

        // HashMap never releases buckets on remove. Reclaim them only after a
        // substantial eviction and only when capacity is far above live keys,
        // avoiding resize churn near either steady-state cap.
        let shrink_target = inner.map.len().saturating_mul(4).max(64);
        if evicted_count >= 64 && inner.map.capacity() > shrink_target {
            inner.map.shrink_to(shrink_target);
        }
    }

    // ------------------------------------------------------------------
    // Disk layer
    // ------------------------------------------------------------------

    /// Path of the record file for `url`: hex of a 64-bit `DefaultHasher`
    /// hash. `DefaultHasher::new()` uses fixed keys, so the mapping is stable
    /// across calls and across runs of the same binary.
    fn disk_path(&self, url: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        self.dir.join(format!("{:016x}", hasher.finish()))
    }

    /// Disk lookup: read the record, verify the embedded URL, return the
    /// trailing chunk bytes. Any I/O error, malformed record, or embedded-URL
    /// mismatch (hash collision) is a miss.
    fn disk_get(&self, url: &str) -> Option<Vec<u8>> {
        let data = fs::read(self.disk_path(url)).ok()?;
        parse_record(&data, url)
    }

    /// Disk write: build the self-describing record, write it to a unique
    /// temp file in the same directory, then atomically rename into place.
    /// All errors are swallowed.
    fn disk_put(&self, url: &str, bytes: &[u8]) {
        let url_bytes = url.as_bytes();
        let url_len: u32 = match url_bytes.len().try_into() {
            Ok(len) => len,
            Err(_) => return, // a >4 GiB "URL" is not worth caching
        };

        let mut record = Vec::with_capacity(MAGIC.len() + 4 + url_bytes.len() + bytes.len());
        record.extend_from_slice(MAGIC);
        record.extend_from_slice(&url_len.to_le_bytes());
        record.extend_from_slice(url_bytes);
        record.extend_from_slice(bytes);

        let final_path = self.disk_path(url);
        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("chunk");
        // pid + per-process counter keeps concurrent writers (threads and
        // processes) from clobbering each other's temp files.
        let tmp_path = self.dir.join(format!(
            "{}.tmp.{}.{}",
            file_name,
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::Relaxed),
        ));

        // Best-effort: recreate the directory in case it vanished since open.
        let _ = fs::create_dir_all(&self.dir);
        if fs::write(&tmp_path, &record).is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        // Size of any record we are about to replace, so the approximate
        // byte counter stays roughly honest across overwrites.
        let replaced = fs::metadata(&final_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if fs::rename(&tmp_path, &final_path).is_err() {
            // (E.g. on Windows, renaming onto an existing file can fail. The
            // existing record is for the same immutable URL, so losing this
            // write is harmless.)
            let _ = fs::remove_file(&tmp_path);
            return;
        }

        let written = record.len() as u64;
        if written >= replaced {
            self.disk_bytes
                .fetch_add(written - replaced, Ordering::Relaxed);
        } else {
            self.disk_bytes_sub(replaced - written);
        }
        self.maybe_evict_disk();
    }

    /// Saturating decrement of the approximate on-disk byte counter.
    fn disk_bytes_sub(&self, n: u64) {
        // `fetch_update` with a closure that always returns `Some` cannot
        // fail; the `let _ =` just discards the Ok value.
        let _ = self
            .disk_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            });
    }

    /// Runs a disk eviction pass if the approximate usage exceeds the cap and
    /// no other thread is already evicting (they skip rather than queue).
    fn maybe_evict_disk(&self) {
        if self.disk_bytes.load(Ordering::Relaxed) <= self.disk_cap {
            return;
        }
        if self
            .disk_evicting
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return; // someone else is on it
        }
        let _reset = ResetOnDrop(&self.disk_evicting);
        self.evict_disk();
    }

    /// Eviction pass: list files with sizes and mtimes, delete oldest-first
    /// until actual usage is under ~90% of the cap, then resync the counter
    /// to the measured total (healing any drift in the approximation).
    fn evict_disk(&self) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
        let mut total: u64 = 0;
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = meta.len();
            total = total.saturating_add(size);
            files.push((mtime, entry.path(), size));
        }

        let target = self.disk_cap.saturating_mul(9) / 10; // ~90% of cap
        if total > target {
            files.sort_by_key(|a| a.0); // oldest mtime first
            for (_, path, size) in &files {
                if total <= target {
                    break;
                }
                if fs::remove_file(path).is_ok() {
                    total = total.saturating_sub(*size);
                }
            }
        }
        self.disk_bytes.store(total, Ordering::Relaxed);
    }
}

/// Clears the "eviction in progress" flag even if the pass unwinds.
struct ResetOnDrop<'a>(&'a AtomicBool);

impl Drop for ResetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn shared_stats() -> crate::ChunkCacheStats {
    shared_stats_from(&SHARED)
}

fn shared_stats_from(shared: &OnceLock<Arc<ChunkCache>>) -> crate::ChunkCacheStats {
    let Some(cache) = shared.get() else {
        return crate::ChunkCacheStats::default();
    };
    let stats = cache.ram_stats();
    crate::ChunkCacheStats {
        entries: stats.entries,
        payload_bytes: stats.payload_bytes,
        key_bytes: stats.key_bytes,
        capacity: stats.capacity,
    }
}

/// Parses an on-disk record and returns the chunk bytes, but only if the
/// record is well formed *and* was stored for exactly `expected_url`.
///
/// Record layout: `b"DCC1"` | `u32` LE URL byte length | URL bytes | chunk.
/// Every read is bounds-checked (`slice::get`), so truncated or corrupt
/// files parse to `None` instead of panicking.
fn parse_record(data: &[u8], expected_url: &str) -> Option<Vec<u8>> {
    if data.get(0..4)? != MAGIC {
        return None;
    }
    let url_len = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    let url_end = 8usize.checked_add(url_len)?;
    if data.get(8..url_end)? != expected_url.as_bytes() {
        return None; // filename hash collision (or foreign file): miss, not wrong bytes
    }
    Some(data.get(url_end..)?.to_vec())
}

/// Best-effort sum of the sizes of all regular files directly in `dir`.
/// Missing/unreadable directory counts as zero.
fn scan_dir_bytes(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_on_uninitialized_shared_cache_do_not_initialize_or_touch_disk() {
        let shared = OnceLock::new();
        let dir = unique_test_dir("uninitialized-shared-stats");
        fs::remove_dir_all(&dir).unwrap();
        assert!(!dir.exists());

        let stats = shared_stats_from(&shared);

        assert_eq!(stats, crate::ChunkCacheStats::default());
        assert!(shared.get().is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn shared_ram_cache_budget_stays_compact() {
        assert!(ChunkCache::shared().ram_budget <= 32 * 1024 * 1024);
    }

    #[test]
    fn ram_stats_reports_entries_bytes_and_capacity() {
        let base = unique_test_dir("ram-stats");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 100, u64::MAX);
        assert_eq!(cache.ram_stats().capacity, 0);

        cache.put("a", Arc::from([1u8; 20]));
        cache.put("beta", Arc::from([2u8; 30]));

        let stats = cache.ram_stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.payload_bytes, 50);
        assert_eq!(stats.key_bytes, 5);
        assert!(stats.capacity >= stats.entries);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ram_stats_tracks_replacement_and_eviction() {
        let base = unique_test_dir("ram-stats-eviction");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 60, u64::MAX);

        cache.put("first", Arc::from([1u8; 30]));
        cache.put("second", Arc::from([2u8; 30]));
        cache.put("second", Arc::from([3u8; 20]));
        cache.put("third", Arc::from([4u8; 30]));

        let stats = cache.ram_stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.payload_bytes, 50);
        assert_eq!(stats.key_bytes, "second".len() + "third".len());
        assert!(stats.capacity >= stats.entries);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ram_hit_returns_the_same_shared_payload() {
        let base = unique_test_dir("ram-shared-payload");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 100, u64::MAX);

        cache.put("shared", Arc::from(&b"payload"[..]));
        let first = cache.get("shared").expect("first RAM hit");
        let second = cache.get("shared").expect("second RAM hit");

        assert!(Arc::ptr_eq(&first, &second));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ram_rejects_empty_payloads_and_caps_tiny_unique_entries() {
        let base = unique_test_dir("ram-entry-cap");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), usize::MAX, u64::MAX);

        cache.put("empty", Arc::from(&b""[..]));
        for i in 0..(MAX_RAM_ENTRIES + 500) {
            cache.put(&format!("tiny-{i}"), Arc::from([i as u8]));
        }

        let stats = cache.ram_stats();
        assert_eq!(cache.get("empty"), None);
        assert_eq!(stats.entries, MAX_RAM_ENTRIES);
        assert_eq!(stats.payload_bytes, MAX_RAM_ENTRIES);
        assert!(stats.capacity <= stats.entries.saturating_mul(4).max(64));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ten_thousand_unique_empty_and_tiny_urls_plateau_ram_metadata() {
        let base = unique_test_dir("ram-10k-plateau");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), usize::MAX, u64::MAX);

        for i in 0..10_000 {
            cache.put(
                &format!("https://cdn.example.test/empty/{i}"),
                Arc::from(&b""[..]),
            );
            cache.put(
                &format!("https://cdn.example.test/tiny/{i}"),
                Arc::from([i as u8]),
            );
        }

        let stats = cache.ram_stats();
        assert_eq!(stats.entries, MAX_RAM_ENTRIES);
        assert_eq!(stats.payload_bytes, MAX_RAM_ENTRIES);
        assert!(stats.key_bytes <= MAX_RAM_ENTRIES * 40);
        assert!(stats.capacity <= stats.entries.saturating_mul(4).max(64));
        assert_eq!(cache.get("https://cdn.example.test/empty/9999"), None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_payload_remains_a_valid_disk_record() {
        let dir = unique_test_dir("disk-empty");
        let url = "https://cdn.example.test/empty.js";
        let writer = ChunkCache::with_dir_for_test(dir.clone(), 64, u64::MAX);
        writer.put(url, Arc::from(&b""[..]));

        let reader = ChunkCache::with_dir_for_test(dir.clone(), 64, u64::MAX);
        assert_eq!(reader.get(url).as_deref(), Some(&b""[..]));
        assert_eq!(reader.ram_stats().entries, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ram_plateaus_at_both_byte_and_entry_caps() {
        let base = unique_test_dir("ram-byte-entry-plateau");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 64, u64::MAX);

        for i in 0..(MAX_RAM_ENTRIES + 500) {
            cache.put(&format!("entry-{i}"), Arc::from([i as u8; 2]));
        }

        let stats = cache.ram_stats();
        assert!(stats.entries <= MAX_RAM_ENTRIES);
        assert!(stats.payload_bytes <= 64);
        assert_eq!(stats.entries, 32);

        let _ = fs::remove_dir_all(&base);
    }

    /// Fresh, uniquely named directory under the system temp dir. Never the
    /// real `$HOME` cache location.
    fn unique_test_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "draco-chunk-cache-test-{}-{}-{}-{}",
            tag,
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// A directory path that can never be created (its parent is a regular
    /// file), so every disk operation fails silently and the cache runs
    /// RAM-only. Doubles as a check that the disk layer degrades instead of
    /// panicking.
    fn blocked_disk_dir(base: &Path) -> PathBuf {
        let blocker = base.join("blocker-file");
        let _ = fs::write(&blocker, b"not a directory");
        blocker.join("chunks")
    }

    #[test]
    fn ram_lru_eviction_respects_recency_and_budget() {
        let base = unique_test_dir("ram-lru");
        // RAM budget of 100 bytes; disk disabled via an uncreatable dir.
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 100, u64::MAX);

        cache.put("a", Arc::from([1u8; 40]));
        cache.put("b", Arc::from([2u8; 40]));
        // Touch "a" so "b" becomes the least-recently-used entry.
        assert_eq!(cache.get("a").as_deref(), Some(&[1u8; 40][..]));

        // 120 bytes > 100 budget: the LRU entry ("b") must be evicted.
        cache.put("c", Arc::from([3u8; 40]));
        assert_eq!(cache.get("b"), None, "oldest (LRU) entry should be evicted");
        assert_eq!(
            cache.get("a").as_deref(),
            Some(&[1u8; 40][..]),
            "recently used entry must survive eviction"
        );
        assert_eq!(
            cache.get("c").as_deref(),
            Some(&[3u8; 40][..]),
            "most recently inserted entry must survive eviction"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ram_rejects_values_larger_than_budget() {
        let base = unique_test_dir("ram-oversize");
        let cache = ChunkCache::with_dir_for_test(blocked_disk_dir(&base), 100, u64::MAX);

        cache.put("small", Arc::from([1u8; 30]));
        // 200 bytes > 100-byte budget: never admitted to RAM (and the disk
        // layer is disabled here), so it simply cannot be cached.
        cache.put("huge", Arc::from([9u8; 200]));
        assert_eq!(cache.get("huge"), None);
        // The oversized put must not have nuked existing entries.
        assert_eq!(cache.get("small").as_deref(), Some(&[1u8; 30][..]));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn disk_round_trip_and_collision_mismatch_is_miss() {
        let dir = unique_test_dir("disk-rt");
        let url_a = "https://cdn.example.com/assets/app.abc123.js";
        let url_b = "https://cdn.example.com/assets/vendor.def456.js";
        let payload = b"console.log('chunk payload');".to_vec();

        {
            let writer = ChunkCache::with_dir_for_test(dir.clone(), 1024 * 1024, u64::MAX);
            writer.put(url_a, Arc::from(payload.clone()));
        }

        // Fresh instance: RAM is empty, so this hit must come from disk.
        let cache = ChunkCache::with_dir_for_test(dir.clone(), 1024 * 1024, u64::MAX);
        assert_eq!(cache.get(url_a).as_deref(), Some(payload.as_slice()));

        // Simulate a filename hash collision: url_b's slot holds a record
        // that was written for url_a. The embedded-URL check must miss.
        let path_a = cache.disk_path(url_a);
        let path_b = cache.disk_path(url_b);
        assert!(fs::copy(&path_a, &path_b).is_ok());
        assert_eq!(
            cache.get(url_b),
            None,
            "stored-URL mismatch must be a miss, never wrong bytes"
        );

        // A disk hit promotes into RAM: remove the file and the chunk must
        // still be served (from memory) by the instance that read it.
        let promoted = ChunkCache::with_dir_for_test(dir.clone(), 1024 * 1024, u64::MAX);
        assert_eq!(promoted.get(url_a).as_deref(), Some(payload.as_slice())); // disk hit + promote
        let _ = fs::remove_file(&path_a);
        assert_eq!(promoted.get(url_a).as_deref(), Some(payload.as_slice())); // RAM hit

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_truncated_disk_records_are_misses() {
        let dir = unique_test_dir("disk-corrupt");
        let cache = ChunkCache::with_dir_for_test(dir.clone(), 1024 * 1024, u64::MAX);
        let url = "https://cdn.example.com/chunk.js";
        let path = cache.disk_path(url);

        // Far too short to hold even the header.
        let _ = fs::write(&path, b"XX");
        assert_eq!(cache.get(url), None);

        // Right length, wrong magic.
        let _ = fs::write(&path, b"NOPE\x05\x00\x00\x00hello-bytes");
        assert_eq!(cache.get(url), None);

        // Right magic, but the declared URL length overruns the file.
        let mut record = Vec::new();
        record.extend_from_slice(MAGIC);
        record.extend_from_slice(&1000u32.to_le_bytes());
        record.extend_from_slice(b"short");
        let _ = fs::write(&path, &record);
        assert_eq!(cache.get(url), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_eviction_deletes_oldest_until_under_target() {
        let dir = unique_test_dir("disk-evict");
        // Each record is 4 (magic) + 4 (len) + 2 (url "uN") + 3000 = 3010
        // bytes. Cap 10_000 => eviction target 9_000. A tiny RAM budget keeps
        // every get() on the disk path.
        let cache = ChunkCache::with_dir_for_test(dir.clone(), 64, 10_000);
        let chunk = vec![7u8; 3000];
        for i in 0..5 {
            cache.put(&format!("u{i}"), Arc::from(chunk.clone()));
            // Distinct mtimes so "oldest first" is deterministic (any
            // filesystem with sub-50ms mtime granularity).
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Timeline: after the 4th put the counter hits 12_040 > 10_000, so
        // the eviction pass deletes the two oldest records (down to 6_020 <=
        // 9_000). The 5th put then fits (9_030 <= cap). Survivors: u2 u3 u4.

        let mut file_count = 0usize;
        let mut total_bytes = 0u64;
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        file_count += 1;
                        total_bytes += meta.len();
                    }
                }
            }
        }
        assert_eq!(
            file_count, 3,
            "exactly two records should have been evicted"
        );
        assert!(
            total_bytes <= 10_000,
            "disk usage {total_bytes} exceeds cap"
        );

        // Written after the eviction pass, so it must survive unconditionally.
        assert_eq!(cache.get("u4").as_deref(), Some(&chunk[..]));
        // The two oldest records were deleted first.
        assert_eq!(cache.get("u0"), None);
        assert_eq!(cache.get("u1"), None);
        // A survivor in the middle is still readable.
        assert_eq!(cache.get("u2").as_deref(), Some(&chunk[..]));

        let _ = fs::remove_dir_all(&dir);
    }
}
