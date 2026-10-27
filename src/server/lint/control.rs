use super::LintConfig;
use crate::server::{lsp::Backend, state::State};
use serde_json::{Value, json};
use std::collections::HashMap;
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionTriggerKind,
        Command, ExecuteCommandParams, MessageType, NumberOrString, ProgressParams,
        ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressCreateParams,
        WorkDoneProgressEnd, notification::Progress, request::WorkDoneProgressCreate,
    },
};
use url::Url;

pub(in crate::server) const RUN_COMMAND: &str = "bend2.lint.run";
pub(in crate::server) const STATUS_COMMAND: &str = "bend2.lint.status";
pub(in crate::server) const EXPLAIN_COMMAND: &str = "bend2.lint.explain";
pub(in crate::server) const RUN_KIND: &str = "source.bend2Lint.run";
pub(in crate::server) const STATUS_KIND: &str = "source.bend2Lint.status";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LintPhase {
    Disabled,
    Waiting,
    Preparing,
    Running,
    Ready,
    Skipped,
    Failed,
}

impl LintPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Waiting => "waiting",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Status {
    phase: LintPhase,
    reason: String,
}

pub(super) struct LintControl {
    states: State<HashMap<Url, Status>>,
    configuration_reason: State<Option<String>>,
    progress: State<HashMap<u64, NumberOrString>>,
    notified_problems: State<HashMap<Url, Status>>,
}

impl Default for LintControl {
    fn default() -> Self {
        Self {
            states: State::new(HashMap::new()),
            configuration_reason: State::new(None),
            progress: State::new(HashMap::new()),
            notified_problems: State::new(HashMap::new()),
        }
    }
}

impl LintControl {
    pub(super) fn configure(&self, _config: &LintConfig, reason: Option<String>) {
        self.states.write().clear();
        self.notified_problems.write().clear();
        *self.configuration_reason.write() = reason;
    }

    pub(super) fn record(&self, uri: &Url, phase: LintPhase, reason: impl Into<String>) -> bool {
        if phase == LintPhase::Ready {
            self.notified_problems.write().remove(uri);
        }
        let status = Status {
            phase,
            reason: reason.into(),
        };
        let mut entries = self.states.write();
        if entries.get(uri) == Some(&status) {
            return false;
        }
        entries.insert(uri.clone(), status);
        true
    }

    pub(super) fn forget(&self, uri: &Url) {
        self.states.write().remove(uri);
        self.notified_problems.write().remove(uri);
    }

    pub(super) fn invalidate(&self, reason: impl Into<String>) {
        let reason = reason.into();
        for status in self.states.write().values_mut() {
            status.phase = LintPhase::Waiting;
            status.reason.clone_from(&reason);
        }
    }

    pub(super) fn invalidate_running(&self, reason: impl Into<String>) {
        let reason = reason.into();
        for status in self.states.write().values_mut() {
            if matches!(status.phase, LintPhase::Preparing | LintPhase::Running) {
                status.phase = LintPhase::Waiting;
                status.reason.clone_from(&reason);
            }
        }
    }

    fn should_notify_problem(&self, uri: &Url, phase: LintPhase, reason: &str) -> bool {
        let mut problems = self.notified_problems.write();
        if problems
            .get(uri)
            .is_some_and(|previous| previous.phase == phase && previous.reason == reason)
        {
            return false;
        }
        problems.insert(
            uri.clone(),
            Status {
                phase,
                reason: reason.into(),
            },
        );
        true
    }

    fn status(&self, uri: &Url, config: &LintConfig) -> Status {
        if !config.enabled {
            return Status {
                phase: LintPhase::Disabled,
                reason: self.configuration_reason.read().clone().unwrap_or_else(||
                    "External analysis is disabled. Set bend.lint.enabled to true and select an absolute bend.lint.path; this explicitly permits external tool execution.".into()),
            };
        }
        self.states.read().get(uri).cloned().unwrap_or_else(|| Status {
            phase: LintPhase::Waiting,
            reason: "No current analysis. The next check analyzes the current open inputs.".into(),
        })
    }
}

impl Backend {
    fn lint_status(&self, uri: &Url) -> Value {
        let config = self.diagnostics.lint.config.read().clone();
        let mut status = self.diagnostics.lint.control.status(uri, &config);
        let result = self.current_lint_result(uri);
        let (partial, skipped) = result.as_ref().map_or((false, 0), |entry| {
            (
                !entry.findings.skipped.is_empty(),
                entry.findings.skipped.len(),
            )
        });
        if status.phase == LintPhase::Ready && result.is_none() {
            status.phase = LintPhase::Waiting;
            status.reason =
                "The previous result is no longer current. The next check will refresh it.".into();
        }
        let project_config = uri.to_file_path().ok().and_then(|path| {
            path.parent()?
                .ancestors()
                .map(|directory| directory.join("bend-lint.json"))
                .find(|candidate| candidate.is_file())
        });
        let compiler = self.compiler.config.read().clone();
        json!({
            "state":status.phase.as_str(), "reason":status.reason, "document":uri,
            "executable":config.path, "arguments":config.arguments,
            "projectConfig":project_config,
            "toolIdentity":self.lint_tool_identity(&config, uri),
            "serverExecutable":std::env::current_exe().ok(),
            "serverVersion":env!("CARGO_PKG_VERSION"),
            "compilerExecutable":compiler.path, "compilerArguments":compiler.arguments,
            "dangerousCodeActions":config.dangerous_code_actions,
            "partial":partial, "skippedChecks":skipped,
        })
    }

    pub(in crate::server) async fn handle_lint_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        if !matches!(
            params.command.as_str(),
            RUN_COMMAND | STATUS_COMMAND | EXPLAIN_COMMAND
        ) {
            return Err(Error::invalid_params("Unknown bend-lint command"));
        }
        let argument = params.arguments.first().ok_or_else(|| {
            Error::invalid_params(
                "Expected arguments: [{\"uri\":\"file:///path/to/document.bend\"}]",
            )
        })?;
        let uri = argument
            .get("uri")
            .and_then(Value::as_str)
            .and_then(|value| Url::parse(value).ok())
            .filter(|uri| uri.to_file_path().is_ok())
            .ok_or_else(|| Error::invalid_params("bend-lint requires a file document URI"))?;
        if params.command == EXPLAIN_COMMAND {
            let reason = argument
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::invalid_params("bend2.lint.explain requires a reason"))?;
            self.client.show_message(MessageType::INFO, reason).await;
            return Ok(Some(self.lint_status(&uri)));
        }
        if params.command == RUN_COMMAND {
            if self.diagnostics.lint.config.read().enabled {
                // An explicit run is also the retry gesture after a failed
                // analyzer preparation.
                self.diagnostics.lint.reset_failed_preparation();
                let _document_read = self.document_read(&uri).await;
                self.schedule_lint([uri.clone()], false).await;
            } else {
                let status = self.lint_status(&uri);
                self.client
                    .show_message(
                        MessageType::INFO,
                        status["reason"].as_str().unwrap_or_default(),
                    )
                    .await;
            }
        }
        let status = self.lint_status(&uri);
        if params.command == STATUS_COMMAND {
            self.client.show_message(MessageType::INFO, format!(
                "bend-lint {}: {}\nDocument: {}\nTool: {} {:?}\nProject config: {}\n{}\nServer: {} ({})\nLSP compiler: {} (independent of analyzer compiler)",
                status["state"].as_str().unwrap_or_default(), status["reason"].as_str().unwrap_or_default(),
                uri, status["executable"].as_str().unwrap_or_default(), status["arguments"],
                status["projectConfig"].as_str().unwrap_or("not found; add bend-lint.json in the project"),
                status["toolIdentity"].as_str().unwrap_or_default(),
                status["serverExecutable"].as_str().unwrap_or("unavailable"), env!("CARGO_PKG_VERSION"),
                status["compilerExecutable"].as_str().unwrap_or_default(),
            )).await;
        }
        Ok(Some(status))
    }

    pub(in crate::server) fn lint_control_actions(
        &self,
        params: &CodeActionParams,
    ) -> Vec<CodeActionOrCommand> {
        if params.context.trigger_kind == Some(CodeActionTriggerKind::AUTOMATIC) {
            return Vec::new();
        }
        let explicitly_requested = params.context.only.as_ref().is_some_and(|only| {
            only.iter()
                .any(|kind| matches!(kind.as_str(), "source.bend2Lint" | RUN_KIND | STATUS_KIND))
        });
        if !self.diagnostics.lint.config.read().enabled && !explicitly_requested {
            return Vec::new();
        }
        let uri = &params.text_document.uri;
        if self
            .document(uri)
            .is_none_or(|document| !Self::supported(&document))
        {
            return Vec::new();
        }
        [
            (RUN_KIND, RUN_COMMAND, "Run bend-lint on the open document"),
            (STATUS_KIND, STATUS_COMMAND, "Show bend-lint status"),
        ]
        .into_iter()
        .filter(|(kind, _, _)| {
            params.context.only.as_ref().is_none_or(|only| {
                only.iter().any(|requested| {
                    *kind == requested.as_str()
                        || kind
                            .strip_prefix(requested.as_str())
                            .is_some_and(|suffix| suffix.starts_with('.'))
                })
            })
        })
        .map(|(kind, command, title)| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: title.into(),
                kind: Some(CodeActionKind::new(kind)),
                command: Some(Command {
                    title: title.into(),
                    command: command.into(),
                    arguments: Some(vec![json!({"uri":uri})]),
                }),
                ..Default::default()
            })
        })
        .collect()
    }

    pub(super) async fn begin_lint_progress(&self, uri: &Url) -> Option<u64> {
        if !self.registration.read().lint.work_done_progress {
            return None;
        }
        let id = self.diagnostics.lint.next_progress();
        let token = NumberOrString::String(format!("bend-lint-{id}"));
        if self
            .client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .is_err()
        {
            return None;
        }
        self.diagnostics
            .lint
            .control
            .progress
            .write()
            .insert(id, token.clone());
        self.client
            .send_notification::<Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Checking Bend project (including unsaved buffers)".into(),
                        cancellable: Some(false),
                        message: Some(uri.to_string()),
                        percentage: None,
                    },
                )),
            })
            .await;
        Some(id)
    }

    pub(super) async fn end_lint_progress(&self, id: Option<u64>, reason: &str) {
        let Some(id) = id else {
            return;
        };
        let token = self
            .diagnostics
            .lint
            .control
            .progress
            .write()
            .remove(&id);
        if let Some(token) = token {
            self.client
                .send_notification::<Progress>(ProgressParams {
                    token,
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                        WorkDoneProgressEnd {
                            message: Some(reason.into()),
                        },
                    )),
                })
                .await;
        }
    }

    pub(super) fn take_progress(&self) -> Vec<NumberOrString> {
        self.diagnostics
            .lint
            .control
            .progress
            .write()
            .drain()
            .map(|(_, token)| token)
            .collect()
    }

    pub(super) async fn end_lint_progress_tokens(
        &self,
        tokens: Vec<NumberOrString>,
        reason: &str,
    ) {
        for token in tokens {
            self.client
                .send_notification::<Progress>(ProgressParams {
                    token,
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                        WorkDoneProgressEnd {
                            message: Some(reason.into()),
                        },
                    )),
                })
                .await;
        }
    }

    pub(super) async fn end_all_lint_progress(&self, reason: &str) {
        let tokens = self.take_progress();
        self.end_lint_progress_tokens(tokens, reason).await;
    }

    pub(super) async fn finish_lint_control(
        &self,
        uri: &Url,
        phase: LintPhase,
        reason: &str,
        epoch: u64,
    ) {
        let (current, notify) = {
            let _task = self.diagnostics.lint.task.read();
            let current = self
                .diagnostics
                .lint
                .epoch
                .load(std::sync::atomic::Ordering::SeqCst)
                == epoch;
            let notify = if current {
                self.diagnostics.lint.control.record(uri, phase, reason);
                matches!(phase, LintPhase::Skipped | LintPhase::Failed)
                    && self
                        .diagnostics
                        .lint
                        .control
                        .should_notify_problem(uri, phase, reason)
            } else {
                false
            };
            (current, notify)
        };
        if current && matches!(phase, LintPhase::Skipped | LintPhase::Failed) {
            let message = format!("bend-lint {} for {uri}: {reason}", phase.as_str());
            self.client
                .log_message(MessageType::WARNING, &message)
                .await;
            if notify {
                self.client
                    .show_message(MessageType::WARNING, message)
                    .await;
            }
        }
    }
}
