use super::{Observability, process::{ProcessMemoryError, ProcessMemoryObserver}, work::{QueuedWork, RunningWork, WorkKind}};
use crate::server::state::{read_lock, write_lock};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock, Weak, atomic::{AtomicU64, Ordering}},
};

#[derive(Clone)]
pub(in crate::server) struct CompilerMemoryTarget {
    pub(in crate::server) pid: u32,
    pub(in crate::server) observer: Option<Arc<Mutex<ProcessMemoryObserver>>>,
    pub(in crate::server) error: Option<ProcessMemoryError>,
}

#[derive(Default)]
pub(in crate::server) struct CompilerRegistry {
    observation: Weak<Observability>,
    generation: AtomicU64,
    children: RwLock<HashMap<u64, CompilerMemoryTarget>>,
}

impl CompilerRegistry {
    pub(in crate::server) fn observed(observation: Weak<Observability>) -> Self {
        Self { observation, ..Self::default() }
    }

    pub(in crate::server) fn metrics(&self) -> Option<Arc<Observability>> {
        self.observation.upgrade()
    }

    pub(in crate::server) fn enqueue(&self) -> Option<CompilerActivity> {
        let observation = self.observation.upgrade()?;
        let generation = self.generation.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
            .unwrap_or_else(|_| panic!("compiler observation generation exhausted"));
        let queued = observation.work.enqueue(WorkKind::Compiler);
        Some(CompilerActivity { generation, observation, queued: Some(queued), running: None })
    }

    pub(in crate::server) fn finish(&self, activity: Option<&CompilerActivity>) {
        if let Some(activity) = activity {
            write_lock(&self.children).remove(&activity.generation);
        }
    }

    pub(in crate::server) fn targets(&self) -> Vec<CompilerMemoryTarget> {
        read_lock(&self.children).values().cloned().collect()
    }
}

pub(in crate::server) struct CompilerActivity {
    generation: u64,
    observation: Arc<Observability>,
    queued: Option<QueuedWork>,
    running: Option<RunningWork>,
}

impl CompilerActivity {
    pub(in crate::server) async fn start(&mut self, registry: &CompilerRegistry, pid: u32) {
        let queued = self.queued.take()
            .unwrap_or_else(|| panic!("compiler process observation started twice"));
        self.running = Some(queued.start());
        let previous = write_lock(&registry.children).insert(self.generation, CompilerMemoryTarget {
            pid, observer: None, error: None,
        });
        assert!(previous.is_none(), "compiler observation generation reused");
        // The owning CompilerChildGuard must exist before this await. If the
        // caller is cancelled, it kills/reaps the child and only then drops
        // its activity. A late capture is discarded, never registered under
        // a recycled PID or another lease's generation.
        let observed = self.observation.blocking(
            WorkKind::Observation,
            move || ProcessMemoryObserver::capture(pid),
        ).await.unwrap_or(Err(ProcessMemoryError::SamplingFailed));
        let mut children = write_lock(&registry.children);
        let target = children.get_mut(&self.generation)
            .unwrap_or_else(|| panic!("compiler activity retired before capture completed"));
        match observed {
            Ok(observer) => target.observer = Some(Arc::new(Mutex::new(observer))),
            Err(error) => target.error = Some(error),
        }
    }

    pub(in crate::server) fn cancel(&self) {
        if let Some(running) = &self.running {
            running.cancel();
        }
    }

    pub(in crate::server) fn fail(&mut self) {
        if let Some(queued) = self.queued.take() {
            self.running = Some(queued.start());
        }
        if let Some(running) = &self.running {
            running.fail();
        }
    }
}
