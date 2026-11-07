//! Bounded, local-only accounting. No source, request payload or path is stored
//! in the report. Detailed tracing retains its separate opt-in lifecycle.

mod buffers;
mod commands;
pub(super) mod compiler;
mod export;
mod history;
mod process;
mod report;
pub(super) mod requests;
pub(super) mod work;

use super::{
    compiler_service::CompilerService, diagnostics::DiagnosticsService,
    orchestration::run_staging, state::State, workspace_service::WorkspaceService,
};
use crate::storage::StorageRegistry;
use history::{History, HistorySample, SAMPLE_INTERVAL_SECONDS};
use process::{ProcessMemoryError, ProcessMemoryObserver};
use requests::RequestMetrics;
use std::{
    io,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore, watch},
    task::JoinHandle,
};
use work::{CacheMetrics, WorkKind, WorkMetrics};

pub(super) const EXPORT_COMMAND: &str = "bend2.diagnostics.export";
pub(super) const EXPORT_KIND: &str = "source.bend2Diagnostics.export";

#[derive(Clone)]
struct Sources {
    workspace: Weak<WorkspaceService>,
    compiler: Weak<CompilerService>,
    diagnostics: Weak<DiagnosticsService>,
}

pub(super) struct Observability {
    pub(super) storage: StorageRegistry,
    pub(super) requests: Arc<RequestMetrics>,
    pub(super) work: Arc<WorkMetrics>,
    pub(super) semantic_cache: CacheMetrics,
    pub(super) compiler_cache: CacheMetrics,
    started: Instant,
    sources: State<Option<Sources>>,
    history: State<History>,
    process: Mutex<Option<ProcessMemoryObserver>>,
    observation_slots: Arc<Semaphore>,
    stop: watch::Sender<bool>,
    sampler: AsyncMutex<Option<JoinHandle<()>>>,
}

impl Default for Observability {
    fn default() -> Self {
        Self {
            storage: StorageRegistry::default(),
            requests: Arc::new(RequestMetrics::default()),
            work: Arc::new(WorkMetrics::default()),
            semantic_cache: CacheMetrics::default(),
            compiler_cache: CacheMetrics::default(),
            started: Instant::now(),
            sources: State::new(None),
            history: State::new(History::default()),
            process: Mutex::new(None),
            observation_slots: Arc::new(Semaphore::new(1)),
            stop: watch::channel(false).0,
            sampler: AsyncMutex::new(None),
        }
    }
}

impl Observability {
    pub(super) fn attach_sources(
        &self,
        workspace: &Arc<WorkspaceService>,
        compiler: &Arc<CompilerService>,
        diagnostics: &Arc<DiagnosticsService>,
    ) {
        *self.sources.write() = Some(Sources {
            workspace: Arc::downgrade(workspace),
            compiler: Arc::downgrade(compiler),
            diagnostics: Arc::downgrade(diagnostics),
        });
    }

    /// Uses the existing cancellation-aware blocking-work boundary. A cancelled
    /// caller cannot hide a still-running observation worker.
    pub(super) async fn blocking<T, F>(&self, kind: WorkKind, operation: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        run_staging(
            self.work.clone(),
            kind,
            self.observation_slots.clone(),
            operation,
        )
        .await
    }

    fn uptime_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn self_memory(&self) -> report::MemoryObservation {
        let pid = std::process::id();
        let result = self
            .process
            .lock()
            .map_err(|_| ProcessMemoryError::SamplingFailed)
            .and_then(|mut retained| {
                if let Some(observer) = retained.as_ref() {
                    observer.sample()
                } else {
                    let observer = ProcessMemoryObserver::capture(pid)?;
                    let sample = observer.sample();
                    *retained = Some(observer);
                    sample
                }
            });
        report::MemoryObservation::from_result(pid, result)
    }

    fn history_sample(&self) -> io::Result<HistorySample> {
        let workspace = self
            .sources
            .read()
            .as_ref()
            .and_then(|sources| sources.workspace.upgrade())
            .ok_or_else(unavailable_state)?;
        // No workspace guard survives the OS call below. Capacity accounting
        // reads indexes/metadata, never the full source text.
        let usage = workspace.read().diagnostic_usage();
        let storage = self.storage.snapshot();
        let work = self.work.totals();
        let memory = self.self_memory();
        Ok(HistorySample {
            uptime_ms: self.uptime_ms(),
            resident_bytes: memory.resident_bytes(),
            source_bytes: storage.snapshots.source_bytes,
            indexed_buffer_bytes: storage
                .snapshots
                .line_index_bytes
                .saturating_add(storage.snapshots.syntax_bytes)
                .saturating_add(storage.semantic_views.owned_backing_bytes)
                .saturating_add(usage.workspace_index_bytes),
            retained_snapshots: storage.snapshots.live_snapshots,
            open_documents: usage.open_documents,
            indexed_documents: usage.indexed_documents,
            active_calls: self.requests.active_requests(),
            queued_calls: self.requests.requests_awaiting_first_poll(),
            active_work: work.executing,
            queued_work: work.queued,
            cancelled_work_still_running: work.cancelled_still_running,
        })
    }

    /// Runtime startup is explicit: constructing a workspace outside Tokio does
    /// not spawn background tasks, and the task owns only a weak server handle.
    pub(super) async fn start_sampler(self: &Arc<Self>) {
        let mut sampler = self.sampler.lock().await;
        if sampler.is_some() || *self.stop.borrow() {
            return;
        }
        let observation = Arc::downgrade(self);
        let mut stopped = self.stop.subscribe();
        let diagnostics = self.sources.read().as_ref().map(|sources| sources.diagnostics.clone());
        let operation = async move {
            let mut interval = tokio::time::interval(Duration::from_secs(SAMPLE_INTERVAL_SECONDS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    _ = interval.tick() => {}
                }
                let Some(observation) = observation.upgrade() else {
                    break;
                };
                let captured = observation.clone();
                match observation
                    .blocking(WorkKind::Observation, move || captured.history_sample())
                    .await
                {
                    Some(Ok(sample)) => observation.history.write().push(sample),
                    Some(Err(_)) | None => {
                        tracing::debug!("production diagnostic sample unavailable");
                    }
                }
            }
        };
        *sampler = Some(tokio::spawn(super::state::supervise(operation, move || {
            if let Some(diagnostics) = diagnostics.as_ref().and_then(Weak::upgrade) {
                diagnostics.report_fatal();
            }
        })));
    }

    pub(super) async fn shutdown_sampler(&self) {
        self.stop.send_replace(true);
        let sampler = self.sampler.lock().await.take();
        if let Some(sampler) = sampler {
            // Preserve a background invariant failure, never pretend a failed
            // sampler shut down successfully.
            super::state::blocking_result(sampler.await);
        }
        self.observation_slots.close();
    }

    pub(super) async fn collect_report(self: &Arc<Self>) -> io::Result<report::DiagnosticReport> {
        let captured = self.clone();
        self.blocking(WorkKind::Observation, move || report::capture(&captured))
            .await
            .ok_or_else(unavailable_state)?
    }
}

fn unavailable_state() -> io::Error {
    io::Error::other("production diagnostic state is no longer available")
}
