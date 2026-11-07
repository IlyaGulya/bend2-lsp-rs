//! Bounded, fixed-cardinality request accounting for the LSP transport.
//!
//! Tests are written ahead of the implementation. The wrapper records only
//! static method buckets, numeric counters and durations: no request payload,
//! URI, request ID or arbitrary method label is retained.

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
        assert_eq!(completion.total_duration_micros, completion.max_duration_micros);
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
        let mut future = metrics.start(RequestMethod::DidChange, true, ready(Ok::<_, &'static str>(
            None,
        )));
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
        assert_eq!(completion.total_duration_micros, completion.max_duration_micros);
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
        assert!(
            snapshot
                .methods
                .iter()
                .all(|row| RequestMethod::ALL.iter().any(|kind| kind.label() == row.method))
        );
    }

    #[test]
    fn snapshot_has_every_fixed_bucket_in_stable_order() {
        let metrics = RequestMetrics::default();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.methods.len(), RequestMethod::COUNT);
        let labels: Vec<&str> = snapshot.methods.iter().map(|row| row.method).collect();
        let expected: Vec<&str> = RequestMethod::ALL.iter().map(|kind| kind.label()).collect();
        assert_eq!(labels, expected);
        for row in &snapshot.methods {
            assert_idle(row);
        }
        assert_eq!(metrics.active_requests(), 0);
        assert_eq!(metrics.requests_awaiting_first_poll(), 0);
    }

    #[test]
    fn snapshot_serializes_camel_case_numeric_metrics() {
        let metrics = RequestMetrics::default();
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
            assert!(hover.get(field).is_some(), "missing serialized field {field}");
        }
        assert!(!hover.as_object().is_some_and(|row| row.contains_key("requestId")));
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
            (
                "workspace/executeCommand",
                RequestMethod::ExecuteCommand,
            ),
            ("textDocument/completion", RequestMethod::Completion),
            ("textDocument/hover", RequestMethod::Hover),
            ("textDocument/definition", RequestMethod::Definition),
            (
                "textDocument/typeDefinition",
                RequestMethod::TypeDefinition,
            ),
            ("textDocument/references", RequestMethod::References),
            (
                "textDocument/documentHighlight",
                RequestMethod::DocumentHighlight,
            ),
            (
                "textDocument/documentSymbol",
                RequestMethod::DocumentSymbol,
            ),
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
            (
                "textDocument/signatureHelp",
                RequestMethod::SignatureHelp,
            ),
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
            (
                "callHierarchy/incomingCalls",
                RequestMethod::IncomingCalls,
            ),
            (
                "callHierarchy/outgoingCalls",
                RequestMethod::OutgoingCalls,
            ),
            (
                "textDocument/prepareTypeHierarchy",
                RequestMethod::PrepareTypeHierarchy,
            ),
            ("typeHierarchy/supertypes", RequestMethod::Supertypes),
            ("typeHierarchy/subtypes", RequestMethod::Subtypes),
        ];
        for (wire, expected) in WIRE_METHODS {
            assert_eq!(RequestMethod::from_method(wire), *expected, "wire method {wire}");
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
