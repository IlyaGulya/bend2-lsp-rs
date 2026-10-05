use crate::{analysis::Revision, workspace::Document};
use std::{
    collections::HashSet,
    error::Error,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{Notify, watch};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RevisionError {
    Poisoned,
    GenerationExhausted,
}

impl fmt::Display for RevisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => formatter.write_str("revision coordinator state is poisoned"),
            Self::GenerationExhausted => {
                formatter.write_str("revision coordinator generation is exhausted")
            }
        }
    }
}

impl Error for RevisionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RevisionStatus {
    desired: Option<Revision>,
    committed: Option<Revision>,
    generation: u64,
    failed_generation: Option<u64>,
    internal_failure: Option<RevisionError>,
}

impl RevisionStatus {
    pub(crate) fn checked(self) -> Result<Self, RevisionError> {
        match self.internal_failure {
            Some(error) => Err(error),
            None => Ok(self),
        }
    }

    pub(crate) fn desired(self) -> Option<Revision> {
        self.desired
    }

    pub(crate) fn committed(self) -> Option<Revision> {
        self.committed
    }

    pub(crate) fn generation(self) -> u64 {
        self.generation
    }

    pub(crate) fn is_failed(self) -> bool {
        self.failed_generation == Some(self.generation)
    }

    pub(crate) fn is_pending(self) -> bool {
        self.desired.is_some() && self.committed != self.desired && !self.is_failed()
    }
}

struct StageOrder {
    next_generation: Option<u64>,
    skipped: HashSet<u64>,
}

impl StageOrder {
    fn finish(&mut self, generation: u64) {
        let Some(next) = self.next_generation else {
            return;
        };
        if generation == next {
            self.next_generation = next.checked_add(1);
            while let Some(next) = self.next_generation {
                if !self.skipped.remove(&next) {
                    break;
                }
                self.next_generation = next.checked_add(1);
            }
        } else if generation > next {
            self.skipped.insert(generation);
        }
    }
}

#[derive(Clone, Copy)]
struct TicketIdentity {
    revision: Revision,
    generation: u64,
}

// These transitions own no locks, tasks, documents, or notification primitives.
// The stage cursor is independent of the latest desired generation: superseded
// edits still stage in order so the next edit can reuse their immutable snapshot.
struct RevisionState {
    status: RevisionStatus,
    order: StageOrder,
    closed_through: u64,
}

impl RevisionState {
    fn new(revision: Option<Revision>) -> Self {
        Self {
            status: RevisionStatus {
                desired: revision,
                committed: revision,
                generation: 0,
                failed_generation: None,
                internal_failure: None,
            },
            order: StageOrder {
                next_generation: Some(1),
                skipped: HashSet::new(),
            },
            closed_through: 0,
        }
    }

    fn reserve(
        &mut self,
        revision: Revision,
        refresh: bool,
    ) -> Result<Option<TicketIdentity>, RevisionError> {
        let accepted = if refresh {
            self.status.desired == Some(revision)
        } else {
            self.status
                .desired
                .is_none_or(|desired| revision.0 > desired.0)
        };
        if !accepted {
            return Ok(None);
        }
        let generation = self
            .status
            .generation
            .checked_add(1)
            .ok_or(RevisionError::GenerationExhausted)?;
        self.status.desired = Some(revision);
        self.status.generation = generation;
        self.status.failed_generation = None;
        if refresh {
            self.status.committed = None;
        }
        Ok(Some(TicketIdentity {
            revision,
            generation,
        }))
    }

    fn is_current(&self, ticket: TicketIdentity) -> bool {
        self.status.generation == ticket.generation && self.status.desired == Some(ticket.revision)
    }

    fn is_open_epoch(&self, ticket: TicketIdentity) -> bool {
        ticket.generation > self.closed_through
    }

    fn commit(&mut self, ticket: TicketIdentity) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        self.status.committed = Some(ticket.revision);
        self.status.failed_generation = None;
        true
    }

    fn finish(&mut self, ticket: TicketIdentity) {
        if self.is_current(ticket) && self.status.committed != Some(ticket.revision) {
            self.status.failed_generation = Some(ticket.generation);
        }
        self.order.finish(ticket.generation);
    }

    fn close(&mut self) {
        self.status.desired = None;
        self.status.committed = None;
        self.status.failed_generation = None;
        self.closed_through = self.status.generation;
        self.order.next_generation = self.status.generation.checked_add(1);
        self.order.skipped.clear();
    }
}

struct RevisionSlot {
    state: RevisionState,
    staged_document: Option<Document>,
}

pub(crate) struct DocumentRevisionSync {
    status: watch::Sender<RevisionStatus>,
    slot: Mutex<RevisionSlot>,
    stage_ready: Notify,
}

impl DocumentRevisionSync {
    pub(crate) fn new(document: Option<Document>) -> Self {
        let state = RevisionState::new(document.as_ref().map(|document| document.revision));
        let (status, _) = watch::channel(state.status);
        Self {
            status,
            slot: Mutex::new(RevisionSlot {
                state,
                staged_document: document,
            }),
            stage_ready: Notify::new(),
        }
    }

    pub(crate) fn status(&self) -> Result<RevisionStatus, RevisionError> {
        (*self.status.borrow()).checked()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<RevisionStatus> {
        self.status.subscribe()
    }

    fn publish(&self, state: RevisionStatus) {
        self.status.send_if_modified(|status| {
            if *status == state {
                return false;
            }
            *status = state;
            true
        });
    }

    fn report_internal_failure(&self, error: RevisionError) -> RevisionError {
        let mut reported = error;
        self.status.send_if_modified(|status| {
            if let Some(existing) = status.internal_failure {
                reported = existing;
                return false;
            }
            status.internal_failure = Some(error);
            true
        });
        self.stage_ready.notify_waiters();
        reported
    }

    fn lock_slot(&self) -> Result<MutexGuard<'_, RevisionSlot>, RevisionError> {
        self.status()?;
        let slot = self
            .slot
            .lock()
            .map_err(|_| self.report_internal_failure(RevisionError::Poisoned))?;
        slot.state.status.checked()?;
        Ok(slot)
    }

    fn update<T>(
        &self,
        transition: impl FnOnce(&mut RevisionSlot) -> Result<T, RevisionError>,
    ) -> Result<T, RevisionError> {
        let mut slot = self.lock_slot()?;
        let result = transition(&mut slot);
        if let Err(error) = &result {
            slot.state.status.internal_failure = Some(*error);
        }
        self.publish(slot.state.status);
        drop(slot);
        if result.is_err() {
            self.stage_ready.notify_waiters();
        }
        result
    }

    fn reserve_ticket(
        self: &Arc<Self>,
        revision: Revision,
        refresh: bool,
    ) -> Result<Option<RevisionTicket>, RevisionError> {
        let identity = self.update(|slot| slot.state.reserve(revision, refresh))?;
        Ok(identity.map(|identity| RevisionTicket {
            sync: self.clone(),
            identity,
        }))
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        revision: Revision,
    ) -> Result<Option<RevisionTicket>, RevisionError> {
        self.reserve_ticket(revision, false)
    }

    pub(crate) fn reserve_refresh(
        self: &Arc<Self>,
        revision: Revision,
    ) -> Result<Option<RevisionTicket>, RevisionError> {
        self.reserve_ticket(revision, true)
    }

    pub(crate) fn close(&self) -> Result<(), RevisionError> {
        let result = self.update(|slot| {
            slot.state.close();
            slot.staged_document = None;
            Ok(())
        });
        self.stage_ready.notify_waiters();
        result
    }
}

pub(crate) async fn wait_for_captured_revision(
    mut status: watch::Receiver<RevisionStatus>,
    captured_generation: u64,
) -> Result<(), RevisionError> {
    loop {
        let state = (*status.borrow_and_update()).checked()?;
        if state.generation() != captured_generation || !state.is_pending() {
            return Ok(());
        }
        if status.changed().await.is_err() {
            return Ok(());
        }
    }
}

pub(crate) struct RevisionTicket {
    sync: Arc<DocumentRevisionSync>,
    identity: TicketIdentity,
}

impl RevisionTicket {
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.identity.generation
    }

    pub(crate) async fn wait_for_turn(&self) -> Result<bool, RevisionError> {
        loop {
            let notified = self.sync.stage_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let slot = self.sync.lock_slot()?;
                if !slot.state.is_open_epoch(self.identity) {
                    return Ok(false);
                }
                if slot.state.order.next_generation == Some(self.identity.generation) {
                    return Ok(true);
                }
            }
            notified.await;
        }
    }

    pub(crate) fn is_current(&self) -> Result<bool, RevisionError> {
        let status = self.sync.status()?;
        Ok(status.generation() == self.identity.generation
            && status.desired() == Some(self.identity.revision))
    }

    pub(crate) fn store_staged_document(&self, document: Document) -> Result<bool, RevisionError> {
        let mut slot = self.sync.lock_slot()?;
        if !slot.state.is_open_epoch(self.identity) {
            return Ok(false);
        }
        slot.staged_document = Some(document);
        Ok(true)
    }

    pub(crate) fn staged_document(&self) -> Result<Option<Document>, RevisionError> {
        let slot = self.sync.lock_slot()?;
        if !slot.state.is_open_epoch(self.identity) {
            return Ok(None);
        }
        Ok(slot.staged_document.clone())
    }

    pub(crate) fn mark_committed(&self) -> Result<bool, RevisionError> {
        self.sync
            .update(|slot| Ok(slot.state.commit(self.identity)))
    }
}

impl Drop for RevisionTicket {
    fn drop(&mut self) {
        // Drop cannot return an error. update publishes terminal internal errors
        // and wakes all waiters; subsequent public operations return that error.
        let _result = self.sync.update(|slot| {
            slot.state.finish(self.identity);
            Ok(())
        });
        self.sync.stage_ready.notify_waiters();
    }
}

#[cfg(test)]
#[path = "revision_tests.rs"]
mod tests;
