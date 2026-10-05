use std::{
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Poison means a service invariant may be broken. Never return an empty result
/// or continue with the poisoned state; fail the operation explicitly.
pub(super) struct State<T>(RwLock<T>);

impl<T> State<T> {
    pub(super) fn new(value: T) -> Self {
        Self(RwLock::new(value))
    }
    pub(super) fn read(&self) -> RwLockReadGuard<'_, T> {
        read_lock(&self.0)
    }
    pub(super) fn write(&self) -> RwLockWriteGuard<'_, T> {
        write_lock(&self.0)
    }
}

pub(super) fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(state) => state,
        Err(error) => panic!("server state invariant failed: {error}"),
    }
}

pub(super) fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(state) => state,
        Err(error) => panic!("server state invariant failed: {error}"),
    }
}

pub(super) fn revision_result<T>(result: Result<T, super::revision::RevisionError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("document revision invariant failed: {error}"),
    }
}

pub(super) fn blocking_result<T>(result: Result<T, tokio::task::JoinError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("server blocking operation failed: {error}"),
    }
}

/// Background futures run outside the transport task. Report an invariant
/// failure to its supervisor before preserving the original panic payload.
pub(super) async fn supervise<F: Future>(future: F, report_failure: impl Fn()) -> F::Output {
    tokio::pin!(future);
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(poll) => poll,
            Err(payload) => {
                report_failure();
                resume_unwind(payload)
            }
        },
    )
    .await
}
