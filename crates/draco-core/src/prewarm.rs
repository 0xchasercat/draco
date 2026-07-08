//! On-demand chunk **prewarmer** (v0.13.14).
//!
//! Tier 2 hydration pulls code-split chunks the up-front prefetch didn't cover
//! via `LoadScript` IPC frames. Each is serviced synchronously by the
//! supervisor's `run_job` loop — the isolate's `op_raze_load_script` is a
//! blocking op on the single V8 thread, and the loop answers one frame at a
//! time — so a page needing *N* on-demand chunks historically paid *N*
//! **sequential** network round-trips (~250 ms each on a Cloudflare-fronted
//! origin). A browser doesn't pay that: its network stack is async, so
//! `import(a)`/`import(b)` fan out concurrently while JS keeps running.
//!
//! The [`Prewarmer`] restores that concurrency without letting the air-gapped
//! isolate touch the network. When a requested chunk lands, its **dependency
//! closure** — the static ES-module imports plus webpack/Next chunk-loader
//! candidates found in its body — is fetched **concurrently in the background**
//! into a per-job cache (on a small multi-thread runtime). The child's *next*
//! (still serial) `LoadScript` then resolves from the warm cache in ~0 ms
//! instead of paying a fresh round-trip. Fetches carry the job's shared cookie
//! jar (via [`crate::tier2::subresource_opts`]), so Cloudflare's `__cf_bm`
//! cookie is reused exactly as v0.13.13 intended.
//!
//! Warming is strictly best-effort and bounded (file-count + total-byte caps,
//! and the cache map doubles as the visited set): a cold miss falls back to a
//! direct fetch — never worse than the pre-v0.13.14 path. Dynamic `import("…")`
//! targets are deliberately excluded from the closure (mirrors the up-front
//! prefetch policy: they are lazy route/widget bundles initial hydration does
//! not need; the on-demand path still fetches any the page actually reaches).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::runtime::Handle;
use tokio::sync::OnceCell;

/// Worker threads backing a job's prewarm runtime. Fetches are IO-bound (async),
/// so a couple of workers drive many concurrent requests; kept small to bound
/// per-job thread creation. Referenced by `tier2::run_job` when it builds the
/// runtime it owns for the job.
pub(crate) const PREWARM_WORKER_THREADS: usize = 2;

/// File-count / total-byte caps on the speculative background walk (mirrors the
/// up-front prefetch budget). The *requested* URL is always served; only closure
/// warming is capped.
const MAX_FILES: usize = 64;
const MAX_TOTAL_BYTES: usize = 12 * 1024 * 1024;

/// A completed subresource fetch. `Ok` bytes on a 2xx; `Err(reason)` otherwise,
/// where the reason is the same short human string `run_job` surfaces as a
/// `runtime.log` line. `Arc`-wrapped so a cache slot is cheap to clone out and
/// share between concurrent awaiters.
type FetchOutcome = Result<Arc<Vec<u8>>, Arc<String>>;

/// One cache slot. The `OnceCell` guarantees the fetch for a URL runs **once**
/// even when the on-demand server and a background warmer race on it, and lets
/// both await the single shared result (dedup + shared await + warm-hit).
type Slot = Arc<OnceCell<FetchOutcome>>;

/// Fetch seam. The production impl wraps `draco_net`; tests inject a mock. Kept
/// object-safe (`async_trait`) and `'static` so background warm tasks can own an
/// `Arc<dyn PrewarmFetch>`.
#[async_trait]
pub(crate) trait PrewarmFetch: Send + Sync + 'static {
    /// Fetch one subresource. `Ok(bytes)` on a 2xx; `Err(reason)` otherwise.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Production [`PrewarmFetch`] — one clamped, cookie-jar-sharing `draco_net`
/// fetch per URL, matching the old `fetch_dynamic_script` contract exactly.
struct NetPrewarmFetch {
    /// Subresource posture (per-fetch timeout clamped, politeness delay dropped)
    /// carrying the job's shared cookie jar — see [`crate::tier2::subresource_opts`].
    opts: draco_net::SessionOpts,
}

#[async_trait]
impl PrewarmFetch for NetPrewarmFetch {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = draco_net::fetch_target(url, &self.opts)
            .await
            .map_err(|e| format!("fetch error: {e:?}"))?;
        let status = resp.meta.status;
        if (200..300).contains(&status) {
            Ok(resp.body.to_vec())
        } else {
            // 403 → challenge/bot-wall (Cloudflare et al.); 404 → a moved/renamed
            // chunk; others verbatim.
            Err(format!("HTTP {status}"))
        }
    }
}

/// Per-job on-demand prewarmer. Cheap to clone (a runtime `Handle` plus `Arc`s);
/// clones share the same cache, runtime, and byte budget, so background warm
/// tasks each hold a clone.
#[derive(Clone)]
pub(crate) struct Prewarmer {
    rt: Handle,
    fetch: Arc<dyn PrewarmFetch>,
    cache: Arc<Mutex<HashMap<String, Slot>>>,
    total_bytes: Arc<AtomicUsize>,
    max_files: usize,
    max_total_bytes: usize,
}

impl Prewarmer {
    /// Build a prewarmer for one Hydrate job, fetching through `draco_net` with
    /// the job's subresource posture + shared cookie jar. `rt` must outlive the
    /// job (its owning runtime is held in `tier2::run_job`).
    pub(crate) fn for_job(rt: Handle, opts: &draco_net::SessionOpts) -> Self {
        Self::with_fetch(
            rt,
            Arc::new(NetPrewarmFetch {
                opts: crate::tier2::subresource_opts(opts),
            }),
        )
    }

    /// Core constructor over an arbitrary fetch seam (production or mock), with
    /// the default budget.
    fn with_fetch(rt: Handle, fetch: Arc<dyn PrewarmFetch>) -> Self {
        Self::build(rt, fetch, MAX_FILES, MAX_TOTAL_BYTES)
    }

    fn build(
        rt: Handle,
        fetch: Arc<dyn PrewarmFetch>,
        max_files: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            rt,
            fetch,
            cache: Arc::new(Mutex::new(HashMap::new())),
            total_bytes: Arc::new(AtomicUsize::new(0)),
            max_files,
            max_total_bytes,
        }
    }

    /// Serve one on-demand `LoadScript`: return the requested URL's bytes (a warm
    /// hit when a prior closure-warm already fetched it, otherwise a direct
    /// fetch), and kick off concurrent warming of its dependency closure so the
    /// *next* request is likely warm.
    ///
    /// Matches the old `fetch_dynamic_script` contract: `Ok(bytes)` on a 2xx,
    /// `Err(reason)` otherwise.
    pub(crate) fn serve(&self, url: &str) -> Result<Vec<u8>, String> {
        // The requested URL is needed *now*, so it is always fetched and bypasses
        // the file cap; only the speculative closure warming below is capped.
        let (slot, is_new) = self.slot(url);
        if is_new {
            self.spawn_warm(url.to_string(), slot.clone());
        }
        match self.rt.block_on(self.init(url, &slot)) {
            Ok(bytes) => Ok((*bytes).clone()),
            Err(reason) => Err((*reason).clone()),
        }
    }

    /// Get-or-create the cache slot for `url`. Returns the slot and whether it was
    /// newly created (the creator is responsible for spawning its warm task).
    fn slot(&self, url: &str) -> (Slot, bool) {
        let mut cache = self.cache.lock().unwrap();
        if let Some(s) = cache.get(url) {
            return (s.clone(), false);
        }
        let s: Slot = Arc::new(OnceCell::new());
        cache.insert(url.to_string(), s.clone());
        (s, true)
    }

    /// Run (once) the fetch that fills a slot. Concurrent callers — the on-demand
    /// server and any background warmer — share the single in-flight fetch via
    /// `OnceCell::get_or_init`, then clone the shared outcome out.
    async fn init(&self, url: &str, slot: &Slot) -> FetchOutcome {
        slot.get_or_init(|| {
            let fetch = self.fetch.clone();
            let url = url.to_string();
            async move {
                match fetch.fetch(&url).await {
                    Ok(bytes) => Ok(Arc::new(bytes)),
                    Err(reason) => Err(Arc::new(reason)),
                }
            }
        })
        .await
        .clone()
    }

    /// Background task: fetch `url`, and on success warm its (bounded) dependency
    /// closure concurrently so subsequent `LoadScript`s become warm hits.
    fn spawn_warm(&self, url: String, slot: Slot) {
        let me = self.clone();
        self.rt.spawn(async move {
            let Ok(bytes) = me.init(&url, &slot).await else {
                return;
            };
            let prev = me.total_bytes.fetch_add(bytes.len(), Ordering::Relaxed);
            if prev.saturating_add(bytes.len()) >= me.max_total_bytes {
                return;
            }
            for child in closure_urls(&url, bytes.as_slice()) {
                me.warm(child);
            }
        });
    }

    /// Speculatively warm one dependency URL — cap-respecting and dedup'd (the
    /// cache map is the visited set, so each URL is fetched at most once).
    fn warm(&self, url: String) {
        {
            let cache = self.cache.lock().unwrap();
            if cache.len() >= self.max_files || cache.contains_key(&url) {
                return;
            }
        }
        if self.total_bytes.load(Ordering::Relaxed) >= self.max_total_bytes {
            return;
        }
        let (slot, is_new) = self.slot(&url);
        if is_new {
            self.spawn_warm(url, slot);
        }
    }
}

/// The dependency closure of one fetched module: its **static** ES-module import
/// specifiers plus webpack/Next chunk-loader candidates, each resolved against
/// the module's own URL and kept only when `http(s)`. Dynamic `import("…")`
/// targets are excluded (see the module docs).
fn closure_urls(module_url: &str, body: &[u8]) -> Vec<String> {
    let Ok(base) = url::Url::parse(module_url) else {
        return Vec::new();
    };
    let src = String::from_utf8_lossy(body);
    let imports = crate::machine::extract_imports(&src);
    let mut out = Vec::new();
    let push = |spec: &str, out: &mut Vec<String>| {
        if let Ok(child) = base.join(spec) {
            if matches!(child.scheme(), "http" | "https") {
                out.push(child.to_string());
            }
        }
    };
    for spec in &imports.statik {
        push(spec, &mut out);
    }
    for spec in crate::machine::extract_chunk_candidates(&src) {
        push(&spec, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// Mock fetcher: serves canned bodies, counts fetches per URL, and records the
    /// max observed in-flight concurrency (a short async sleep makes overlap
    /// observable). A missing URL 404s.
    struct MockFetch {
        bodies: Map<String, String>,
        delay: Duration,
        calls: Arc<StdMutex<Map<String, usize>>>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    impl MockFetch {
        fn make(bodies: &[(&str, &str)], delay_ms: u64) -> Arc<Self> {
            Arc::new(MockFetch {
                bodies: bodies
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                delay: Duration::from_millis(delay_ms),
                calls: Arc::new(StdMutex::new(Map::new())),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
            })
        }
        fn call_count(&self, url: &str) -> usize {
            self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
        }
        fn distinct_calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl PrewarmFetch for MockFetch {
        async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
            *self
                .calls
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_insert(0) += 1;
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            match self.bodies.get(url) {
                Some(b) => Ok(b.clone().into_bytes()),
                None => Err("HTTP 404".to_string()),
            }
        }
    }

    /// A prewarmer + its owning multi-thread runtime. The runtime is returned so
    /// the caller keeps it alive (dropping it cancels background warms), mirroring
    /// how `tier2::run_job` owns the runtime for the job's duration.
    fn harness(mock: Arc<MockFetch>, max_files: usize) -> (tokio::runtime::Runtime, Prewarmer) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(PREWARM_WORKER_THREADS)
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let pw = Prewarmer::build(rt.handle().clone(), mock, max_files, MAX_TOTAL_BYTES);
        (rt, pw)
    }

    const A: &str = "https://cdn.test/app/a.js";
    const B: &str = "https://cdn.test/app/b.js";
    const C: &str = "https://cdn.test/app/c.js";
    const ROOT: &str = "https://cdn.test/app/root.js";

    /// Serving a chunk returns its body and warms its static-import closure so the
    /// children are fetched in the background without anyone asking for them.
    #[test]
    fn serves_body_and_warms_static_closure() {
        let mock = MockFetch::make(
            &[
                (ROOT, "import \"./a.js\";\nimport \"./b.js\";\n"),
                (A, "export const a = 1;"),
                (B, "export const b = 2;"),
            ],
            20,
        );
        let (_rt, pw) = harness(mock.clone(), MAX_FILES);

        let body = pw.serve(ROOT).expect("root served");
        assert_eq!(body, b"import \"./a.js\";\nimport \"./b.js\";\n");

        // Give the background closure warm time to run (runtime stays alive).
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(mock.call_count(ROOT), 1);
        assert_eq!(mock.call_count(A), 1, "a.js should have been warmed");
        assert_eq!(mock.call_count(B), 1, "b.js should have been warmed");
    }

    /// A diamond (root→a,b ; a→c ; b→c) fetches the shared leaf exactly once —
    /// the `OnceCell` cache dedups concurrent warmers.
    #[test]
    fn diamond_closure_dedups_shared_leaf() {
        let mock = MockFetch::make(
            &[
                (ROOT, "import \"./a.js\";\nimport \"./b.js\";"),
                (A, "import \"./c.js\";"),
                (B, "import \"./c.js\";"),
                (C, "export const c = 3;"),
            ],
            20,
        );
        let (_rt, pw) = harness(mock.clone(), MAX_FILES);

        pw.serve(ROOT).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            mock.call_count(C),
            1,
            "shared leaf fetched once, not per-parent"
        );
    }

    /// Warming a fan-out closure overlaps fetches — proof the walk is concurrent,
    /// not the old serial round-trip chain.
    #[test]
    fn closure_warm_runs_concurrently() {
        let mock = MockFetch::make(
            &[
                (
                    ROOT,
                    "import \"./a.js\";\nimport \"./b.js\";\nimport \"./c.js\";",
                ),
                (A, "export const a = 1;"),
                (B, "export const b = 2;"),
                (C, "export const c = 3;"),
            ],
            60,
        );
        let (_rt, pw) = harness(mock.clone(), MAX_FILES);

        pw.serve(ROOT).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            mock.max_in_flight.load(Ordering::SeqCst) >= 2,
            "expected concurrent closure fetches, saw max in-flight {}",
            mock.max_in_flight.load(Ordering::SeqCst)
        );
    }

    /// The file-count cap bounds the speculative walk: root + at most (cap-1)
    /// warmed children.
    #[test]
    fn file_budget_bounds_walk() {
        let mock = MockFetch::make(
            &[
                (
                    ROOT,
                    "import \"./a.js\";\nimport \"./b.js\";\nimport \"./c.js\";\nimport \"./d.js\";",
                ),
                (A, "export const a = 1;"),
                (B, "export const b = 2;"),
                (C, "export const c = 3;"),
                ("https://cdn.test/app/d.js", "export const d = 4;"),
            ],
            10,
        );
        let (_rt, pw) = harness(mock.clone(), 3);

        pw.serve(ROOT).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            mock.distinct_calls() <= 3,
            "file cap should bound the walk, saw {} distinct fetches",
            mock.distinct_calls()
        );
        assert_eq!(mock.call_count(ROOT), 1, "requested URL is always served");
    }

    /// After a chunk is warmed, serving it is a cache hit — the fetch is not
    /// repeated.
    #[test]
    fn serve_after_warm_is_a_cache_hit() {
        let mock = MockFetch::make(
            &[(ROOT, "import \"./a.js\";"), (A, "export const a = 1;")],
            20,
        );
        let (_rt, pw) = harness(mock.clone(), MAX_FILES);

        pw.serve(ROOT).unwrap();
        std::thread::sleep(Duration::from_millis(200)); // let a.js warm
        let a_body = pw.serve(A).expect("a served from warm cache");
        assert_eq!(a_body, b"export const a = 1;");
        assert_eq!(mock.call_count(A), 1, "warmed chunk must not be re-fetched");
    }
}
