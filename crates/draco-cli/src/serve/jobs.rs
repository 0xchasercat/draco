//! Shared async-job registry for the long-running daemon endpoints
//! (`POST /v1/crawl`, `POST /v1/batch/scrape`).
//!
//! Both endpoints kick off background work, return an id immediately, and are
//! polled for progress — the same lifecycle (`scraping → completed | failed |
//! cancelled`) and the same Firecrawl-shaped status body. That machinery lives
//! here, once, so crawl and batch scrape share it instead of duplicating it.
//!
//! The registry is **in-memory**: jobs do not survive a daemon restart. The
//! `expiresAt` a status reports is advisory (a 24h window mirroring Firecrawl);
//! there is no background reaper — a restart clears everything regardless.
//!
//! Status bodies paginate exactly like Firecrawl's: `GET …/{id}?skip=&limit=`
//! slices `data`, and a `next` URL is present whenever the job is still running
//! **or** the returned page hit the 10 MiB serialized-size cap with more data
//! behind it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Advisory result-retention window reported as `expiresAt` (Firecrawl uses
/// 24h). The in-memory store has no real TTL; a restart clears it.
const JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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
}

impl Job {
    fn new(total: usize) -> Self {
        Self {
            status: JobStatus::Scraping,
            total,
            completed: 0,
            data: Vec::new(),
            errors: Vec::new(),
            robots_blocked: Vec::new(),
            created: SystemTime::now(),
        }
    }
}

/// In-memory registry of async jobs. Interior mutability so the daemon can
/// share one instance behind `Arc<AppState>`; every critical section is a few
/// field updates — the lock is never held across an await.
pub(crate) struct JobStore {
    jobs: Mutex<HashMap<String, Job>>,
    next_id: AtomicU64,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }
}

impl JobStore {
    /// Register a job seeded with a single admitted unit (a crawl's seed URL);
    /// more are admitted as the frontier grows via [`JobStore::add_admitted`].
    pub(crate) fn create_seeded(&self) -> String {
        self.insert(Job::new(1))
    }

    /// Register a job whose full unit count is known upfront (a batch scrape's
    /// URL list) — nothing further is admitted.
    pub(crate) fn create_with_total(&self, total: usize) -> String {
        self.insert(Job::new(total))
    }

    fn insert(&self, job: Job) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        self.jobs.lock().unwrap().insert(id.clone(), job);
        id
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
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;

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
            "expiresAt": OffsetDateTime::from(job.created + JOB_TTL)
                .format(&Rfc3339)
                .ok(),
            "next": next,
            "data": page,
        }))
    }

    /// The `/errors` body: per-URL failures plus URLs skipped by `robots.txt`
    /// (draco-net signals a robots deny as `NetKind::Robots`, which the worker
    /// routes here via [`JobStore::record_robots_blocked`] rather than `errors`).
    pub(crate) fn errors_snapshot(&self, id: &str) -> Option<Value> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id)?;
        Some(json!({
            "errors": job.errors,
            "robotsBlocked": job.robots_blocked,
        }))
    }

    /// Request cancellation. Returns whether the id existed. Terminal jobs stay
    /// terminal (cancelling a completed job is a no-op beyond the flag check).
    pub(crate) fn cancel(&self, id: &str) -> bool {
        let mut jobs = self.jobs.lock().unwrap();
        match jobs.get_mut(id) {
            Some(job) => {
                if job.status == JobStatus::Scraping {
                    job.status = JobStatus::Cancelled;
                }
                true
            }
            None => false,
        }
    }

    /// True when the job is cancelled — or unknown (a vanished job should stop
    /// its worker just as a cancelled one does).
    pub(crate) fn is_cancelled(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(id)
            .map(|j| j.status == JobStatus::Cancelled)
            .unwrap_or(true)
    }

    /// Record a finished unit: bump `completed`, append its data entry when the
    /// scrape succeeded.
    pub(crate) fn record_page(&self, id: &str, entry: Option<Value>) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.completed += 1;
            if let Some(e) = entry {
                job.data.push(e);
            }
        }
    }

    /// Record a per-URL failure (surfaced by the `/errors` endpoint). Does not
    /// bump `completed` — the caller pairs this with [`JobStore::record_page`]
    /// when the unit is also "done".
    pub(crate) fn record_error(&self, id: &str, url: &str, error: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.errors.push(json!({ "url": url, "error": error }));
        }
    }

    /// Record a URL skipped due to `robots.txt` (surfaced under `robotsBlocked`
    /// by the `/errors` endpoint). Pairs with [`JobStore::record_page`]`(None)`
    /// so the unit still counts as processed.
    pub(crate) fn record_robots_blocked(&self, id: &str, url: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.robots_blocked.push(url.to_string());
        }
    }

    /// Record newly admitted frontier URLs (crawl only; batch knows `total`
    /// upfront).
    pub(crate) fn add_admitted(&self, id: &str, n: usize) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.total += n;
        }
    }

    /// Transition a drained job to its terminal state (unless cancelled, which
    /// is sticky). `Completed` when any data was collected, else `Failed`.
    /// Returns the resulting terminal status (so the caller can fire the
    /// matching webhook event); `None` for an unknown id.
    pub(crate) fn finish(&self, id: &str) -> Option<JobStatus> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs.get_mut(id)?;
        if job.status == JobStatus::Scraping {
            job.status = if job.data.is_empty() && !job.errors.is_empty() {
                JobStatus::Failed
            } else {
                JobStatus::Completed
            };
        }
        Some(job.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_slices_and_sets_next_while_running() {
        let store = JobStore::default();
        let id = store.create_with_total(3);
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
        let id = store.create_with_total(1);
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
        let id = store.create_with_total(2);
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
        let id = store.create_with_total(1);
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
