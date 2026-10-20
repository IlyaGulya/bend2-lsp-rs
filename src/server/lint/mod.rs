mod actions;
mod config;
pub(super) mod control;
mod process;
mod results;
mod saved;
mod wire;
pub(super) use actions::DANGEROUS_KIND;
use config::{LintConfig, installed_identity};
use control::{LintControl, LintPhase};
use results::LintResults;

use super::{
    lsp::Backend,
    state::{State, revision_result},
};
use crate::workspace::{Document, FileId};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{sync::Semaphore, task::JoinHandle};
use tower_lsp::lsp_types::MessageType;
use url::Url;

pub(super) struct LintResult {
    epoch: u64,
    revisions: Vec<(FileId, u64)>,
    config: LintConfig,
    saved: Arc<saved::SavedProject>,
    findings: wire::Findings,
}

pub(super) struct LintService {
    config: State<LintConfig>,
    results: State<LintResults>,
    control: LintControl,
    task: State<Option<JoinHandle<()>>>,
    epoch: AtomicU64,
    closed: AtomicBool,
    semaphore: Arc<Semaphore>,
}

pub(super) struct LintInvalidation {
    previous: Option<JoinHandle<()>>,
    affected: HashSet<Url>,
    warning: Option<String>,
    epoch: u64,
}

impl Default for LintService {
    fn default() -> Self {
        Self {
            config: State::new(LintConfig::default()),
            results: State::new(LintResults::default()),
            control: LintControl::default(),
            task: State::new(None),
            epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            semaphore: Arc::new(Semaphore::new(1)),
        }
    }
}

impl LintService {
    fn next_epoch(&self) -> u64 {
        self.epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("bend-lint epoch exhausted"))
            + 1
    }

    fn take_results(&self) -> HashSet<Url> {
        self.results.write().clear()
    }

    fn invalidate(&self, config: Option<LintConfig>, warning: Option<String>) -> LintInvalidation {
        let mut task = self.task.write();
        if let Some(config) = config {
            self.control.configure(&config, warning.clone());
            *self.config.write() = config;
        } else {
            self.control
                .invalidate("Project inputs changed; save or run bend-lint again");
        }
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.next_epoch();
        let previous = task.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        LintInvalidation {
            previous,
            affected: self.take_results(),
            warning,
            epoch,
        }
    }

    fn invalidate_entries(
        &self,
        mut matches: impl FnMut(&Url, &LintResult) -> bool,
    ) -> LintInvalidation {
        let mut task = self.task.write();
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.next_epoch();
        let previous = task.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        self.control
            .invalidate_running("Inputs changed during checking; save or run again");
        let affected = self.results.write().invalidate(|uri, result| {
            if !matches(uri, result) {
                return false;
            }
            self.control.record(
                uri,
                LintPhase::WaitingForSave,
                "Project inputs changed; save or run bend-lint again",
            );
            true
        });
        LintInvalidation {
            previous,
            affected,
            warning: None,
            epoch,
        }
    }
}

impl Backend {
    pub(super) fn configure_lint(&self, settings: &serde_json::Value) -> LintInvalidation {
        let parsed = LintConfig::from_settings(settings);
        let warning = parsed
            .as_ref()
            .err()
            .map(|message| format!("bend-lint configuration error: {message}"));
        self.diagnostics
            .lint
            .invalidate(Some(parsed.unwrap_or_default()), warning)
    }

    pub(super) fn invalidate_lint(&self) -> LintInvalidation {
        self.diagnostics.lint.invalidate(None, None)
    }

    pub(super) fn invalidate_lint_inputs<'a>(
        &self,
        uris: impl IntoIterator<Item = &'a Url>,
    ) -> LintInvalidation {
        // No retained inputs need filesystem identity checks. This also keeps
        // the default-disabled provider out of the ordinary typing hot path.
        if self.diagnostics.lint.results.read().is_empty() {
            return self.invalidate_lint();
        }
        let paths: Vec<_> = uris
            .into_iter()
            .filter_map(|uri| uri.to_file_path().ok())
            .map(|path| {
                let canonical = saved::input_identity(&path);
                (path, canonical)
            })
            .collect();
        self.diagnostics.lint.invalidate_entries(|_, result| {
            paths.iter().any(|(path, canonical)| {
                result.saved.covers_path(path)
                    || canonical
                        .as_ref()
                        .is_some_and(|path| result.saved.covers_path(path))
            })
        })
    }

    pub(super) fn close_lint(&self, uri: &Url) -> LintInvalidation {
        let invalidation = self
            .diagnostics
            .lint
            .invalidate_entries(|entry, _| entry == uri);
        self.diagnostics.lint.control.forget(uri);
        invalidation
    }

    // Lifecycle callers biased-join their original mutation before this cleanup:
    // no lint await may precede its revision reservation or operation fence.
    pub(super) async fn finish_lint_invalidation(&self, invalidation: LintInvalidation) {
        join_lint_task(invalidation.previous).await;
        self.end_lint_progress(invalidation.epoch, "Cancelled; save or run again")
            .await;
        if !invalidation.affected.is_empty() {
            self.publish_coordinated_diagnostics(invalidation.affected)
                .await;
        }
        if let Some(warning) = invalidation.warning {
            self.client
                .log_message(MessageType::WARNING, &warning)
                .await;
            self.client
                .show_message(MessageType::WARNING, warning)
                .await;
        }
    }

    pub(super) async fn shutdown_lint(&self) {
        self.diagnostics.lint.closed.store(true, Ordering::SeqCst);
        self.diagnostics.lint.semaphore.close();
        self.finish_lint_invalidation(self.invalidate_lint()).await;
    }

    pub(super) fn current_lint_result(&self, uri: &Url) -> Option<Arc<LintResult>> {
        let result = self.diagnostics.lint.results.read().get(uri)?;
        (*self.diagnostics.lint.config.read() == result.config
            && self.lint_revisions_current(&result.revisions))
        .then_some(result)
    }

    fn lint_revisions_current(&self, revisions: &[(FileId, u64)]) -> bool {
        let state = self.workspace.state.read();
        revisions.iter().all(|(id, generation)| {
            state.revisions.get(id).is_some_and(|sync| {
                let status = revision_result(sync.status());
                !status.is_pending() && status.generation() == *generation
            })
        })
    }

    fn lint_revision_fences(
        &self,
        saved: &saved::SavedProject,
        documents: &[Document],
    ) -> Vec<(FileId, u64)> {
        let state = self.workspace.state.read();
        documents
            .iter()
            .filter_map(|document| {
                let path = document.uri.to_file_path().ok()?;
                if !saved.covers_path(&path) {
                    return None;
                }
                let id = state.database.file_id_by_uri(&document.uri)?;
                let sync = state.revisions.get(&id)?;
                Some((id, revision_result(sync.status()).generation()))
            })
            .collect()
    }

    pub(super) fn lint_diagnostics(&self, uri: &Url) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        self.current_lint_result(uri)
            .and_then(|result| result.findings.by_uri.get(uri).cloned())
            .unwrap_or_default()
    }

    pub(super) async fn schedule_lint(&self, uri: Url) {
        let (config, epoch, previous_epoch, previous) = {
            let mut task = self.diagnostics.lint.task.write();
            let config = self.diagnostics.lint.config.read().clone();
            if !config.enabled || self.diagnostics.lint.closed.load(Ordering::SeqCst) {
                return;
            }
            let previous_epoch = self.diagnostics.lint.epoch.load(Ordering::SeqCst);
            let epoch = self.diagnostics.lint.next_epoch();
            let previous = task.take();
            if let Some(previous) = &previous {
                previous.abort();
                self.diagnostics
                    .lint
                    .control
                    .invalidate_running("Superseded by another saved-document check");
            }
            (config, epoch, previous_epoch, previous)
        };
        let schedule = async {
            join_lint_task(previous).await;
            self.end_lint_progress(previous_epoch, "Superseded by another check")
                .await;
            let mut task = self.diagnostics.lint.task.write();
            // An intervening mutation or consent update supersedes this save
            // while it waits for the old task to stop.
            if self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || self.diagnostics.lint.closed.load(Ordering::SeqCst)
                || *self.diagnostics.lint.config.read() != config
            {
                return;
            }
            let backend = self.clone();
            *task = Some(tokio::spawn(async move {
                let operation = async {
                    {
                        let _task = backend.diagnostics.lint.task.read();
                        if backend.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch {
                            return;
                        }
                        backend.diagnostics.lint.control.record(
                            &uri,
                            LintPhase::Running,
                            "Checking saved project inputs",
                        );
                    }
                    backend.begin_lint_progress(&uri, epoch).await;
                    if let Err(error) = backend.run_lint(uri.clone(), config, epoch).await {
                        let phase = if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::NotFound
                        ) {
                            LintPhase::Skipped
                        } else {
                            LintPhase::Failed
                        };
                        backend
                            .finish_lint_control(&uri, phase, &error.to_string(), epoch)
                            .await;
                    }
                };
                super::state::supervise(operation, || backend.diagnostics.report_fatal()).await;
            }));
        };
        schedule.await;
    }

    async fn run_lint(&self, uri: Url, config: LintConfig, epoch: u64) -> std::io::Result<()> {
        self.workspace.wait_for_discovery().await;
        let Some(document) = self.document(&uri).filter(Self::supported) else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Open a supported saved Bend document before checking",
                epoch,
            )
            .await;
            return Ok(());
        };
        let Some(file) = uri.to_file_path().ok() else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Save this document to a file before checking",
                epoch,
            )
            .await;
            return Ok(());
        };
        let Some(graph) = self.compiler_source_graph(uri.clone()).await else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Document inputs changed; save or run again",
                epoch,
            )
            .await;
            return Ok(());
        };
        let documents = self.workspace_documents();
        let reusable = self.diagnostics.lint.results.read().saved_for(&file);
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let identity_config = config.clone();
        let (saved, permit, identity, file, documents) = super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let saved = match reusable.filter(|saved| saved.reusable(&file, &graph, &documents))
                {
                    Some(saved) => saved,
                    None => Arc::new(saved::SavedProject::capture(&file, &graph, &documents)?),
                };
                Ok::<_, std::io::Error>((
                    saved,
                    permit,
                    installed_identity(&identity_config),
                    file,
                    documents,
                ))
            })
            .await,
        )?;
        if !self.diagnostics_snapshot_current(&uri, &document.snapshot)
            || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Document inputs changed during snapshot capture; save or run again",
            ));
        }
        // Index-only commits (for example a reloaded prelude) do not change
        // analyzer inputs. Fence their actual open revisions before execution;
        // final disk hashes and cancellation epochs remain independent checks.
        let revisions = self.lint_revision_fences(&saved, &documents);
        let affected = {
            let _task = self.diagnostics.lint.task.read();
            self.diagnostics
                .lint
                .results
                .write()
                .invalidate(|_, result| {
                    result.saved.root == saved.root && !Arc::ptr_eq(&result.saved, &saved)
                })
        };
        if !affected.is_empty() {
            self.publish_coordinated_diagnostics(affected).await;
        }
        self.client.log_message(MessageType::INFO, format!(
            "bend-lint: executing {} {:?}; external rule/compiler execution was explicitly enabled. {identity}. The analyzer's pinned Bend is independent of the selected LSP compiler.", config.path, config.arguments)).await;
        let output = process::run(
            &config,
            &saved.root,
            &file,
            self.compiler.reapers.clone(),
            permit,
        )
        .await?;
        self.finish_lint(document, config, epoch, revisions, saved, output)
            .await
    }

    async fn finish_lint(
        &self,
        document: Document,
        config: LintConfig,
        epoch: u64,
        revisions: Vec<(FileId, u64)>,
        saved: Arc<saved::SavedProject>,
        output: std::process::Output,
    ) -> std::io::Result<()> {
        let uri = &document.uri;
        if !output.stderr.is_empty() {
            let stderr = &output.stderr[..output.stderr.len().min(16 * 1024)];
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "bend-lint stderr (up to 16 KiB): {}",
                        String::from_utf8_lossy(stderr)
                    ),
                )
                .await;
        }
        let checked = saved.clone();
        let documents = self.workspace_documents();
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let (findings, unchanged) = super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let findings = wire::parse(&output, &checked)?;
                let unchanged = checked.unchanged() && checked.documents_saved(&documents);
                Ok::<_, std::io::Error>((findings, unchanged))
            })
            .await,
        )?;
        let _publish = self.diagnostics.imported_publish.lock().await;
        let affected = {
            // Serialize the final epoch check/result store with synchronous
            // invalidation; never hold this state guard across publication.
            let _task = self.diagnostics.lint.task.read();
            if !unchanged
                || !self.lint_revisions_current(&revisions)
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || !self.diagnostics_snapshot_current(uri, &document.snapshot)
            {
                None
            } else {
                let affected = self.diagnostics.lint.results.write().insert(
                    uri.clone(),
                    Arc::new(LintResult {
                        epoch,
                        revisions,
                        config,
                        saved,
                        findings,
                    }),
                );
                self.diagnostics.lint.control.record(
                    uri,
                    LintPhase::Ready,
                    "Saved-file analysis complete",
                );
                Some(affected)
            }
        };
        let Some(affected) = affected else {
            self.finish_lint_control(uri, LintPhase::Skipped, "Saved project inputs changed during analysis; save the inputs and run bend-lint again", epoch).await;
            return Ok(());
        };
        self.end_lint_progress(epoch, "Saved-file analysis complete")
            .await;
        self.publish_coordinated_locked(affected).await;
        Ok(())
    }
}

async fn join_lint_task(previous: Option<JoinHandle<()>>) {
    if let Some(previous) = previous
        && let Err(error) = previous.await
        && error.is_panic()
    {
        std::panic::resume_unwind(error.into_panic());
    }
}
