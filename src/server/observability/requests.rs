//! Bounded, fixed-cardinality request accounting for the LSP transport.
//!
//! The wrapper records only static method buckets, numeric counters and
//! durations: no request payload, URI, request ID or arbitrary method label is
//! retained. Method buckets are a fixed enum, so custom or future methods all
//! share [`RequestMethod::Other`].
//!
//! Outcomes are mutually exclusive per call: `completed`, `errors`,
//! `cancelled` or `abandoned`, with `active` covering calls that have not yet
//! reached a terminal outcome. A dropped future is counted as `abandoned`
//! because a drop alone does not prove client-requested cancellation; only a
//! service response carrying [`tower_lsp::jsonrpc::ErrorCode::RequestCancelled`]
//! counts as `cancelled`.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use serde::Serialize;
use tower_lsp::jsonrpc::{ErrorCode, Response};

/// Static JSON-RPC methods implemented by this server.
///
/// The declaration order is the deterministic snapshot order. Every variant
/// maps back to one wire method; [`RequestMethod::Other`] intentionally absorbs
/// all remaining and future methods so cardinality cannot grow from input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(crate) enum RequestMethod {
    Initialize,
    Initialized,
    Shutdown,
    Exit,
    Cancel,
    DidOpen,
    DidChange,
    DidSave,
    DidClose,
    DidChangeConfiguration,
    DidChangeWatchedFiles,
    DidChangeWorkspaceFolders,
    DidRenameFiles,
    WillRenameFiles,
    ExecuteCommand,
    Completion,
    Hover,
    Definition,
    TypeDefinition,
    References,
    DocumentHighlight,
    DocumentSymbol,
    WorkspaceSymbol,
    CodeAction,
    CodeLens,
    InlayHint,
    DocumentLink,
    FoldingRange,
    SelectionRange,
    Formatting,
    RangeFormatting,
    OnTypeFormatting,
    SignatureHelp,
    PrepareRename,
    Rename,
    SemanticTokensFull,
    SemanticDocument,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
    PrepareTypeHierarchy,
    Supertypes,
    Subtypes,
    Other,
}

impl RequestMethod {
    /// Number of fixed method buckets, including [`RequestMethod::Other`].
    pub(crate) const COUNT: usize = 44;

    /// Every bucket in deterministic snapshot order.
    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::Initialize,
        Self::Initialized,
        Self::Shutdown,
        Self::Exit,
        Self::Cancel,
        Self::DidOpen,
        Self::DidChange,
        Self::DidSave,
        Self::DidClose,
        Self::DidChangeConfiguration,
        Self::DidChangeWatchedFiles,
        Self::DidChangeWorkspaceFolders,
        Self::DidRenameFiles,
        Self::WillRenameFiles,
        Self::ExecuteCommand,
        Self::Completion,
        Self::Hover,
        Self::Definition,
        Self::TypeDefinition,
        Self::References,
        Self::DocumentHighlight,
        Self::DocumentSymbol,
        Self::WorkspaceSymbol,
        Self::CodeAction,
        Self::CodeLens,
        Self::InlayHint,
        Self::DocumentLink,
        Self::FoldingRange,
        Self::SelectionRange,
        Self::Formatting,
        Self::RangeFormatting,
        Self::OnTypeFormatting,
        Self::SignatureHelp,
        Self::PrepareRename,
        Self::Rename,
        Self::SemanticTokensFull,
        Self::SemanticDocument,
        Self::PrepareCallHierarchy,
        Self::IncomingCalls,
        Self::OutgoingCalls,
        Self::PrepareTypeHierarchy,
        Self::Supertypes,
        Self::Subtypes,
        Self::Other,
    ];

    /// Resolves a JSON-RPC wire method to its static bucket.
    #[must_use]
    pub(crate) fn from_method(method: &str) -> Self {
        match method {
            "initialize" => Self::Initialize,
            "initialized" => Self::Initialized,
            "shutdown" => Self::Shutdown,
            "exit" => Self::Exit,
            "$/cancelRequest" => Self::Cancel,
            "textDocument/didOpen" => Self::DidOpen,
            "textDocument/didChange" => Self::DidChange,
            "textDocument/didSave" => Self::DidSave,
            "textDocument/didClose" => Self::DidClose,
            "workspace/didChangeConfiguration" => Self::DidChangeConfiguration,
            "workspace/didChangeWatchedFiles" => Self::DidChangeWatchedFiles,
            "workspace/didChangeWorkspaceFolders" => Self::DidChangeWorkspaceFolders,
            "workspace/didRenameFiles" => Self::DidRenameFiles,
            "workspace/willRenameFiles" => Self::WillRenameFiles,
            "workspace/executeCommand" => Self::ExecuteCommand,
            "textDocument/completion" => Self::Completion,
            "textDocument/hover" => Self::Hover,
            "textDocument/definition" => Self::Definition,
            "textDocument/typeDefinition" => Self::TypeDefinition,
            "textDocument/references" => Self::References,
            "textDocument/documentHighlight" => Self::DocumentHighlight,
            "textDocument/documentSymbol" => Self::DocumentSymbol,
            "workspace/symbol" => Self::WorkspaceSymbol,
            "textDocument/codeAction" => Self::CodeAction,
            "textDocument/codeLens" => Self::CodeLens,
            "textDocument/inlayHint" => Self::InlayHint,
            "textDocument/documentLink" => Self::DocumentLink,
            "textDocument/foldingRange" => Self::FoldingRange,
            "textDocument/selectionRange" => Self::SelectionRange,
            "textDocument/formatting" => Self::Formatting,
            "textDocument/rangeFormatting" => Self::RangeFormatting,
            "textDocument/onTypeFormatting" => Self::OnTypeFormatting,
            "textDocument/signatureHelp" => Self::SignatureHelp,
            "textDocument/prepareRename" => Self::PrepareRename,
            "textDocument/rename" => Self::Rename,
            "textDocument/semanticTokens/full" => Self::SemanticTokensFull,
            "bend2/semanticDocument" => Self::SemanticDocument,
            "textDocument/prepareCallHierarchy" => Self::PrepareCallHierarchy,
            "callHierarchy/incomingCalls" => Self::IncomingCalls,
            "callHierarchy/outgoingCalls" => Self::OutgoingCalls,
            "textDocument/prepareTypeHierarchy" => Self::PrepareTypeHierarchy,
            "typeHierarchy/supertypes" => Self::Supertypes,
            "typeHierarchy/subtypes" => Self::Subtypes,
            _ => Self::Other,
        }
    }

    /// Stable snapshot label for this fixed bucket.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Initialized => "initialized",
            Self::Shutdown => "shutdown",
            Self::Exit => "exit",
            Self::Cancel => "cancel",
            Self::DidOpen => "did_open",
            Self::DidChange => "did_change",
            Self::DidSave => "did_save",
            Self::DidClose => "did_close",
            Self::DidChangeConfiguration => "did_change_configuration",
            Self::DidChangeWatchedFiles => "did_change_watched_files",
            Self::DidChangeWorkspaceFolders => "did_change_workspace_folders",
            Self::DidRenameFiles => "did_rename_files",
            Self::WillRenameFiles => "will_rename_files",
            Self::ExecuteCommand => "execute_command",
            Self::Completion => "completion",
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::TypeDefinition => "type_definition",
            Self::References => "references",
            Self::DocumentHighlight => "document_highlight",
            Self::DocumentSymbol => "document_symbol",
            Self::WorkspaceSymbol => "workspace_symbol",
            Self::CodeAction => "code_action",
            Self::CodeLens => "code_lens",
            Self::InlayHint => "inlay_hint",
            Self::DocumentLink => "document_link",
            Self::FoldingRange => "folding_range",
            Self::SelectionRange => "selection_range",
            Self::Formatting => "formatting",
            Self::RangeFormatting => "range_formatting",
            Self::OnTypeFormatting => "on_type_formatting",
            Self::SignatureHelp => "signature_help",
            Self::PrepareRename => "prepare_rename",
            Self::Rename => "rename",
            Self::SemanticTokensFull => "semantic_tokens_full",
            Self::SemanticDocument => "semantic_document",
            Self::PrepareCallHierarchy => "prepare_call_hierarchy",
            Self::IncomingCalls => "incoming_calls",
            Self::OutgoingCalls => "outgoing_calls",
            Self::PrepareTypeHierarchy => "prepare_type_hierarchy",
            Self::Supertypes => "supertypes",
            Self::Subtypes => "subtypes",
            Self::Other => "other",
        }
    }

    /// Coarse label used by existing native trace flows.
    ///
    /// This preserves the previous `method_label` classification exactly:
    /// methods that were not part of the original trace set remain `other`.
    #[must_use]
    pub(crate) const fn trace_label(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Shutdown => "shutdown",
            Self::Exit => "exit",
            Self::Cancel => "cancel",
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::TypeDefinition => "type_definition",
            Self::References => "references",
            _ => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Per-method request accounting shared by the transport and the diagnostics
/// sampler.
pub struct RequestMetrics {
    methods: [MethodMetrics; RequestMethod::COUNT],
}

impl Default for RequestMetrics {
    fn default() -> Self {
        Self {
            methods: std::array::from_fn(|_| MethodMetrics::default()),
        }
    }
}

impl RequestMetrics {
    /// Starts accounting for one service call and wraps its future.
    ///
    /// The wrapper stores only the method bucket, a boolean notification
    /// marker and a start instant. No payload, URI, request ID or method string
    /// is retained, and the returned future adds no heap allocation.
    pub(crate) fn start<F>(
        self: &Arc<Self>,
        method: RequestMethod,
        notification: bool,
        future: F,
    ) -> RequestFuture<F> {
        let bucket = &self.methods[method.index()];
        bucket.calls.fetch_add(1, Ordering::Relaxed);
        if notification {
            bucket.notifications.fetch_add(1, Ordering::Relaxed);
        }
        bucket.active.fetch_add(1, Ordering::Relaxed);
        bucket.awaiting_first_poll.fetch_add(1, Ordering::Relaxed);
        RequestFuture {
            inner: future,
            metrics: Arc::clone(self),
            method,
            started: Instant::now(),
            first_polled: false,
            finished: false,
        }
    }

    /// Captures the complete fixed-cardinality request aggregate.
    #[must_use]
    pub fn snapshot(&self) -> RequestMetricsSnapshot {
        RequestMetricsSnapshot {
            methods: RequestMethod::ALL
                .iter()
                .copied()
                .map(|method| self.methods[method.index()].snapshot(method))
                .collect(),
        }
    }

    /// Live calls that have not reached a terminal outcome.
    ///
    /// Includes notifications and calls whose future has not been polled yet.
    /// Reads only the fixed gauges, so samplers can record it without building
    /// or serializing a full snapshot.
    #[must_use]
    pub fn active_requests(&self) -> u64 {
        self.methods
            .iter()
            .map(|bucket| bucket.active.load(Ordering::Relaxed))
            .sum()
    }

    /// Live calls whose wrapped future has not been polled once.
    #[must_use]
    pub fn requests_awaiting_first_poll(&self) -> u64 {
        self.methods
            .iter()
            .map(|bucket| bucket.awaiting_first_poll.load(Ordering::Relaxed))
            .sum()
    }
}

/// Serializable request aggregate with one row per fixed method bucket.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMetricsSnapshot {
    /// Fixed method buckets in [`RequestMethod::ALL`] order.
    pub methods: Vec<RequestMethodSnapshot>,
}

/// Serializable counters and durations for one fixed method bucket.
///
/// `calls` always equals `completed + errors + cancelled + abandoned +
/// active`. `notifications` is a subset of `calls`. Durations are microseconds
/// measured from service `call` to the terminal outcome; the scheduling wait is
/// measured only from `call` to the first poll, so it never includes handler
/// execution time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMethodSnapshot {
    /// Fixed bucket label.
    pub method: &'static str,
    /// Service calls observed, including notifications.
    pub calls: u64,
    /// Calls without a request ID.
    pub notifications: u64,
    /// Calls that finished without an error response, including notifications.
    pub completed: u64,
    /// Calls that failed with a service error or non-cancellation error response.
    pub errors: u64,
    /// Calls whose service response carried JSON-RPC `RequestCancelled`.
    pub cancelled: u64,
    /// Calls whose future was dropped before completion.
    pub abandoned: u64,
    /// Calls that have not reached a terminal outcome yet.
    pub active: u64,
    /// Calls whose future has not been polled once (serializes as `awaitingFirstPoll`).
    pub awaiting_first_poll: u64,
    /// Sum of `call` to terminal-outcome durations.
    pub total_duration_micros: u64,
    /// Longest `call` to terminal-outcome duration.
    pub max_duration_micros: u64,
    /// Sum of `call` to first-poll waits.
    pub total_scheduling_wait_micros: u64,
    /// Longest `call` to first-poll wait.
    pub max_scheduling_wait_micros: u64,
}

#[derive(Default)]
struct MethodMetrics {
    calls: AtomicU64,
    notifications: AtomicU64,
    completed: AtomicU64,
    errors: AtomicU64,
    cancelled: AtomicU64,
    abandoned: AtomicU64,
    active: AtomicU64,
    awaiting_first_poll: AtomicU64,
    total_duration_micros: AtomicU64,
    max_duration_micros: AtomicU64,
    total_scheduling_wait_micros: AtomicU64,
    max_scheduling_wait_micros: AtomicU64,
}

impl MethodMetrics {
    fn snapshot(&self, method: RequestMethod) -> RequestMethodSnapshot {
        RequestMethodSnapshot {
            method: method.label(),
            calls: self.calls.load(Ordering::Relaxed),
            notifications: self.notifications.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            abandoned: self.abandoned.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            awaiting_first_poll: self.awaiting_first_poll.load(Ordering::Relaxed),
            total_duration_micros: self.total_duration_micros.load(Ordering::Relaxed),
            max_duration_micros: self.max_duration_micros.load(Ordering::Relaxed),
            total_scheduling_wait_micros: self.total_scheduling_wait_micros.load(Ordering::Relaxed),
            max_scheduling_wait_micros: self.max_scheduling_wait_micros.load(Ordering::Relaxed),
        }
    }
}

/// Future wrapper that accounts for first-poll latency and terminal outcome.
///
/// It borrows no request data, performs no allocation, and only touches the
/// caller's fixed method bucket. The inner future is required to be `Unpin` so
/// the wrapper can avoid boxing.
pub(crate) struct RequestFuture<F> {
    inner: F,
    metrics: Arc<RequestMetrics>,
    method: RequestMethod,
    started: Instant,
    first_polled: bool,
    finished: bool,
}

impl<F, E> Future for RequestFuture<F>
where
    F: Future<Output = Result<Option<Response>, E>> + Unpin,
{
    type Output = Result<Option<Response>, E>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let bucket = &this.metrics.methods[this.method.index()];
        if !this.first_polled {
            this.first_polled = true;
            bucket.awaiting_first_poll.fetch_sub(1, Ordering::Relaxed);
            let wait = micros(this.started.elapsed());
            bucket
                .total_scheduling_wait_micros
                .fetch_add(wait, Ordering::Relaxed);
            bucket
                .max_scheduling_wait_micros
                .fetch_max(wait, Ordering::Relaxed);
        }
        match Pin::new(&mut this.inner).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => {
                this.finished = true;
                bucket.active.fetch_sub(1, Ordering::Relaxed);
                let duration = micros(this.started.elapsed());
                bucket
                    .total_duration_micros
                    .fetch_add(duration, Ordering::Relaxed);
                bucket
                    .max_duration_micros
                    .fetch_max(duration, Ordering::Relaxed);
                match outcome(&output) {
                    Outcome::Completed => {
                        bucket.completed.fetch_add(1, Ordering::Relaxed);
                    }
                    Outcome::Error => {
                        bucket.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Outcome::Cancelled => {
                        bucket.cancelled.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Poll::Ready(output)
            }
        }
    }
}

impl<F> Drop for RequestFuture<F> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A dropped future is not proof that the client cancelled it: the
        // transport may abandon work at shutdown. Keep the outcome explicit.
        let bucket = &self.metrics.methods[self.method.index()];
        bucket.abandoned.fetch_add(1, Ordering::Relaxed);
        bucket.active.fetch_sub(1, Ordering::Relaxed);
        if !self.first_polled {
            bucket.awaiting_first_poll.fetch_sub(1, Ordering::Relaxed);
        }
        let duration = micros(self.started.elapsed());
        bucket
            .total_duration_micros
            .fetch_add(duration, Ordering::Relaxed);
        bucket
            .max_duration_micros
            .fetch_max(duration, Ordering::Relaxed);
    }
}

enum Outcome {
    Completed,
    Error,
    Cancelled,
}

fn outcome<E>(output: &Result<Option<Response>, E>) -> Outcome {
    match output {
        Err(_) => Outcome::Error,
        Ok(None) => Outcome::Completed,
        Ok(Some(response)) => match response.error() {
            Some(error) if error.code == ErrorCode::RequestCancelled => Outcome::Cancelled,
            Some(_) => Outcome::Error,
            None => Outcome::Completed,
        },
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Must;
    use std::future::{Future, ready};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use tower_lsp::jsonrpc::{Error, Id, Response};

    type TestOutput = Result<Option<Response>, &'static str>;

    fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        Pin::new(future).poll(&mut context)
    }

    fn ok_none() -> TestOutput {
        Ok(None)
    }

    fn bucket<'a>(
        snapshot: &'a RequestMetricsSnapshot,
        method: RequestMethod,
    ) -> &'a RequestMethodSnapshot {
        snapshot
            .methods
            .iter()
            .find(|row| row.method == method.label())
            .must_be("snapshot contains every fixed method bucket")
    }

    fn assert_idle(row: &RequestMethodSnapshot) {
        assert_eq!(row.calls, 0);
        assert_eq!(row.notifications, 0);
        assert_eq!(row.completed, 0);
        assert_eq!(row.errors, 0);
        assert_eq!(row.cancelled, 0);
        assert_eq!(row.abandoned, 0);
        assert_eq!(row.active, 0);
        assert_eq!(row.awaiting_first_poll, 0);
        assert_eq!(row.total_duration_micros, 0);
        assert_eq!(row.max_duration_micros, 0);
        assert_eq!(row.total_scheduling_wait_micros, 0);
        assert_eq!(row.max_scheduling_wait_micros, 0);
    }

    #[test]
    fn first_poll_is_measured_and_gauges_transition_exactly_once() {
        let metrics = Arc::new(RequestMetrics::default());
        let mut polls = 0u8;
        let future = metrics.start(
            RequestMethod::Hover,
            false,
            std::future::poll_fn(move |_| {
                if polls == 0 {
                    polls = 1;
                    Poll::Pending
                } else {
                    Poll::Ready(ok_none())
                }
            }),
        );
        let mut future = future;

        let initial = metrics.snapshot();
        let hover = bucket(&initial, RequestMethod::Hover);
        assert_eq!(hover.calls, 1);
        assert_eq!(hover.active, 1);
        assert_eq!(hover.awaiting_first_poll, 1);
        assert_eq!(hover.completed, 0);
        assert_eq!(hover.total_scheduling_wait_micros, 0);
        assert_eq!(hover.max_scheduling_wait_micros, 0);
        assert_eq!(hover.total_duration_micros, 0);
        assert_eq!(metrics.active_requests(), 1);
        assert_eq!(metrics.requests_awaiting_first_poll(), 1);

        assert!(poll_once(&mut future).is_pending());
        let polled = metrics.snapshot();
        let hover = bucket(&polled, RequestMethod::Hover);
        assert_eq!(hover.active, 1);
        assert_eq!(hover.awaiting_first_poll, 0);
        assert_eq!(hover.completed, 0);
        assert_eq!(
            hover.total_scheduling_wait_micros,
            hover.max_scheduling_wait_micros
        );
        let recorded_wait = hover.total_scheduling_wait_micros;
        assert_eq!(metrics.requests_awaiting_first_poll(), 0);

        assert!(poll_once(&mut future).is_ready());
        let done = metrics.snapshot();
        let hover = bucket(&done, RequestMethod::Hover);
        assert_eq!(hover.calls, 1);
        assert_eq!(hover.active, 0);
        assert_eq!(hover.awaiting_first_poll, 0);
        assert_eq!(hover.completed, 1);
        assert_eq!(hover.errors, 0);
        assert_eq!(hover.cancelled, 0);
        assert_eq!(hover.abandoned, 0);
        assert_eq!(hover.total_scheduling_wait_micros, recorded_wait);
        assert_eq!(hover.total_duration_micros, hover.max_duration_micros);
        assert!(hover.total_duration_micros >= hover.total_scheduling_wait_micros);
        assert_eq!(metrics.active_requests(), 0);

        drop(future);
        let dropped = metrics.snapshot();
        let hover = bucket(&dropped, RequestMethod::Hover);
        assert_eq!(hover.completed, 1);
        assert_eq!(hover.abandoned, 0);
        assert_eq!(hover.active, 0);
    }

    #[test]
    fn service_errors_are_counted_as_errors() {
        let metrics = Arc::new(RequestMetrics::default());
        let mut future = metrics.start(
            RequestMethod::Completion,
            false,
            ready(Err::<Option<Response>, _>("service failure")),
        );
        assert!(poll_once(&mut future).is_ready());

        let snapshot = metrics.snapshot();
        let completion = bucket(&snapshot, RequestMethod::Completion);
        assert_eq!(completion.calls, 1);
        assert_eq!(completion.errors, 1);
        assert_eq!(completion.completed, 0);
        assert_eq!(completion.cancelled, 0);
        assert_eq!(completion.abandoned, 0);
        assert_eq!(completion.active, 0);
        assert_eq!(
            completion.total_duration_micros,
            completion.max_duration_micros
        );
    }

    #[test]
    fn request_cancelled_responses_are_counted_as_cancelled() {
        let metrics = Arc::new(RequestMetrics::default());
        let response = Response::from_error(Id::Number(3), Error::request_cancelled());
        let mut future = metrics.start(
            RequestMethod::References,
            false,
            ready(Ok::<_, &'static str>(Some(response))),
        );
        assert!(poll_once(&mut future).is_ready());

        let snapshot = metrics.snapshot();
        let references = bucket(&snapshot, RequestMethod::References);
        assert_eq!(references.calls, 1);
        assert_eq!(references.cancelled, 1);
        assert_eq!(references.errors, 0);
        assert_eq!(references.completed, 0);
        assert_eq!(references.active, 0);
    }

    #[test]
    fn other_error_responses_are_errors_not_cancellations() {
        let metrics = Arc::new(RequestMetrics::default());
        let response = Response::from_error(Id::Number(4), Error::internal_error());
        let mut future = metrics.start(
            RequestMethod::References,
            false,
            ready(Ok::<_, &'static str>(Some(response))),
        );
        assert!(poll_once(&mut future).is_ready());

        let snapshot = metrics.snapshot();
        let references = bucket(&snapshot, RequestMethod::References);
        assert_eq!(references.errors, 1);
        assert_eq!(references.cancelled, 0);
        assert_eq!(references.completed, 0);
    }

    #[test]
    fn notifications_are_counted_separately_from_request_completions() {
        let metrics = Arc::new(RequestMetrics::default());
        let mut future = metrics.start(RequestMethod::DidChange, true, ready(ok_none()));
        assert!(poll_once(&mut future).is_ready());

        let mut failed = metrics.start(
            RequestMethod::DidChange,
            true,
            ready(Err::<Option<Response>, _>("notification failed")),
        );
        assert!(poll_once(&mut failed).is_ready());

        let snapshot = metrics.snapshot();
        let did_change = bucket(&snapshot, RequestMethod::DidChange);
        assert_eq!(did_change.calls, 2);
        assert_eq!(did_change.notifications, 2);
        assert_eq!(did_change.completed, 1);
        assert_eq!(did_change.errors, 1);
        assert_eq!(did_change.active, 0);
    }

    #[test]
    fn dropping_a_never_polled_future_counts_as_abandoned() {
        let metrics = Arc::new(RequestMetrics::default());
        let future = metrics.start(
            RequestMethod::Completion,
            false,
            std::future::pending::<TestOutput>(),
        );
        assert_eq!(metrics.active_requests(), 1);
        assert_eq!(metrics.requests_awaiting_first_poll(), 1);

        drop(future);
        let snapshot = metrics.snapshot();
        let completion = bucket(&snapshot, RequestMethod::Completion);
        assert_eq!(completion.calls, 1);
        assert_eq!(completion.abandoned, 1);
        assert_eq!(completion.completed, 0);
        assert_eq!(completion.errors, 0);
        assert_eq!(completion.cancelled, 0);
        assert_eq!(completion.active, 0);
        assert_eq!(completion.awaiting_first_poll, 0);
        assert_eq!(completion.total_scheduling_wait_micros, 0);
        assert_eq!(
            completion.total_duration_micros,
            completion.max_duration_micros
        );
        assert_eq!(metrics.active_requests(), 0);
        assert_eq!(metrics.requests_awaiting_first_poll(), 0);
    }

    #[test]
    fn dropping_a_polled_pending_future_counts_as_abandoned() {
        let metrics = Arc::new(RequestMetrics::default());
        let future = metrics.start(
            RequestMethod::SemanticTokensFull,
            false,
            std::future::poll_fn(|_| Poll::<TestOutput>::Pending),
        );
        let mut future = future;
        assert!(poll_once(&mut future).is_pending());
        let polled = metrics.snapshot();
        let tokens = bucket(&polled, RequestMethod::SemanticTokensFull);
        assert_eq!(tokens.active, 1);
        assert_eq!(tokens.awaiting_first_poll, 0);
        assert_eq!(
            tokens.total_scheduling_wait_micros,
            tokens.max_scheduling_wait_micros
        );

        drop(future);
        let snapshot = metrics.snapshot();
        let tokens = bucket(&snapshot, RequestMethod::SemanticTokensFull);
        assert_eq!(tokens.abandoned, 1);
        assert_eq!(tokens.active, 0);
        assert_eq!(tokens.completed, 0);
        assert_eq!(tokens.errors, 0);
        assert_eq!(tokens.cancelled, 0);
        assert_eq!(tokens.total_duration_micros, tokens.max_duration_micros);
        assert!(tokens.total_duration_micros >= tokens.total_scheduling_wait_micros);
    }

    #[test]
    fn completion_followed_by_drop_does_not_count_abandoned_or_decrement_twice() {
        let metrics = Arc::new(RequestMetrics::default());
        let future = metrics.start(RequestMethod::Definition, false, ready(ok_none()));
        let mut future = future;
        assert!(poll_once(&mut future).is_ready());

        drop(future);
        let snapshot = metrics.snapshot();
        let definition = bucket(&snapshot, RequestMethod::Definition);
        assert_eq!(definition.calls, 1);
        assert_eq!(definition.completed, 1);
        assert_eq!(definition.abandoned, 0);
        assert_eq!(definition.active, 0);
        assert_eq!(metrics.active_requests(), 0);
    }

    #[test]
    fn unknown_methods_share_one_bounded_other_bucket() {
        let metrics = Arc::new(RequestMetrics::default());
        for index in 0..512 {
            let method = format!("custom/unknownMethod{index}");
            let kind = RequestMethod::from_method(&method);
            assert_eq!(kind, RequestMethod::Other);
            let mut future = metrics.start(kind, false, ready(ok_none()));
            assert!(poll_once(&mut future).is_ready());
        }

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.methods.len(), RequestMethod::COUNT);
        let other = bucket(&snapshot, RequestMethod::Other);
        assert_eq!(other.calls, 512);
        assert_eq!(other.completed, 512);
        let total_calls: u64 = snapshot.methods.iter().map(|row| row.calls).sum();
        assert_eq!(total_calls, 512);
        assert!(snapshot.methods.iter().all(|row| {
            RequestMethod::ALL
                .iter()
                .any(|kind| kind.label() == row.method)
        }));
    }

    #[test]
    fn snapshot_has_every_fixed_bucket_in_stable_order() {
        let metrics = RequestMetrics::default();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.methods.len(), RequestMethod::COUNT);
        let labels: Vec<&str> = snapshot.methods.iter().map(|row| row.method).collect();
        let expected: Vec<&str> = RequestMethod::ALL
            .iter()
            .copied()
            .map(RequestMethod::label)
            .collect();
        assert_eq!(labels, expected);
        for row in &snapshot.methods {
            assert_idle(row);
        }
        assert_eq!(metrics.active_requests(), 0);
        assert_eq!(metrics.requests_awaiting_first_poll(), 0);
    }

    #[test]
    fn snapshot_serializes_camel_case_numeric_metrics() {
        let metrics = Arc::new(RequestMetrics::default());
        let mut future = metrics.start(RequestMethod::Hover, false, ready(ok_none()));
        assert!(poll_once(&mut future).is_ready());

        let value = serde_json::to_value(metrics.snapshot()).must_be("serialize request snapshot");
        let rows = value
            .get("methods")
            .and_then(serde_json::Value::as_array)
            .must_be("methods array");
        assert_eq!(rows.len(), RequestMethod::COUNT);
        let hover = rows
            .iter()
            .find(|row| row.get("method") == Some(&serde_json::Value::from("hover")))
            .must_be("hover row");
        for field in [
            "calls",
            "notifications",
            "completed",
            "errors",
            "cancelled",
            "abandoned",
            "active",
            "awaitingFirstPoll",
            "totalDurationMicros",
            "maxDurationMicros",
            "totalSchedulingWaitMicros",
            "maxSchedulingWaitMicros",
        ] {
            assert!(
                hover.get(field).is_some(),
                "missing serialized field {field}"
            );
        }
        assert!(
            !hover
                .as_object()
                .is_some_and(|row| row.contains_key("requestId"))
        );
    }

    #[test]
    fn live_gauge_totals_span_methods_without_building_a_snapshot() {
        let metrics = Arc::new(RequestMetrics::default());
        let mut first = metrics.start(
            RequestMethod::Hover,
            false,
            std::future::poll_fn(|_| Poll::<TestOutput>::Pending),
        );
        let second = metrics.start(
            RequestMethod::from_method("dynamic/one-off"),
            false,
            std::future::pending::<TestOutput>(),
        );
        assert_eq!(metrics.active_requests(), 2);
        assert_eq!(metrics.requests_awaiting_first_poll(), 2);

        assert!(poll_once(&mut first).is_pending());
        assert_eq!(metrics.active_requests(), 2);
        assert_eq!(metrics.requests_awaiting_first_poll(), 1);

        drop(second);
        assert_eq!(metrics.active_requests(), 1);
        assert_eq!(metrics.requests_awaiting_first_poll(), 0);
        drop(first);
        assert_eq!(metrics.active_requests(), 0);
    }

    #[test]
    fn wire_methods_map_to_distinct_static_buckets() {
        const WIRE_METHODS: &[(&str, RequestMethod)] = &[
            ("initialize", RequestMethod::Initialize),
            ("initialized", RequestMethod::Initialized),
            ("shutdown", RequestMethod::Shutdown),
            ("exit", RequestMethod::Exit),
            ("$/cancelRequest", RequestMethod::Cancel),
            ("textDocument/didOpen", RequestMethod::DidOpen),
            ("textDocument/didChange", RequestMethod::DidChange),
            ("textDocument/didSave", RequestMethod::DidSave),
            ("textDocument/didClose", RequestMethod::DidClose),
            (
                "workspace/didChangeConfiguration",
                RequestMethod::DidChangeConfiguration,
            ),
            (
                "workspace/didChangeWatchedFiles",
                RequestMethod::DidChangeWatchedFiles,
            ),
            (
                "workspace/didChangeWorkspaceFolders",
                RequestMethod::DidChangeWorkspaceFolders,
            ),
            ("workspace/didRenameFiles", RequestMethod::DidRenameFiles),
            ("workspace/willRenameFiles", RequestMethod::WillRenameFiles),
            ("workspace/executeCommand", RequestMethod::ExecuteCommand),
            ("textDocument/completion", RequestMethod::Completion),
            ("textDocument/hover", RequestMethod::Hover),
            ("textDocument/definition", RequestMethod::Definition),
            ("textDocument/typeDefinition", RequestMethod::TypeDefinition),
            ("textDocument/references", RequestMethod::References),
            (
                "textDocument/documentHighlight",
                RequestMethod::DocumentHighlight,
            ),
            ("textDocument/documentSymbol", RequestMethod::DocumentSymbol),
            ("workspace/symbol", RequestMethod::WorkspaceSymbol),
            ("textDocument/codeAction", RequestMethod::CodeAction),
            ("textDocument/codeLens", RequestMethod::CodeLens),
            ("textDocument/inlayHint", RequestMethod::InlayHint),
            ("textDocument/documentLink", RequestMethod::DocumentLink),
            ("textDocument/foldingRange", RequestMethod::FoldingRange),
            ("textDocument/selectionRange", RequestMethod::SelectionRange),
            ("textDocument/formatting", RequestMethod::Formatting),
            (
                "textDocument/rangeFormatting",
                RequestMethod::RangeFormatting,
            ),
            (
                "textDocument/onTypeFormatting",
                RequestMethod::OnTypeFormatting,
            ),
            ("textDocument/signatureHelp", RequestMethod::SignatureHelp),
            ("textDocument/prepareRename", RequestMethod::PrepareRename),
            ("textDocument/rename", RequestMethod::Rename),
            (
                "textDocument/semanticTokens/full",
                RequestMethod::SemanticTokensFull,
            ),
            ("bend2/semanticDocument", RequestMethod::SemanticDocument),
            (
                "textDocument/prepareCallHierarchy",
                RequestMethod::PrepareCallHierarchy,
            ),
            ("callHierarchy/incomingCalls", RequestMethod::IncomingCalls),
            ("callHierarchy/outgoingCalls", RequestMethod::OutgoingCalls),
            (
                "textDocument/prepareTypeHierarchy",
                RequestMethod::PrepareTypeHierarchy,
            ),
            ("typeHierarchy/supertypes", RequestMethod::Supertypes),
            ("typeHierarchy/subtypes", RequestMethod::Subtypes),
        ];
        for (wire, expected) in WIRE_METHODS {
            assert_eq!(
                RequestMethod::from_method(wire),
                *expected,
                "wire method {wire}"
            );
        }
        assert_eq!(WIRE_METHODS.len(), RequestMethod::COUNT - 1);
        let mut labels: Vec<&str> = RequestMethod::ALL.iter().map(|kind| kind.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), RequestMethod::COUNT);
    }

    #[test]
    fn trace_labels_preserve_the_existing_coarse_classification() {
        assert_eq!(RequestMethod::Initialize.trace_label(), "initialize");
        assert_eq!(RequestMethod::Shutdown.trace_label(), "shutdown");
        assert_eq!(RequestMethod::Exit.trace_label(), "exit");
        assert_eq!(RequestMethod::Cancel.trace_label(), "cancel");
        assert_eq!(RequestMethod::Hover.trace_label(), "hover");
        assert_eq!(RequestMethod::Definition.trace_label(), "definition");
        assert_eq!(
            RequestMethod::TypeDefinition.trace_label(),
            "type_definition"
        );
        assert_eq!(RequestMethod::References.trace_label(), "references");
        for coarse_other in [
            RequestMethod::DidOpen,
            RequestMethod::DidChange,
            RequestMethod::Completion,
            RequestMethod::SemanticDocument,
            RequestMethod::ExecuteCommand,
            RequestMethod::Other,
        ] {
            assert_eq!(coarse_other.trace_label(), "other");
        }
    }
}
