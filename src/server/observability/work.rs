//! Bounded, static work and cache accounting for the local diagnostics report.
//!
//! Every count lives in a fixed per-kind bucket; no task name, path, id or
//! growing history is retained. Gauges describe admission and execution
//! honestly: `queued` covers debounce/admission/result-delivery waits and
//! `executing` covers the observed blocking or async body. A cancelled caller
//! that leaves a blocking worker or owned process running increments
//! `cancelled_still_running` until the worker's guard is actually dropped.

use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Fixed work buckets. Adding a bucket is a report schema change, not a
/// per-task allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::server) enum WorkKind {
    Staging,
    Discovery,
    Diagnostics,
    Preparation,
    Lint,
    Compiler,
    SnapshotExport,
    Observation,
}

impl WorkKind {
    pub(in crate::server) const COUNT: usize = 8;

    const fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub(in crate::server) const fn label(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Discovery => "discovery",
            Self::Diagnostics => "diagnostics",
            Self::Preparation => "preparation",
            Self::Lint => "lint",
            Self::Compiler => "compiler",
            Self::SnapshotExport => "snapshotExport",
            Self::Observation => "observation",
        }
    }
}

#[derive(Default)]
struct KindCounters {
    queued: AtomicU64,
    executing: AtomicU64,
    cancelled_still_running: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
}

/// Fixed-bucket work metrics shared through `Arc<WorkMetrics>`.
pub(in crate::server) struct WorkMetrics {
    kinds: [KindCounters; WorkKind::COUNT],
}

impl Default for WorkMetrics {
    fn default() -> Self {
        Self {
            kinds: std::array::from_fn(|_| KindCounters::default()),
        }
    }
}

impl WorkMetrics {
    #[must_use]
    pub(in crate::server) fn new() -> Self {
        Self::default()
    }

    /// Admit one logical work item. The returned guard must be moved into the
    /// future/task that owns the item; dropping it before [`QueuedWork::start`]
    /// records a cancellation that never reached a worker.
    #[must_use]
    pub(in crate::server) fn enqueue(self: &Arc<Self>, kind: WorkKind) -> QueuedWork {
        self.kinds[kind.index()]
            .queued
            .fetch_add(1, Ordering::AcqRel);
        QueuedWork {
            metrics: Arc::clone(self),
            kind,
            active: true,
        }
    }

    /// Cheap scalar gauges/totals for a sampler; never builds a report.
    #[must_use]
    pub(in crate::server) fn totals(&self) -> WorkTotals {
        let mut totals = WorkTotals::default();
        for counters in &self.kinds {
            totals.queued += counters.queued.load(Ordering::Acquire);
            totals.executing += counters.executing.load(Ordering::Acquire);
            totals.cancelled_still_running +=
                counters.cancelled_still_running.load(Ordering::Acquire);
            totals.completed += counters.completed.load(Ordering::Acquire);
            totals.failed += counters.failed.load(Ordering::Acquire);
            totals.cancelled += counters.cancelled.load(Ordering::Acquire);
        }
        totals
    }

    /// Complete bounded aggregate. The vector has exactly [`WorkKind::COUNT`]
    /// entries, ordered by kind index.
    #[must_use]
    pub(in crate::server) fn snapshot(&self) -> WorkSummary {
        let mut by_kind = Vec::with_capacity(WorkKind::COUNT);
        for (index, counters) in self.kinds.iter().enumerate() {
            by_kind.push(WorkKindSummary {
                kind: kind_by_index(index).label(),
                queued: counters.queued.load(Ordering::Acquire),
                executing: counters.executing.load(Ordering::Acquire),
                cancelled_still_running: counters.cancelled_still_running.load(Ordering::Acquire),
                completed: counters.completed.load(Ordering::Acquire),
                failed: counters.failed.load(Ordering::Acquire),
                cancelled: counters.cancelled.load(Ordering::Acquire),
            });
        }
        WorkSummary {
            totals: self.totals(),
            by_kind,
        }
    }
}

fn kind_by_index(index: usize) -> WorkKind {
    match index {
        0 => WorkKind::Staging,
        1 => WorkKind::Discovery,
        2 => WorkKind::Diagnostics,
        3 => WorkKind::Preparation,
        4 => WorkKind::Lint,
        5 => WorkKind::Compiler,
        6 => WorkKind::SnapshotExport,
        _ => WorkKind::Observation,
    }
}

/// Serializable scalar aggregate. Field order is fixed by the report schema.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct WorkTotals {
    pub(in crate::server) queued: u64,
    pub(in crate::server) executing: u64,
    pub(in crate::server) cancelled_still_running: u64,
    pub(in crate::server) completed: u64,
    pub(in crate::server) failed: u64,
    pub(in crate::server) cancelled: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct WorkSummary {
    #[serde(flatten)]
    pub(in crate::server) totals: WorkTotals,
    pub(in crate::server) by_kind: Vec<WorkKindSummary>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct WorkKindSummary {
    pub(in crate::server) kind: &'static str,
    pub(in crate::server) queued: u64,
    pub(in crate::server) executing: u64,
    pub(in crate::server) cancelled_still_running: u64,
    pub(in crate::server) completed: u64,
    pub(in crate::server) failed: u64,
    pub(in crate::server) cancelled: u64,
}

/// Admission guard. Dropping it before the worker starts records one
/// cancellation and releases the `queued` gauge.
#[must_use]
pub(in crate::server) struct QueuedWork {
    metrics: Arc<WorkMetrics>,
    kind: WorkKind,
    active: bool,
}

impl QueuedWork {
    /// Move the item from admission to execution. The returned guard must be
    /// retained until the work actually ends; dropping it earlier is a
    /// cancellation, not a completion.
    #[must_use]
    pub(in crate::server) fn start(mut self) -> RunningWork {
        self.active = false;
        let counters = &self.metrics.kinds[self.kind.index()];
        counters.queued.fetch_sub(1, Ordering::AcqRel);
        counters.executing.fetch_add(1, Ordering::AcqRel);
        RunningWork {
            state: Arc::new(RunningWorkState {
                metrics: Arc::clone(&self.metrics),
                kind: self.kind,
                outcome: std::sync::atomic::AtomicU8::new(RUNNING),
            }),
        }
    }
}

impl Drop for QueuedWork {
    fn drop(&mut self) {
        if self.active {
            let counters = &self.metrics.kinds[self.kind.index()];
            counters.queued.fetch_sub(1, Ordering::AcqRel);
            if std::thread::panicking() {
                counters.failed.fetch_add(1, Ordering::AcqRel);
            } else {
                counters.cancelled.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}

const RUNNING: u8 = 0;
const CANCELLED: u8 = 1;
const FAILED: u8 = 2;
const FINISHED: u8 = 3;

struct RunningWorkState {
    metrics: Arc<WorkMetrics>,
    kind: WorkKind,
    outcome: std::sync::atomic::AtomicU8,
}

impl Drop for RunningWorkState {
    fn drop(&mut self) {
        let counters = &self.metrics.kinds[self.kind.index()];
        counters.executing.fetch_sub(1, Ordering::AcqRel);
        match self.outcome.swap(FINISHED, Ordering::AcqRel) {
            CANCELLED => {
                counters
                    .cancelled_still_running
                    .fetch_sub(1, Ordering::AcqRel);
            }
            FAILED => {
                counters.failed.fetch_add(1, Ordering::AcqRel);
            }
            RUNNING => {
                // A panicking blocking body still releases its guard; classify
                // that unwind as a failure instead of a silent completion.
                if std::thread::panicking() {
                    counters.failed.fetch_add(1, Ordering::AcqRel);
                } else {
                    counters.completed.fetch_add(1, Ordering::AcqRel);
                }
            }
            _ => {}
        }
    }
}

/// Execution guard. Clone it to keep one handle in the caller and one in the
/// blocking worker; the item settles exactly once, when the last clone drops.
#[derive(Clone)]
#[must_use]
pub(in crate::server) struct RunningWork {
    state: Arc<RunningWorkState>,
}

impl RunningWork {
    /// Mark this item cancelled while it is still executing. Idempotent and
    /// race-safe: a late cancellation after settlement is a no-op, and the
    /// first outcome wins.
    pub(in crate::server) fn cancel(&self) {
        let counters = &self.state.metrics.kinds[self.state.kind.index()];
        if self
            .state
            .outcome
            .compare_exchange(RUNNING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            counters
                .cancelled_still_running
                .fetch_add(1, Ordering::AcqRel);
            counters.cancelled.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Mark this item failed. The failure total is recorded when the guard is
    /// actually dropped; a later cancellation cannot overwrite the outcome.
    pub(in crate::server) fn fail(&self) {
        let _ = self.state.outcome.compare_exchange(
            RUNNING,
            FAILED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Cancels the wrapped item when dropped unless disarmed. Use it around an
/// `await` that may be abandoned while an independent blocking worker keeps
/// running: the caller's drop cancels the logical item, while the worker's own
/// [`RunningWork`] clone keeps `executing`/`cancelled_still_running` honest.
#[must_use]
pub(in crate::server) struct CancelOnDrop {
    work: RunningWork,
    armed: bool,
}

impl CancelOnDrop {
    #[must_use]
    pub(in crate::server) fn new(work: RunningWork) -> Self {
        Self { work, armed: true }
    }

    /// Record a normal completion so the drop is not a cancellation.
    pub(in crate::server) fn disarm(&mut self) {
        self.armed = false;
    }

    /// Preserve a failed operation instead of classifying its guard drop as
    /// cancellation. Independent workers remain counted until they exit.
    pub(in crate::server) fn fail(&mut self) {
        self.work.fail();
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            // A dropped future is a cancellation; a panicking owner is a
            // failure. The worker's own clone still keeps the item counted
            // until its blocking body actually ends.
            if std::thread::panicking() {
                self.work.fail();
            } else {
                self.work.cancel();
            }
        }
    }
}

/// Bounded per-cache counters. Hits and misses are recorded at the actual
/// lookup transition; evictions are recorded only by the owning cache when it
/// actually drops an entry. For the semantic cache, exact evictions come from
/// `WorkspaceDb`'s cold-only monotonic counter, not from this type.
#[derive(Default)]
pub(in crate::server) struct CacheMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl CacheMetrics {
    #[must_use]
    pub(in crate::server) fn new() -> Self {
        Self::default()
    }

    pub(in crate::server) fn hit(&self) {
        self.hits.fetch_add(1, Ordering::AcqRel);
    }

    pub(in crate::server) fn miss(&self) {
        self.misses.fetch_add(1, Ordering::AcqRel);
    }

    pub(in crate::server) fn eviction(&self) {
        self.evictions.fetch_add(1, Ordering::AcqRel);
    }

    #[must_use]
    pub(in crate::server) fn totals(&self) -> CacheTotals {
        CacheTotals {
            hits: self.hits.load(Ordering::Acquire),
            misses: self.misses.load(Ordering::Acquire),
            evictions: self.evictions.load(Ordering::Acquire),
        }
    }

    #[must_use]
    pub(in crate::server) fn snapshot(&self) -> CacheTotals {
        self.totals()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CacheTotals {
    pub(in crate::server) hits: u64,
    pub(in crate::server) misses: u64,
    pub(in crate::server) evictions: u64,
}

#[cfg(test)]
mod tests {
    use super::{CacheMetrics, CancelOnDrop, WorkKind, WorkMetrics};
    use std::{
        collections::HashSet,
        sync::{Arc, mpsc},
    };

    fn metrics() -> Arc<WorkMetrics> {
        Arc::new(WorkMetrics::new())
    }

    #[test]
    fn queued_drop_before_start_records_cancellation() {
        let metrics = metrics();
        let queued = metrics.enqueue(WorkKind::Staging);
        assert_eq!(metrics.totals().queued, 1);
        drop(queued);
        let totals = metrics.totals();
        assert_eq!(totals.queued, 0);
        assert_eq!(totals.executing, 0);
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.completed, 0);
        assert_eq!(totals.cancelled_still_running, 0);
    }

    #[test]
    fn started_work_is_executing_until_the_guard_drops() {
        let metrics = metrics();
        let running = metrics.enqueue(WorkKind::Lint).start();
        let totals = metrics.totals();
        assert_eq!(totals.queued, 0);
        assert_eq!(totals.executing, 1);
        assert_eq!(totals.completed, 0);
        drop(running);
        let totals = metrics.totals();
        assert_eq!(totals.executing, 0);
        assert_eq!(totals.completed, 1);
        assert_eq!(totals.cancelled, 0);
    }

    #[test]
    fn cancellation_after_worker_start_keeps_running_until_worker_ends() {
        let metrics = metrics();
        let caller = metrics.enqueue(WorkKind::Discovery).start();
        let worker = caller.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _worker = worker;
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        let _ = started_rx.recv();

        caller.cancel();
        let totals = metrics.totals();
        assert_eq!(totals.executing, 1);
        assert_eq!(totals.cancelled_still_running, 1);
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.completed, 0);

        let _ = release_tx.send(());
        let _ = handle.join();
        let totals = metrics.totals();
        assert_eq!(totals.executing, 0);
        assert_eq!(totals.cancelled_still_running, 0);
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.completed, 0);
    }

    #[test]
    fn late_cancellation_after_worker_clone_drop_is_recorded_once() {
        let metrics = metrics();
        let caller = metrics.enqueue(WorkKind::Compiler).start();
        let worker = caller.clone();
        drop(worker);
        assert_eq!(metrics.totals().executing, 1);

        caller.cancel();
        drop(caller);
        let totals = metrics.totals();
        assert_eq!(totals.executing, 0);
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.cancelled_still_running, 0);
        assert_eq!(totals.completed, 0);
        assert_eq!(totals.failed, 0);
    }

    #[test]
    fn cancelled_then_failed_keeps_the_first_outcome() {
        let metrics = metrics();
        let running = metrics.enqueue(WorkKind::Preparation).start();
        running.cancel();
        running.fail();
        drop(running);
        let totals = metrics.totals();
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.failed, 0);
        assert_eq!(totals.completed, 0);
        assert_eq!(totals.cancelled_still_running, 0);
    }

    #[test]
    fn panicking_worker_is_counted_as_failed() {
        let metrics = metrics();
        let running = metrics.enqueue(WorkKind::Staging).start();
        let handle = std::thread::spawn(move || {
            let _running = running;
            panic!("blocking body failed");
        });
        assert!(handle.join().is_err());
        let totals = metrics.totals();
        assert_eq!(totals.executing, 0);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.completed, 0);
    }

    #[test]
    fn cancel_on_drop_marks_only_an_abandoned_item() {
        let metrics = metrics();
        let running = metrics.enqueue(WorkKind::SnapshotExport).start();
        drop(CancelOnDrop::new(running.clone()));
        assert_eq!(metrics.totals().cancelled_still_running, 1);
        drop(running);
        let totals = metrics.totals();
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.cancelled_still_running, 0);
        assert_eq!(totals.completed, 0);

        let running = metrics.enqueue(WorkKind::Observation).start();
        let mut guard = CancelOnDrop::new(running.clone());
        guard.disarm();
        drop(guard);
        drop(running);
        let totals = metrics.totals();
        assert_eq!(totals.cancelled, 1);
        assert_eq!(totals.completed, 1);
    }

    #[test]
    fn snapshot_has_one_bounded_entry_per_kind() {
        let metrics = metrics();
        let _queued = metrics.enqueue(WorkKind::Lint);
        let _running = metrics.enqueue(WorkKind::Compiler).start();
        let summary = metrics.snapshot();
        assert_eq!(summary.by_kind.len(), WorkKind::COUNT);
        let labels: HashSet<_> = summary.by_kind.iter().map(|kind| kind.kind).collect();
        assert_eq!(labels.len(), WorkKind::COUNT);
        assert_eq!(summary.totals.queued, 1);
        assert_eq!(summary.totals.executing, 1);
        let lint = summary
            .by_kind
            .iter()
            .find(|kind| kind.kind == WorkKind::Lint.label());
        assert!(lint.is_some_and(|lint| lint.queued == 1 && lint.executing == 0));
        let compiler = summary
            .by_kind
            .iter()
            .find(|kind| kind.kind == WorkKind::Compiler.label());
        assert!(compiler.is_some_and(|compiler| compiler.queued == 0 && compiler.executing == 1));
    }

    #[test]
    fn cache_metrics_record_actual_transitions() {
        let cache = CacheMetrics::new();
        cache.hit();
        cache.hit();
        cache.miss();
        cache.eviction();
        let totals = cache.snapshot();
        assert_eq!(totals.hits, 2);
        assert_eq!(totals.misses, 1);
        assert_eq!(totals.evictions, 1);
    }
}
