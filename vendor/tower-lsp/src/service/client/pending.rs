//! Types for tracking server-to-client JSON-RPC requests.

use std::fmt::{self, Debug, Formatter};
use std::future::Future;

use dashmap::{mapref::entry::Entry, DashMap};
use futures::channel::oneshot;
use tracing::warn;

use crate::jsonrpc::{Id, Response};

/// A hashmap containing pending client requests, keyed by request ID.
pub struct Pending(DashMap<Id, Vec<oneshot::Sender<Response>>>);

impl Pending {
    /// Creates a new pending client requests map.
    pub fn new() -> Self {
        Pending(DashMap::new())
    }

    /// Inserts the given response into the map.
    ///
    /// The corresponding `.wait()` future will then resolve to the given value.
    pub fn insert(&self, r: Response) {
        match r.id() {
            Id::Null => warn!("received response with request ID of `null`, ignoring"),
            id => match self.0.entry(id.clone()) {
                Entry::Vacant(_) => warn!("received response with unknown request ID: {}", id),
                Entry::Occupied(mut entry) => {
                    let tx = match entry.get().len() {
                        1 => entry.remove().remove(0),
                        _ => entry.get_mut().remove(0),
                    };

                    if let Err(response) = tx.send(r) {
                        warn!(
                            "received response after request waiter was dropped: {}",
                            response.id()
                        );
                    }
                }
            },
        }
    }

    /// Marks the given request ID as pending and waits for its corresponding response to arrive.
    ///
    /// If the same request ID is being waited upon in multiple locations, then the incoming
    /// response will be routed to one of the callers in a first come, first served basis. To
    /// ensure correct routing of JSON-RPC requests, each identifier value used _must_ be unique.
    pub fn wait(
        &self,
        id: Id,
    ) -> impl Future<Output = Result<Response, oneshot::Canceled>> + Send + 'static {
        let (tx, rx) = oneshot::channel();

        match self.0.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(vec![tx]);
            }
            Entry::Occupied(mut entry) => {
                let txs = entry.get_mut();
                txs.reserve(1); // We assume concurrent waits are rare, so reserve one by one.
                txs.push(tx);
            }
        }

        rx
    }
}

impl Debug for Pending {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let iter = self.0.iter().map(|e| (e.key().clone(), e.value().len()));

        f.debug_map().entries(iter).finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::jsonrpc::EncodedResult;

    fn semantic_response(response: Response) -> Value {
        serde_json::from_slice(&response.to_json_bytes().unwrap()).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waits_for_client_response() {
        let pending = Pending::new();

        let id = Id::Number(1);
        let wait_fut = pending.wait(id.clone());

        let response = Response::from_ok(id, EncodedResult::new(&json!({})).unwrap());
        pending.insert(response.clone());

        assert_eq!(
            semantic_response(wait_fut.await.unwrap()),
            semantic_response(response)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routes_responses_in_fifo_order() {
        let pending = Pending::new();

        let id = Id::Number(1);
        let wait_fut1 = pending.wait(id.clone());
        let wait_fut2 = pending.wait(id.clone());

        let foo = Response::from_ok(id.clone(), EncodedResult::new(&json!("foo")).unwrap());
        let bar = Response::from_ok(id, EncodedResult::new(&json!("bar")).unwrap());
        pending.insert(bar.clone());
        pending.insert(foo.clone());

        assert_eq!(
            semantic_response(wait_fut1.await.unwrap()),
            semantic_response(bar)
        );
        assert_eq!(
            semantic_response(wait_fut2.await.unwrap()),
            semantic_response(foo)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceled_waiter_does_not_panic_when_response_arrives() {
        let pending = Pending::new();
        let id = Id::Number(1);
        let wait_fut = pending.wait(id.clone());
        drop(wait_fut);

        pending.insert(Response::from_ok(
            id,
            EncodedResult::new(&json!(null)).unwrap(),
        ));
        assert!(pending.0.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_pending_returns_cancellation() {
        let pending = Pending::new();
        let wait_fut = pending.wait(Id::Number(1));
        drop(pending);

        assert!(wait_fut.await.is_err());
    }
}
