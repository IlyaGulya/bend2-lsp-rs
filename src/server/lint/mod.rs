mod actions;
mod process;
mod saved;
mod wire;
pub(super) use actions::DANGEROUS_KIND;

use super::{
    lsp::Backend,
    state::{State, revision_result},
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{sync::Semaphore, task::JoinHandle};
use tower_lsp::lsp_types::MessageType;
use url::Url;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct LintConfig {
    enabled: bool,
    path: String,
    arguments: Vec<String>,
    dangerous_code_actions: bool,
    timeout_ms: u64,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            arguments: Vec::new(),
            dangerous_code_actions: false,
            timeout_ms: 10_000,
        }
    }
}

impl LintConfig {
    fn from_settings(settings: &serde_json::Value) -> Result<Self, String> {
        let Some(value) = settings.pointer("/bend/lint") else {
            return Ok(Self::default());
        };
        let config: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("Invalid bend.lint settings: {error}"))?;
        if config.enabled {
            if !Path::new(&config.path).is_absolute() {
                return Err("bend.lint.path must be an explicit absolute executable path; bend-lint remains disabled".into());
            }
            let name = Path::new(&config.path)
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default();
            if matches!(name, "bunx" | "npx" | "npm" | "pnpm" | "yarn") {
                return Err("bend.lint.path must not invoke a package manager; install the analyzer yourself".into());
            }
            if Path::new(&config.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
                })
            {
                return Err("bend.lint.path must be a directly executable program, not a shell launcher; use an absolute Bun executable and lint.arguments for the installed script".into());
            }
            if config
                .arguments
                .iter()
                .any(|argument| argument.starts_with("--fix") || argument == "--worker")
            {
                return Err(
                    "bend.lint.arguments must not enable fixes or internal worker mode".into(),
                );
            }
            if !(1..=120_000).contains(&config.timeout_ms) {
                return Err("bend.lint.timeoutMs must be between 1 and 120000".into());
            }
        }
        Ok(config)
    }
}

pub(super) struct LintResult {
    epoch: u64,
    workspace_generation: u64,
    config: LintConfig,
    saved: Arc<saved::SavedProject>,
    findings: wire::Findings,
}

pub(super) struct LintService {
    config: State<LintConfig>,
    results: State<Option<Arc<LintResult>>>,
    task: State<Option<JoinHandle<()>>>,
    epoch: AtomicU64,
    closed: AtomicBool,
    semaphore: Arc<Semaphore>,
}

pub(super) struct LintInvalidation {
    previous: Option<JoinHandle<()>>,
    affected: HashSet<Url>,
    warning: Option<String>,
}

impl Default for LintService {
    fn default() -> Self {
        Self {
            config: State::new(LintConfig::default()),
            results: State::new(None),
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
        self.results
            .write()
            .take()
            .map_or_else(HashSet::new, |result| {
                result.findings.by_uri.keys().cloned().collect()
            })
    }

    fn invalidate(&self, config: Option<LintConfig>, warning: Option<String>) -> LintInvalidation {
        let mut task = self.task.write();
        if let Some(config) = config {
            *self.config.write() = config;
        }
        self.next_epoch();
        let previous = task.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        LintInvalidation {
            previous,
            affected: self.take_results(),
            warning,
        }
    }
}

impl Backend {
    pub(super) fn configure_lint(&self, settings: &serde_json::Value) -> LintInvalidation {
        let parsed = LintConfig::from_settings(settings);
        let warning = parsed.as_ref().err().cloned();
        self.diagnostics
            .lint
            .invalidate(Some(parsed.unwrap_or_default()), warning)
    }

    pub(super) fn invalidate_lint(&self) -> LintInvalidation {
        self.diagnostics.lint.invalidate(None, None)
    }

    // Lifecycle callers biased-join their original mutation before this cleanup:
    // no lint await may precede its revision reservation or operation fence.
    pub(super) async fn finish_lint_invalidation(&self, invalidation: LintInvalidation) {
        join_lint_task(invalidation.previous).await;
        if !invalidation.affected.is_empty() {
            self.publish_coordinated_diagnostics(invalidation.affected)
                .await;
        }
        if let Some(warning) = invalidation.warning {
            self.client.log_message(MessageType::WARNING, warning).await;
        }
    }

    pub(super) async fn shutdown_lint(&self) {
        self.diagnostics.lint.closed.store(true, Ordering::SeqCst);
        self.diagnostics.lint.semaphore.close();
        self.finish_lint_invalidation(self.invalidate_lint()).await;
    }

    pub(super) fn current_lint_result(&self) -> Option<Arc<LintResult>> {
        let result = self.diagnostics.lint.results.read().clone()?;
        let state = self.workspace.state.read();
        // A database commit changes generation, so indexed snapshots cannot
        // change underneath this result. Revision status also fences edits and
        // closes reserved before their database commit, without cloning every
        // open document on each diagnostics publication.
        (self.diagnostics.lint.epoch.load(Ordering::SeqCst) == result.epoch
            && state.generation() == result.workspace_generation
            && *self.diagnostics.lint.config.read() == result.config
            && state.revisions.iter().all(|(id, sync)| {
                let status = revision_result(sync.status());
                !status.is_pending()
                    && state
                        .database
                        .open_snapshot(*id)
                        .is_none_or(|snapshot| status.desired() == Some(snapshot.revision))
            }))
        .then_some(result)
    }

    pub(super) fn lint_diagnostics(&self, uri: &Url) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        self.current_lint_result()
            .and_then(|result| result.findings.by_uri.get(uri).cloned())
            .unwrap_or_default()
    }

    pub(super) async fn schedule_lint(&self, uri: Url) {
        let (config, epoch, previous, affected) = {
            let mut task = self.diagnostics.lint.task.write();
            let config = self.diagnostics.lint.config.read().clone();
            if !config.enabled || self.diagnostics.lint.closed.load(Ordering::SeqCst) {
                return;
            }
            let epoch = self.diagnostics.lint.next_epoch();
            let previous = task.take();
            if let Some(previous) = &previous {
                previous.abort();
            }
            (
                config,
                epoch,
                previous,
                self.diagnostics.lint.take_results(),
            )
        };
        let schedule = async {
            join_lint_task(previous).await;
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
                    if let Err(error) = backend.run_lint(uri, config, epoch).await {
                        backend
                            .client
                            .log_message(MessageType::WARNING, format!("bend-lint failed: {error}"))
                            .await;
                    }
                };
                super::state::supervise(operation, || backend.diagnostics.report_fatal()).await;
            }));
        };
        let clear = async {
            if !affected.is_empty() {
                self.publish_coordinated_diagnostics(affected).await;
            }
        };
        tokio::join!(biased; schedule, clear);
    }

    async fn run_lint(&self, uri: Url, config: LintConfig, epoch: u64) -> std::io::Result<()> {
        self.workspace.wait_for_discovery().await;
        let Some(document) = self.document(&uri).filter(Self::supported) else {
            return Ok(());
        };
        let Some(file) = uri.to_file_path().ok() else {
            return Ok(());
        };
        let Some(graph) = self.compiler_source_graph(uri.clone()).await else {
            return Ok(());
        };
        let generation = self.workspace.generation();
        let documents = self.workspace_documents();
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let identity_config = config.clone();
        let (saved, permit, identity) = super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let saved = saved::SavedProject::capture(&file, &graph, &documents)?;
                Ok::<_, std::io::Error>((saved, permit, installed_identity(&identity_config)))
            })
            .await,
        )?;
        if !self.diagnostics_snapshot_current(&uri, &document.snapshot)
            || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
        {
            return Ok(());
        }
        self.client.log_message(MessageType::INFO, format!(
            "bend-lint: executing {} {:?}; external rule/compiler execution was explicitly enabled. {identity}. The analyzer's pinned Bend is independent of the selected LSP compiler.", config.path, config.arguments)).await;
        let output = process::run(
            &config,
            &saved.root,
            &saved.entry,
            self.compiler.reapers.clone(),
            permit,
        )
        .await?;
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
        let saved = Arc::new(saved);
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
                let unchanged =
                    checked.unchanged() && saved::SavedProject::documents_saved(&documents);
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
                || generation != self.workspace.generation()
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || !self.diagnostics_snapshot_current(&uri, &document.snapshot)
            {
                return Ok(());
            }
            let affected = findings.by_uri.keys().cloned().collect();
            *self.diagnostics.lint.results.write() = Some(Arc::new(LintResult {
                epoch,
                workspace_generation: generation,
                config,
                saved,
                findings,
            }));
            affected
        };
        self.publish_coordinated_locked(affected).await;
        Ok(())
    }
}

fn installed_identity(config: &LintConfig) -> String {
    use std::io::Read;
    let installed = std::iter::once(&config.path)
        .chain(&config.arguments)
        .map(Path::new)
        .filter(|path| path.is_absolute() && path.file_name().is_some_and(|name| name == "seam.ts"))
        .find_map(|script| {
            let package = script.parent()?.parent()?.parent()?.join("package.json");
            let mut text = String::new();
            std::fs::File::open(package)
                .ok()?
                .take(64 * 1024)
                .read_to_string(&mut text)
                .ok()?;
            let metadata: serde_json::Value = serde_json::from_str(&text).ok()?;
            (metadata["name"] == "bend-lint").then(|| {
                serde_json::json!({
                    "version":metadata["version"], "bendRelease":metadata["bendRelease"],
                    "bendWindows":metadata["bendWindows"],
                })
            })
        });
    installed.map_or_else(
        || "Tool/compiler versions unknown: upstream JSON contains no version identity".into(),
        |metadata| {
            format!("Installed bend-lint package metadata (not executable attestation): {metadata}")
        },
    )
}

async fn join_lint_task(previous: Option<JoinHandle<()>>) {
    if let Some(previous) = previous
        && let Err(error) = previous.await
        && error.is_panic()
    {
        std::panic::resume_unwind(error.into_panic());
    }
}
