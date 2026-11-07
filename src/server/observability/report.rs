use super::{
    Observability,
    history::HistorySnapshot,
    process::{MEMORY_SCOPE, ProcessMemoryError, ProcessMemorySample},
    requests::RequestMetricsSnapshot,
    work::{CacheTotals, WorkSummary},
};
use crate::{
    server::{
        compiler::{CompilerCacheUsage, compiler_cache_usage},
        lint::LintDiagnosticUsage,
    },
    storage::{StorageSummary, WorkspaceUsage},
};
use serde::Serialize;
use std::io;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct DiagnosticReport {
    schema_version: u32,
    server: Server,
    memory: Memory,
    workspace: WorkspaceUsage,
    storage: StorageSummary,
    caches: Caches,
    lint: LintDiagnosticUsage,
    requests: RequestMetricsSnapshot,
    work: WorkSummary,
    history: HistorySnapshot,
    accounting_notes: [&'static str; 4],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Server {
    name: &'static str,
    version: &'static str,
    operating_system: &'static str,
    architecture: &'static str,
    pid: u32,
    uptime_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Memory {
    scope: &'static str,
    server: MemoryObservation,
    compiler_children: Vec<MemoryObservation>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(super) enum MemoryObservation {
    Available {
        sample: ProcessMemorySample,
    },
    Unavailable {
        pid: u32,
        reason: ProcessMemoryError,
    },
    Pending {
        pid: u32,
    },
}

impl MemoryObservation {
    pub(super) fn from_result(
        pid: u32,
        result: Result<ProcessMemorySample, ProcessMemoryError>,
    ) -> Self {
        match result {
            Ok(sample) => Self::Available { sample },
            Err(reason) => Self::Unavailable { pid, reason },
        }
    }

    pub(super) fn resident_bytes(&self) -> Option<u64> {
        match self {
            Self::Available { sample } => sample.resident_bytes,
            Self::Unavailable { .. } | Self::Pending { .. } => None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Caches {
    semantic: SemanticCache,
    compiler: CompilerCache,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SemanticCache {
    entries: usize,
    retained_owned_backing_bytes: u64,
    counters: CacheTotals,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompilerCache {
    usage: CompilerCacheUsage,
    counters: CacheTotals,
}

/// Runs entirely in the bounded observation worker. Each service guard is
/// released before OS sampling; only aggregate values and owned process
/// observers cross the boundary into the serialized report.
pub(super) fn capture(observation: &Observability) -> io::Result<DiagnosticReport> {
    let sources = observation
        .sources
        .read()
        .clone()
        .ok_or_else(super::unavailable_state)?;
    let workspace = sources
        .workspace
        .upgrade()
        .ok_or_else(super::unavailable_state)?;
    let compiler = sources
        .compiler
        .upgrade()
        .ok_or_else(super::unavailable_state)?;
    let diagnostics = sources
        .diagnostics
        .upgrade()
        .ok_or_else(super::unavailable_state)?;
    let workspace_usage = workspace.read().diagnostic_usage();
    let storage = observation.storage.snapshot();
    let caches = Caches {
        semantic: SemanticCache {
            entries: workspace_usage.semantic_cache_entries,
            retained_owned_backing_bytes: workspace_usage.semantic_cache_bytes,
            counters: workspace.semantic_cache_totals(workspace_usage.semantic_evictions),
        },
        compiler: CompilerCache {
            usage: compiler_cache_usage(&compiler.results),
            counters: observation.compiler_cache.snapshot(),
        },
    };
    let lint = diagnostics.lint.diagnostic_usage();
    let targets = compiler.reapers.observations.targets();
    let mut compiler_children = Vec::with_capacity(targets.len());
    for target in targets {
        let sample = if let Some(observer) = target.observer {
            let result = observer
                .lock()
                .map_err(|_| ProcessMemoryError::SamplingFailed)
                .and_then(|observer| observer.sample());
            MemoryObservation::from_result(target.pid, result)
        } else if let Some(reason) = target.error {
            MemoryObservation::Unavailable {
                pid: target.pid,
                reason,
            }
        } else {
            // Never sample an uncaptured reusable PID merely to fill a row.
            MemoryObservation::Pending { pid: target.pid }
        };
        compiler_children.push(sample);
    }
    Ok(DiagnosticReport {
        schema_version: 1,
        server: Server {
            name: "bend2-lsp",
            version: env!("CARGO_PKG_VERSION"),
            operating_system: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            pid: std::process::id(),
            uptime_ms: observation.uptime_ms(),
        },
        memory: Memory {
            scope: MEMORY_SCOPE,
            server: observation.self_memory(),
            compiler_children,
        },
        workspace: workspace_usage,
        storage,
        caches,
        lint,
        requests: observation.requests.snapshot(),
        work: observation.work.snapshot(),
        history: observation.history.read().snapshot(),
        accounting_notes: [
            "Owned backing-buffer capacities are estimates, not exact live heap or OS memory. Allocator headers, fragmentation and untracked owners remain unattributed.",
            "Workspace/cache/lint ownership fields can be subsets of shared live snapshot/view storage. Do not sum them as disjoint memory buckets.",
            "Service counters and OS samples are captured independently, not as a globally atomic transaction; live request/work gauges may change during collection.",
            "History has at most sixty periodic samples, one per minute. Explicit exports do not add history samples; source text, request payloads and project paths are excluded.",
        ],
    })
}
