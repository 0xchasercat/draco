//! Shared async-job registry for the long-running daemon endpoints
//! (`POST /v1/crawl`, `POST /v1/batch/scrape`).
//!
//! Both endpoints kick off background work, return an id immediately, and are
//! polled for progress — the same lifecycle (`scraping → completed | failed |
//! cancelled`) and the same Firecrawl-shaped status body. That machinery lives
//! here, once, so crawl and batch scrape share it instead of duplicating it.
//!
//! The registry is **in-memory**: jobs do not survive a daemon restart. Jobs are
//! retained until the reported 24h `expiresAt`, subject to bounded job-count and
//! terminal-payload caps. The daemon's maintenance loop reaps expired entries.
//!
//! Status bodies paginate exactly like Firecrawl's: `GET …/{id}?skip=&limit=`
//! slices `data`, and a `next` URL is present whenever the job is still running
//! **or** the returned page hit the 10 MiB serialized-size cap with more data
//! behind it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Result-retention window reported as `expiresAt` and enforced by the daemon.
const JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum number of running + terminal jobs retained per registry.
const MAX_JOBS: usize = 1024;

/// Maximum serialized payload bytes retained across terminal/running jobs.
/// Running jobs are never evicted, so a running-only excess is allowed until it
/// becomes terminal; older terminal jobs are still reclaimed immediately.
const MAX_TERMINAL_RETAINED_BYTES: usize = 256 * 1024 * 1024;

/// A status page stops accumulating `data` once its serialized size reaches
/// this cap; the remainder is fetched via `next` (Firecrawl's exact 10 MiB).
const PAGE_BYTE_CAP: usize = 10 * 1024 * 1024;

/// Lifecycle of an async job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobStatus {
    Scraping,
    Completed,
    Cancelled,
    Failed,
}

impl JobStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            JobStatus::Scraping => "scraping",
            JobStatus::Completed => "completed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed => "failed",
        }
    }
}

/// One job's observable state. `total` counts units of work (crawl: URLs
/// admitted to the frontier, bounded by `limit`; batch: the URL count, known
/// upfront); `completed` counts finished units; `data` holds successful page
/// payloads in completion order; `errors`/`robots_blocked` back the `/errors`
/// endpoint.
struct Job {
    status: JobStatus,
    total: usize,
    completed: usize,
    data: Vec<Value>,
    errors: Vec<Value>,
    robots_blocked: Vec<String>,
    created: SystemTime,
    ordinal: u64,
    retained_bytes: usize,
}

impl Job {
    fn new(total: usize, created: SystemTime, ordinal: u64) -> Self {
        Self {
            status: JobStatus::Scraping,
            total,
            completed: 0,
            data: Vec::new(),
            errors: Vec::new(),
            robots_blocked: Vec::new(),
            created,
            ordinal,
            retained_bytes: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobCapacityError;

impl std::fmt::Display for JobCapacityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("async job capacity exhausted")
    }
}

impl std::error::Error for JobCapacityError {}

/// Logical ownership counters surfaced through daemon health diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStoreStats {
    pub(crate) jobs: usize,
    pub(crate) running: usize,
    pub(crate) retained_bytes: usize,
}

struct StoreInner {
    jobs: HashMap<JobKey, Job>,
    retained_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum JobKind {
    Standalone,
    Crawl,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct JobKey {
    kind: JobKind,
    id: String,
}

struct SharedJobStore {
    inner: Mutex<StoreInner>,
    next_id: AtomicU64,
    max_jobs: usize,
    max_retained_bytes: usize,
}

/// In-memory registry of async jobs. Interior mutability so the daemon can
/// share one instance behind `Arc<AppState>`; every critical section is a few
/// field updates — the lock is never held across an await.
#[derive(Clone)]
pub(crate) struct JobStore {
    shared: Arc<SharedJobStore>,
    kind: JobKind,
}

impl Default for JobStore {
    fn default() -> Self {
        Self::new_with_kind(
            JobKind::Standalone,
            Arc::new(SharedJobStore {
                inner: Mutex::new(StoreInner {
                    jobs: HashMap::new(),
                    retained_bytes: 0,
                }),
                next_id: AtomicU64::new(1),
                max_jobs: MAX_JOBS,
                max_retained_bytes: MAX_TERMINAL_RETAINED_BYTES,
            }),
        )
    }
}

impl JobStore {
    fn new_with_kind(kind: JobKind, shared: Arc<SharedJobStore>) -> Self {
        Self { shared, kind }
    }

    /// Crawl and batch routes use distinct namespaces over one process-wide
    /// capacity/accounting coordinator.
    pub(crate) fn shared_pair() -> (Self, Self) {
        Self::shared_pair_with_limits(MAX_JOBS, MAX_TERMINAL_RETAINED_BYTES)
    }

    fn shared_pair_with_limits(max_jobs: usize, max_retained_bytes: usize) -> (Self, Self) {
        let shared = Arc::new(SharedJobStore {
            inner: Mutex::new(StoreInner {
                jobs: HashMap::new(),
                retained_bytes: 0,
            }),
            next_id: AtomicU64::new(1),
            max_jobs,
            max_retained_bytes,
        });
        (
            Self::new_with_kind(JobKind::Crawl, shared.clone()),
            Self::new_with_kind(JobKind::Batch, shared),
        )
    }

    /// Register a job seeded with a single admitted unit (a crawl's seed URL);
    /// more are admitted as the frontier grows via [`JobStore::add_admitted`].
    pub(crate) fn create_seeded(&self) -> Result<String, JobCapacityError> {
        self.insert(1, SystemTime::now())
    }

    /// Register a job whose full unit count is known upfront (a batch scrape's
    /// URL list) — nothing further is admitted.
    pub(crate) fn create_with_total(&self, total: usize) -> Result<String, JobCapacityError> {
        self.insert(total, SystemTime::now())
    }

    fn insert(&self, total: usize, created: SystemTime) -> Result<String, JobCapacityError> {
        let mut inner = self.shared.inner.lock().unwrap();
        while inner.jobs.len() >= self.shared.max_jobs {
            if !evict_oldest_terminal(&mut inner) {
                return Err(JobCapacityError);
            }
        }
        let ordinal = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let id = ordinal.to_string();
        inner.jobs.insert(
            JobKey {
                kind: self.kind,
                id: id.clone(),
            },
            Job::new(total, created, ordinal),
        );
        Ok(id)
    }

    #[cfg(test)]
    fn with_limits_for_test(max_jobs: usize, max_retained_bytes: usize) -> Self {
        Self::new_with_kind(
            JobKind::Standalone,
            Arc::new(SharedJobStore {
                inner: Mutex::new(StoreInner {
                    jobs: HashMap::new(),
                    retained_bytes: 0,
                }),
                next_id: AtomicU64::new(1),
                max_jobs,
                max_retained_bytes,
            }),
        )
    }

    #[cfg(test)]
    pub(crate) fn shared_pair_with_limits_for_test(
        max_jobs: usize,
        max_retained_bytes: usize,
    ) -> (Self, Self) {
        Self::shared_pair_with_limits(max_jobs, max_retained_bytes)
    }

    #[cfg(test)]
    fn create_with_total_at_for_test(
        &self,
        total: usize,
        created: SystemTime,
    ) -> Result<String, JobCapacityError> {
        self.insert(total, created)
    }

    /// Remove jobs at the same boundary reported in `expiresAt`.
    pub(crate) fn reap_expired(&self, now: SystemTime) -> usize {
        let mut inner = self.shared.inner.lock().unwrap();
        let before = inner.jobs.len();
        let mut removed_bytes = 0usize;
        inner.jobs.retain(|key, job| {
            if key.kind != self.kind {
                return true;
            }
            let expired = job
                .created
                .checked_add(JOB_TTL)
                .map(|expires| expires <= now)
                .unwrap_or(false);
            if expired {
                removed_bytes = removed_bytes.saturating_add(job.retained_bytes);
            }
            !expired
        });
        inner.retained_bytes = inner.retained_bytes.saturating_sub(removed_bytes);
        before.saturating_sub(inner.jobs.len())
    }

    pub(crate) fn stats(&self) -> JobStoreStats {
        let inner = self.shared.inner.lock().unwrap();
        JobStoreStats {
            jobs: inner
                .jobs
                .keys()
                .filter(|key| key.kind == self.kind)
                .count(),
            running: inner
                .jobs
                .iter()
                .filter(|(key, job)| key.kind == self.kind && job.status == JobStatus::Scraping)
                .count(),
            retained_bytes: inner
                .jobs
                .iter()
                .filter(|(key, _)| key.kind == self.kind)
                .fold(0usize, |total, (_, job)| {
                    total.saturating_add(job.retained_bytes)
                }),
        }
    }

    pub(crate) fn global_stats(&self) -> JobStoreStats {
        let inner = self.shared.inner.lock().unwrap();
        JobStoreStats {
            jobs: inner.jobs.len(),
            running: inner
                .jobs
                .values()
                .filter(|job| job.status == JobStatus::Scraping)
                .count(),
            retained_bytes: inner.retained_bytes,
        }
    }

    fn enforce_payload_cap(&self, inner: &mut StoreInner) {
        while terminal_retained_bytes(inner) > self.shared.max_retained_bytes {
            if !evict_oldest_terminal(inner) {
                break;
            }
        }
    }

    /// Snapshot a job as its Firecrawl status body, paginated. `data` is sliced
    /// `[skip..]` up to `limit` items and the 10 MiB byte cap; `next` points at
    /// the next slice when the job is still running or more data remains.
    /// `next_base` is the id's status path (e.g. `/v1/batch/scrape/7`), to which
    /// the `?skip=&limit=` query is appended. `None` for unknown ids.
    pub(crate) fn snapshot(
        &self,
        id: &str,
        skip: usize,
        limit: Option<usize>,
        next_base: &str,
    ) -> Option<Value> {
        let inner = self.shared.inner.lock().unwrap();
        let job = inner.jobs.get(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        })?;

        let mut page = Vec::new();
        let mut bytes = 0usize;
        let mut i = skip.min(job.data.len());
        while i < job.data.len() {
            if let Some(l) = limit {
                if page.len() >= l {
                    break;
                }
            }
            let item = &job.data[i];
            let sz = serde_json::to_string(item).map(|s| s.len()).unwrap_or(0);
            // Always return at least one item, then stop before exceeding the cap.
            if !page.is_empty() && bytes + sz > PAGE_BYTE_CAP {
                break;
            }
            bytes += sz;
            page.push(item.clone());
            i += 1;
        }

        let more = i < job.data.len();
        let still_running = job.status == JobStatus::Scraping;
        let next = (more || still_running).then(|| {
            let mut u = format!("{next_base}?skip={i}");
            if let Some(l) = limit {
                u.push_str(&format!("&limit={l}"));
            }
            Value::String(u)
        });

        Some(json!({
            "success": true,
            "status": job.status.as_str(),
            "total": job.total,
            "completed": job.completed,
            "creditsUsed": job.completed,
            "expiresAt": job.created.checked_add(JOB_TTL).and_then(|expires| {
                OffsetDateTime::from(expires).format(&Rfc3339).ok()
            }),
            "next": next,
            "data": page,
        }))
    }

    /// The `/errors` body: per-URL failures plus URLs skipped by `robots.txt`
    /// (draco-net signals a robots deny as `NetKind::Robots`, which the worker
    /// routes here via [`JobStore::record_robots_blocked`] rather than `errors`).
    pub(crate) fn errors_snapshot(&self, id: &str) -> Option<Value> {
        let inner = self.shared.inner.lock().unwrap();
        let job = inner.jobs.get(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        })?;
        Some(json!({
            "errors": job.errors,
            "robotsBlocked": job.robots_blocked,
        }))
    }

    /// Request cancellation. Returns whether the id existed. Terminal jobs stay
    /// terminal (cancelling a completed job is a no-op beyond the flag check).
    pub(crate) fn cancel(&self, id: &str) -> bool {
        let mut inner = self.shared.inner.lock().unwrap();
        let key = JobKey {
            kind: self.kind,
            id: id.to_string(),
        };
        let found = match inner.jobs.get_mut(&key) {
            Some(job) => {
                if job.status == JobStatus::Scraping {
                    job.status = JobStatus::Cancelled;
                }
                true
            }
            None => false,
        };
        self.enforce_payload_cap(&mut inner);
        found
    }

    /// True when the job is cancelled — or unknown (a vanished job should stop
    /// its worker just as a cancelled one does).
    pub(crate) fn is_cancelled(&self, id: &str) -> bool {
        let inner = self.shared.inner.lock().unwrap();
        inner
            .jobs
            .get(&JobKey {
                kind: self.kind,
                id: id.to_string(),
            })
            .map(|j| j.status == JobStatus::Cancelled)
            .unwrap_or(true)
    }

    /// Record a finished unit: bump `completed`, append its data entry when the
    /// scrape succeeded.
    pub(crate) fn record_page(&self, id: &str, entry: Option<Value>) {
        let mut inner = self.shared.inner.lock().unwrap();
        let mut added = 0usize;
        if let Some(job) = inner.jobs.get_mut(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        }) {
            job.completed += 1;
            if let Some(e) = entry {
                added = serialized_bytes(&e);
                job.retained_bytes = job.retained_bytes.saturating_add(added);
                job.data.push(e);
            }
        }
        inner.retained_bytes = inner.retained_bytes.saturating_add(added);
        self.enforce_payload_cap(&mut inner);
    }

    /// Record a per-URL failure (surfaced by the `/errors` endpoint). Does not
    /// bump `completed` — the caller pairs this with [`JobStore::record_page`]
    /// when the unit is also "done".
    pub(crate) fn record_error(&self, id: &str, url: &str, error: &str) {
        let mut inner = self.shared.inner.lock().unwrap();
        let value = json!({ "url": url, "error": error });
        let added = serialized_bytes(&value);
        let mut retained = false;
        if let Some(job) = inner.jobs.get_mut(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        }) {
            job.retained_bytes = job.retained_bytes.saturating_add(added);
            job.errors.push(value);
            retained = true;
        }
        if retained {
            inner.retained_bytes = inner.retained_bytes.saturating_add(added);
        }
        self.enforce_payload_cap(&mut inner);
    }

    /// Record a URL skipped due to `robots.txt` (surfaced under `robotsBlocked`
    /// by the `/errors` endpoint). Pairs with [`JobStore::record_page`]`(None)`
    /// so the unit still counts as processed.
    pub(crate) fn record_robots_blocked(&self, id: &str, url: &str) {
        let mut inner = self.shared.inner.lock().unwrap();
        let added = serialized_string_bytes(url);
        let mut retained = false;
        if let Some(job) = inner.jobs.get_mut(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        }) {
            job.retained_bytes = job.retained_bytes.saturating_add(added);
            job.robots_blocked.push(url.to_string());
            retained = true;
        }
        if retained {
            inner.retained_bytes = inner.retained_bytes.saturating_add(added);
        }
        self.enforce_payload_cap(&mut inner);
    }

    /// Record newly admitted frontier URLs (crawl only; batch knows `total`
    /// upfront).
    pub(crate) fn add_admitted(&self, id: &str, n: usize) {
        let mut inner = self.shared.inner.lock().unwrap();
        if let Some(job) = inner.jobs.get_mut(&JobKey {
            kind: self.kind,
            id: id.to_string(),
        }) {
            job.total += n;
        }
    }

    /// Transition a drained job to its terminal state (unless cancelled, which
    /// is sticky). `Completed` when any data was collected, else `Failed`.
    /// Returns the resulting terminal status (so the caller can fire the
    /// matching webhook event); `None` for an unknown id.
    pub(crate) fn finish(&self, id: &str) -> Option<JobStatus> {
        let mut inner = self.shared.inner.lock().unwrap();
        let status = {
            let job = inner.jobs.get_mut(&JobKey {
                kind: self.kind,
                id: id.to_string(),
            })?;
            if job.status == JobStatus::Scraping {
                job.status = if job.data.is_empty() && !job.errors.is_empty() {
                    JobStatus::Failed
                } else {
                    JobStatus::Completed
                };
            }
            job.status
        };
        self.enforce_payload_cap(&mut inner);
        Some(status)
    }
}

fn serialized_bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

fn serialized_string_bytes(value: &str) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

fn evict_oldest_terminal(inner: &mut StoreInner) -> bool {
    let victim = inner
        .jobs
        .iter()
        .filter(|(_, job)| job.status != JobStatus::Scraping)
        .min_by_key(|(_, job)| (job.created, job.ordinal))
        .map(|(key, _)| key.clone());
    let Some(victim) = victim else {
        return false;
    };
    if let Some(job) = inner.jobs.remove(&victim) {
        inner.retained_bytes = inner.retained_bytes.saturating_sub(job.retained_bytes);
        true
    } else {
        false
    }
}

fn terminal_retained_bytes(inner: &StoreInner) -> usize {
    inner
        .jobs
        .values()
        .filter(|job| job.status != JobStatus::Scraping)
        .fold(0usize, |total, job| {
            total.saturating_add(job.retained_bytes)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn reap_expired_matches_reported_expiry_boundary() {
        let store = JobStore::with_limits_for_test(8, usize::MAX);
        let created = at(1_000_000);
        let id = store.create_with_total_at_for_test(1, created).unwrap();
        let snap = store.snapshot(&id, 0, None, "/job/1").unwrap();
        let expected_expiry = OffsetDateTime::from(created + JOB_TTL)
            .format(&Rfc3339)
            .unwrap();
        assert_eq!(snap["expiresAt"], expected_expiry);

        assert_eq!(
            store.reap_expired(created + JOB_TTL - Duration::from_secs(1)),
            0
        );
        assert!(store.snapshot(&id, 0, None, "/job/1").is_some());
        assert_eq!(store.reap_expired(created + JOB_TTL), 1);
        assert!(store.snapshot(&id, 0, None, "/job/1").is_none());
    }

    #[test]
    fn count_cap_evicts_oldest_terminal_job() {
        let store = JobStore::with_limits_for_test(3, usize::MAX);
        let oldest = store.create_with_total_at_for_test(1, at(10)).unwrap();
        store.finish(&oldest);
        let newer = store.create_with_total_at_for_test(1, at(20)).unwrap();
        store.finish(&newer);
        let running = store.create_with_total_at_for_test(1, at(30)).unwrap();

        let admitted = store.create_with_total_at_for_test(1, at(40)).unwrap();

        assert!(store.snapshot(&oldest, 0, None, "/oldest").is_none());
        assert!(store.snapshot(&newer, 0, None, "/newer").is_some());
        assert!(store.snapshot(&running, 0, None, "/running").is_some());
        assert!(store.snapshot(&admitted, 0, None, "/admitted").is_some());
    }

    #[test]
    fn byte_cap_evicts_oldest_terminal_payload() {
        let entry = json!({ "markdown": "12345678901234567890" });
        let entry_bytes = serialized_bytes(&entry);
        let store = JobStore::with_limits_for_test(8, entry_bytes + 4);

        let oldest = store.create_with_total_at_for_test(1, at(10)).unwrap();
        store.record_page(&oldest, Some(entry.clone()));
        store.finish(&oldest);

        let newer = store.create_with_total_at_for_test(1, at(20)).unwrap();
        store.record_page(&newer, Some(entry));
        store.finish(&newer);

        assert!(store.snapshot(&oldest, 0, None, "/oldest").is_none());
        assert!(store.snapshot(&newer, 0, None, "/newer").is_some());
        assert!(store.stats().retained_bytes <= entry_bytes + 4);
    }

    #[test]
    fn all_running_capacity_returns_error_without_evicting() {
        let store = JobStore::with_limits_for_test(3, usize::MAX);
        let first = store.create_with_total(1).unwrap();
        let second = store.create_with_total(1).unwrap();
        let third = store.create_with_total(1).unwrap();

        assert_eq!(store.create_seeded(), Err(JobCapacityError));
        for id in [&first, &second, &third] {
            assert!(store.snapshot(id, 0, None, "/running").is_some());
        }
        assert_eq!(
            store.stats(),
            JobStoreStats {
                jobs: 3,
                running: 3,
                retained_bytes: 0,
            }
        );
    }

    #[test]
    fn running_job_is_never_evicted_to_satisfy_byte_cap() {
        let entry = json!({ "markdown": "payload larger than the cap" });
        let bytes = serialized_bytes(&entry);
        let store = JobStore::with_limits_for_test(4, bytes.saturating_sub(1));
        let id = store.create_with_total(1).unwrap();

        store.record_page(&id, Some(entry));

        assert!(store.snapshot(&id, 0, None, "/running").is_some());
        assert_eq!(store.stats().jobs, 1);
        assert_eq!(store.stats().running, 1);
        assert_eq!(store.stats().retained_bytes, bytes);
    }

    #[test]
    fn stats_account_for_data_errors_robots_and_removal() {
        let store = JobStore::with_limits_for_test(8, usize::MAX);
        let created = at(50);
        let id = store.create_with_total_at_for_test(3, created).unwrap();
        let data = json!({ "markdown": "ok" });
        let error = json!({ "url": "https://x.test/a", "error": "boom" });
        let robot = "https://x.test/blocked";

        store.record_page(&id, Some(data.clone()));
        store.record_error(&id, "https://x.test/a", "boom");
        store.record_robots_blocked(&id, robot);
        let expected =
            serialized_bytes(&data) + serialized_bytes(&error) + serialized_string_bytes(robot);
        assert_eq!(
            store.stats(),
            JobStoreStats {
                jobs: 1,
                running: 1,
                retained_bytes: expected,
            }
        );

        store.finish(&id);
        assert_eq!(store.stats().running, 0);
        assert_eq!(store.reap_expired(created + JOB_TTL), 1);
        assert_eq!(store.stats().retained_bytes, 0);
    }

    #[test]
    fn shared_namespaces_enforce_one_count_cap_and_cross_kind_eviction() {
        let (crawl, batch) = JobStore::shared_pair_with_limits_for_test(3, usize::MAX);
        let old_crawl = crawl.create_with_total_at_for_test(1, at(10)).unwrap();
        crawl.finish(&old_crawl);
        let newer_batch = batch.create_with_total_at_for_test(1, at(20)).unwrap();
        batch.finish(&newer_batch);
        let running_crawl = crawl.create_with_total_at_for_test(1, at(30)).unwrap();

        let admitted_batch = batch.create_with_total_at_for_test(1, at(40)).unwrap();

        assert!(crawl.snapshot(&old_crawl, 0, None, "/crawl").is_none());
        assert!(batch.snapshot(&newer_batch, 0, None, "/batch").is_some());
        assert!(crawl.snapshot(&running_crawl, 0, None, "/crawl").is_some());
        assert!(batch.snapshot(&admitted_batch, 0, None, "/batch").is_some());
        assert!(crawl
            .snapshot(&newer_batch, 0, None, "/wrong-kind")
            .is_none());
        assert_eq!(crawl.global_stats().jobs, 3);
        assert_eq!(batch.global_stats().jobs, 3);
    }

    #[test]
    fn shared_namespaces_reject_combined_all_running_capacity() {
        let (crawl, batch) = JobStore::shared_pair_with_limits_for_test(3, usize::MAX);
        let crawl_a = crawl.create_seeded().unwrap();
        let batch_a = batch.create_with_total(1).unwrap();
        let crawl_b = crawl.create_seeded().unwrap();

        assert_eq!(batch.create_with_total(1), Err(JobCapacityError));
        assert!(crawl.snapshot(&crawl_a, 0, None, "/crawl").is_some());
        assert!(batch.snapshot(&batch_a, 0, None, "/batch").is_some());
        assert!(crawl.snapshot(&crawl_b, 0, None, "/crawl").is_some());
        assert_eq!(crawl.global_stats().running, 3);
    }

    #[test]
    fn shared_namespaces_enforce_one_terminal_payload_cap() {
        let entry = json!({ "markdown": "cross namespace payload" });
        let bytes = serialized_bytes(&entry);
        let (crawl, batch) = JobStore::shared_pair_with_limits_for_test(8, bytes + 2);
        let crawl_id = crawl.create_with_total_at_for_test(1, at(10)).unwrap();
        crawl.record_page(&crawl_id, Some(entry.clone()));
        crawl.finish(&crawl_id);
        let batch_id = batch.create_with_total_at_for_test(1, at(20)).unwrap();
        batch.record_page(&batch_id, Some(entry));
        batch.finish(&batch_id);

        assert!(crawl.snapshot(&crawl_id, 0, None, "/crawl").is_none());
        assert!(batch.snapshot(&batch_id, 0, None, "/batch").is_some());
        assert_eq!(batch.global_stats().retained_bytes, bytes);
    }

    #[test]
    fn pagination_slices_and_sets_next_while_running() {
        let store = JobStore::default();
        let id = store.create_with_total(3).unwrap();
        store.record_page(&id, Some(json!({ "markdown": "a" })));
        store.record_page(&id, Some(json!({ "markdown": "b" })));

        // Running job with a limit smaller than the data → next is present.
        let snap = store
            .snapshot(&id, 0, Some(1), "/v1/batch/scrape/1")
            .unwrap();
        assert_eq!(snap["status"], "scraping");
        assert_eq!(snap["data"].as_array().unwrap().len(), 1);
        assert_eq!(snap["next"], "/v1/batch/scrape/1?skip=1&limit=1");
        assert_eq!(snap["completed"], 2);
        assert_eq!(snap["total"], 3);
    }

    #[test]
    fn completed_job_without_more_data_has_null_next() {
        let store = JobStore::default();
        let id = store.create_with_total(1).unwrap();
        store.record_page(&id, Some(json!({ "markdown": "only" })));
        store.finish(&id);
        let snap = store.snapshot(&id, 0, None, "/v1/batch/scrape/1").unwrap();
        assert_eq!(snap["status"], "completed");
        assert!(snap["next"].is_null());
        assert_eq!(snap["data"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn errors_snapshot_carries_failures_and_robots() {
        let store = JobStore::default();
        let id = store.create_with_total(2).unwrap();
        store.record_error(&id, "https://x.test/a", "boom");
        store.record_robots_blocked(&id, "https://x.test/b");
        let e = store.errors_snapshot(&id).unwrap();
        assert_eq!(e["errors"][0]["url"], "https://x.test/a");
        assert_eq!(e["errors"][0]["error"], "boom");
        assert_eq!(e["robotsBlocked"][0], "https://x.test/b");
    }

    #[test]
    fn empty_job_with_errors_finishes_failed() {
        let store = JobStore::default();
        let id = store.create_with_total(1).unwrap();
        store.record_error(&id, "https://x.test/a", "boom");
        store.record_page(&id, None);
        store.finish(&id);
        let snap = store.snapshot(&id, 0, None, "/v1/batch/scrape/1").unwrap();
        assert_eq!(snap["status"], "failed");
    }

    #[test]
    fn unknown_id_snapshots_none() {
        let store = JobStore::default();
        assert!(store.snapshot("999", 0, None, "/x").is_none());
        assert!(store.errors_snapshot("999").is_none());
        assert!(!store.cancel("999"));
    }
}
