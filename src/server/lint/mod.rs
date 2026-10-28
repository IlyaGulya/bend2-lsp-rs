mod actions;
mod config;
pub(super) mod control;
mod lifecycle;
mod process;
mod project;
mod results;
mod scheduler;
mod snapshot;
mod util;
mod wire;
pub(super) use actions::DANGEROUS_KIND;
use config::LintConfig;
pub(in crate::server) use lifecycle::LintChange;
use project::{PreparationPhase, ProjectKey, ProjectState};
pub(super) use scheduler::LintService;

use super::lsp::Backend;
use crate::workspace::Document;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};
use url::Url;

const WORKSPACE_REASON: &str = "Workspace folders changed; the next check will re-read them";
const AFFECTED_REASON: &str = "Inputs changed; the affected results will be rechecked";
const SUPERSEDED_REASON: &str = "Superseded by newer editor changes";
const CANCELLED_REASON: &str = "Cancelled; the current inputs will be rechecked";

pub(super) struct LintResult {
    epoch: u64,
    project: ProjectKey,
    // Project revision that produced this entry. Cleanup may only retire
    // results from revisions an older plan actually fenced.
    revision: u64,
    revisions: Vec<(Url, u64)>,
    config: LintConfig,
    snapshot: Arc<snapshot::ProjectSnapshot>,
    findings: wire::Findings,
}

// One coherent open-input read: the immutable document snapshots and their
// revision generations are sampled under the same workspace lock, so no fence
// is ever sampled after the capture it describes. A later change advances the
// stamp and fences the result; publication additionally compares the complete
// current open-input view with the captured one.
struct LintInputs {
    documents: Vec<Document>,
    generations: HashMap<Url, u64>,
}

impl LintInputs {
    fn fences(&self, view: &snapshot::ProjectSnapshot) -> Vec<(Url, u64)> {
        self.documents
            .iter()
            .filter_map(|document| {
                let path = document.uri.to_file_path().ok()?;
                // Identity is resolved once for this document; the pairing
                // itself stays in the shared read-set predicate.
                let identity = snapshot::paths::input_identity(&path);
                if !view.covers(&path, identity.as_deref()) {
                    return None;
                }
                self.generations
                    .get(&document.uri)
                    .map(|generation| (document.uri.clone(), *generation))
            })
            .collect()
    }
}

impl Backend {
    pub(super) fn current_lint_result(&self, uri: &Url) -> Option<Arc<LintResult>> {
        self.current_lint_contributors(uri).into_iter().next()
    }

    // Every current entry that indexes this document, newest first. Status and
    // partial semantics use the first (primary) entry; diagnostics, fixes, and
    // invalidation must consider all of them.
    fn current_lint_contributors(&self, uri: &Url) -> Vec<Arc<LintResult>> {
        let contributors = self.diagnostics.lint.results.read().contributors(uri);
        if contributors.is_empty() {
            return contributors;
        }
        let documents = self.workspace_documents();
        // One revision snapshot for every contributor: the project fence is
        // sampled once, not once per retained entry.
        let projects = self.diagnostics.lint.projects.read();
        contributors
            .into_iter()
            .filter(|result| self.lint_result_current_in(result, &documents, &projects))
            .collect()
    }

    // Publication and retained-result use share this fence: the immutable
    // captured view must still be the current complete open-input view, not
    // only the root document or the generations sampled with the capture.
    //
    // The declaration order keeps the project revision first: a project fence
    // makes the retired revision's results ineligible immediately, so an older
    // contributor, fix, or status can never outlive the mutation that retired
    // it while that mutation is still awaiting. Ownership must be live and
    // exact: a result whose project was pruned, or whose recorded revision is
    // gone, is orphaned and fails closed.
    fn lint_result_current_in(
        &self,
        result: &LintResult,
        documents: &[Document],
        projects: &HashMap<ProjectKey, ProjectState>,
    ) -> bool {
        projects
            .get(&result.project)
            .is_some_and(|state| state.revision == result.revision)
            && *self.diagnostics.lint.config.read() == result.config
            && self.lint_revisions_current(&result.revisions)
            && result.snapshot.open_documents_current(documents)
    }

    fn lint_revisions_current(&self, revisions: &[(Url, u64)]) -> bool {
        let state = self.workspace.state.read();
        revisions
            .iter()
            .all(|(uri, generation)| state.revision_current(uri, *generation))
    }

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_inputs", result_count = tracing::field::Empty))]
    fn lint_inputs(&self) -> LintInputs {
        let state = self.workspace.state.read();
        let documents = state.database.open_documents();
        tracing::Span::current().record("result_count", documents.len());
        let generations = documents
            .iter()
            .filter_map(|document| {
                state
                    .revision_generation(&document.uri)
                    .map(|generation| (document.uri.clone(), generation))
            })
            .collect();
        LintInputs {
            documents,
            generations,
        }
    }

    pub(super) fn lint_scheduling_enabled(&self) -> bool {
        self.diagnostics.lint.config.read().enabled
            && !self.diagnostics.lint.closed.load(Ordering::SeqCst)
    }

    pub(super) fn lint_diagnostics(&self, uri: &Url) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let mut combined = Vec::new();
        for result in self.current_lint_contributors(uri) {
            let Some(items) = result.findings.by_uri.get(uri) else {
                continue;
            };
            for item in items {
                if !combined.contains(item) {
                    combined.push(item.clone());
                }
            }
        }
        combined
    }

    pub(super) fn lint_tool_identity(&self, config: &LintConfig, uri: &Url) -> String {
        let key = Self::project_key(uri);
        let projects = self.diagnostics.lint.projects.read();
        match projects
            .get(&key)
            .and_then(|state| state.preparation.as_ref())
        {
            Some(entry) if entry.key.config == *config => match &entry.phase {
                PreparationPhase::Ready(description) => description.clone(),
                PreparationPhase::Running => "Preparing the analyzer and its pinned compiler; the identity is reported when preparation completes.".into(),
                PreparationPhase::Failed(message) => message.clone(),
            },
            _ => "Not prepared yet; the first check validates the analyzer contract and reports its identity.".into(),
        }
    }
}

// Pairing-only coverage: the caller resolves each event spelling's identity
// once, and the read-set semantics stay in the shared snapshot predicate.
fn covered_by(view: &snapshot::ProjectSnapshot, spellings: &[(PathBuf, Option<PathBuf>)]) -> bool {
    spellings
        .iter()
        .any(|(path, identity)| view.covers(path, identity.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::{
        Backend, DANGEROUS_KIND, LintChange, LintConfig, LintResult,
        control::LintPhase,
        project::{Preparation, PreparationKey, PreparationPhase, PreparationToken, ProjectState},
        scheduler::PREPARATION_GATE,
        snapshot, wire,
    };
    use crate::server::{
        compiler::CompilerReapers, diagnostics::DiagnosticsService,
        workspace_service::WorkspaceService,
    };
    use std::{
        collections::HashMap,
        error::Error,
        fs, io,
        path::PathBuf,
        sync::{Arc, atomic::Ordering},
    };
    use tokio::sync::{Notify, Semaphore};
    use tower_lsp::{
        LanguageServer, LspService,
        lsp_types::{
            CodeAction, CodeActionOrCommand, CodeActionParams, Diagnostic, DiagnosticSeverity,
            DocumentChanges, ExecuteCommandParams, OneOf, Position, Range,
        },
    };
    use url::Url;

    const MAIN: &str = "import ./dep.bend as Dep\ndef main = Dep.value\n";
    const DEP: &str = "def value = 1\n";
    const CHANGED_DEP: &str = "def value = 2\n";
    const NEW_INPUT: &str = "def extra = 2\n";

    struct Project {
        directory: tempfile::TempDir,
        main: PathBuf,
        dep: PathBuf,
        main_uri: Url,
        dep_uri: Url,
    }

    fn project() -> Result<Project, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let main = directory.path().join("main.bend");
        let dep = directory.path().join("dep.bend");
        fs::write(&main, MAIN)?;
        fs::write(&dep, DEP)?;
        fs::write(directory.path().join("bend-lint.json"), "{\"rules\":{}}")?;
        let main_uri =
            Url::from_file_path(&main).map_err(|()| io::Error::other("main file URI"))?;
        let dep_uri =
            Url::from_file_path(&dep).map_err(|()| io::Error::other("dependency file URI"))?;
        Ok(Project {
            directory,
            main,
            dep,
            main_uri,
            dep_uri,
        })
    }

    // A retained result only exists after the provider is opted in with a
    // configured executable. The test binary is that absolute executable; no
    // check is scheduled here, so it is never invoked.
    fn lint_config() -> Result<LintConfig, Box<dyn Error>> {
        let path = std::env::current_exe()?
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::other("the test executable path is not UTF-8"))?;
        Ok(LintConfig {
            enabled: true,
            dangerous_code_actions: true,
            path,
            ..LintConfig::default()
        })
    }

    fn backend() -> Result<Backend, Box<dyn Error>> {
        let workspace = Arc::new(WorkspaceService {
            staging: Arc::new(Semaphore::new(1)),
            ..WorkspaceService::default()
        });
        let (service, _socket) = LspService::new(|client| {
            Backend::new(
                client,
                workspace,
                Arc::new(DiagnosticsService::default()),
                Arc::new(CompilerReapers::default()),
            )
        });
        let backend = service.inner().clone();
        *backend.diagnostics.lint.config.write() = lint_config()?;
        Ok(backend)
    }

    async fn open(
        backend: &Backend,
        uri: &Url,
        version: i32,
        text: &str,
        path: PathBuf,
    ) -> Result<(), Box<dyn Error>> {
        backend
            .open_workspace_text(uri.clone(), "bend".into(), version, text.into(), Some(path))
            .await
            .ok_or_else(|| io::Error::other("the open document should commit"))?;
        Ok(())
    }

    fn fence(backend: &Backend, uri: &Url) -> Result<(Url, u64), Box<dyn Error>> {
        let generation = backend
            .workspace
            .state
            .read()
            .revision_generation(uri)
            .ok_or_else(|| io::Error::other("open input revision"))?;
        Ok((uri.clone(), generation))
    }

    fn retain(
        backend: &Backend,
        uri: &Url,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(Url, u64)>,
    ) {
        let epoch = backend.diagnostics.lint.epoch.load(Ordering::SeqCst);
        retain_entry(
            backend,
            uri,
            entry(backend, epoch, snapshot, revisions, no_findings()),
        );
    }

    fn diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("bend-lint".into()),
            message: message.into(),
            ..Diagnostic::default()
        }
    }

    fn no_findings() -> wire::Findings {
        wire::Findings {
            by_uri: HashMap::new(),
            actionable: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn fixture(
        backend: &Backend,
        project: &Project,
    ) -> Result<Arc<snapshot::ProjectSnapshot>, Box<dyn Error>> {
        let captured = backend.workspace_documents();
        Ok(Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?))
    }

    fn entry(
        backend: &Backend,
        epoch: u64,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(Url, u64)>,
        findings: wire::Findings,
    ) -> Arc<LintResult> {
        entry_at(backend, epoch, 0, snapshot, revisions, findings)
    }

    fn entry_at(
        backend: &Backend,
        epoch: u64,
        revision: u64,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(Url, u64)>,
        findings: wire::Findings,
    ) -> Arc<LintResult> {
        Arc::new(LintResult {
            epoch,
            project: Backend::root_key(&snapshot.root),
            revision,
            revisions,
            config: backend.diagnostics.lint.config.read().clone(),
            snapshot: snapshot.clone(),
            findings,
        })
    }

    // A retained result is current only for the live project revision that
    // produced it, so a fixture registers that legitimate ownership instead of
    // relying on a missing project state. A test that already fenced the
    // project keeps its recorded revision.
    fn retain_entry(backend: &Backend, uri: &Url, result: Arc<LintResult>) {
        let mut projects = backend.diagnostics.lint.projects.write();
        if !projects.contains_key(&result.project) {
            projects.insert(
                result.project.clone(),
                ProjectState {
                    revision: result.revision,
                    ..ProjectState::default()
                },
            );
        }
        drop(projects);
        backend
            .diagnostics
            .lint
            .results
            .write()
            .insert(uri.clone(), result);
    }

    fn actionable(message: &str, uri: &Url) -> wire::Findings {
        let reported = diagnostic(message);
        let mut findings = no_findings();
        findings.by_uri.insert(uri.clone(), vec![reported.clone()]);
        findings.actionable.push(wire::ActionableFinding {
            uri: uri.clone(),
            diagnostic: reported,
            fixes: vec![wire::Fix {
                title: format!("Apply {message}"),
                applicability: "dangerous".into(),
                edits: vec![wire::Edit {
                    path: "main.bend".into(),
                    range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                    text: "def identity: U32".into(),
                }],
            }],
        });
        findings
    }

    fn dangerous_params(uri: &Url) -> Result<CodeActionParams, Box<dyn Error>> {
        Ok(serde_json::from_value(serde_json::json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "context": {"diagnostics": [], "only": [DANGEROUS_KIND], "triggerKind": 1}
        }))?)
    }

    fn dangerous_actions(actions: &[CodeActionOrCommand]) -> Vec<&CodeAction> {
        actions
            .iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(action)
                    if action
                        .kind
                        .as_ref()
                        .is_some_and(|kind| kind.as_str() == DANGEROUS_KIND) =>
                {
                    Some(action)
                }
                _ => None,
            })
            .collect()
    }

    // A dependency changes after the immutable capture but before publication.
    // Sampling generations after the capture pairs older text with the newer
    // generation, so the retained entry must still be rejected.
    #[tokio::test]
    async fn captured_text_cannot_be_retained_under_a_newer_dependency_generation()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        open(
            &backend,
            &project.dep_uri,
            2,
            CHANGED_DEP,
            project.dep.clone(),
        )
        .await?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain(&backend, &project.main_uri, &snapshot, revisions);
        assert!(
            backend.current_lint_result(&project.main_uri).is_none(),
            "captured dependency text outlived the newer generation it was paired with"
        );
        Ok(())
    }

    // A covered input opens after the capture and is absent from the captured
    // fence list entirely. Its unsaved text was never analyzed, so the retained
    // entry is not current.
    #[tokio::test]
    async fn newly_opened_covered_input_invalidates_a_retained_result() -> Result<(), Box<dyn Error>>
    {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        let revisions = vec![fence(&backend, &project.main_uri)?];
        retain(&backend, &project.main_uri, &snapshot, revisions);
        let extra = project
            .main
            .parent()
            .ok_or_else(|| io::Error::other("project root"))?
            .join("extra.bend");
        let extra_uri =
            Url::from_file_path(&extra).map_err(|()| io::Error::other("new input URI"))?;
        open(&backend, &extra_uri, 1, NEW_INPUT, extra).await?;
        assert!(
            backend.current_lint_result(&project.main_uri).is_none(),
            "a covered input opened after the capture left the older result current"
        );
        Ok(())
    }

    // Fences sampled together with the immutable capture cannot describe a
    // generation that only exists after those snapshots were taken.
    #[tokio::test]
    async fn capture_time_fences_reject_a_dependency_changed_after_sampling()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let inputs = backend.lint_inputs();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &inputs.documents,
        )?);
        let revisions = inputs.fences(&snapshot);
        assert!(revisions.contains(&fence(&backend, &project.dep_uri)?));
        open(
            &backend,
            &project.dep_uri,
            2,
            CHANGED_DEP,
            project.dep.clone(),
        )
        .await?;
        assert!(
            !backend.lint_revisions_current(&revisions),
            "fences sampled with the capture described a later generation"
        );
        Ok(())
    }

    // Currentness is scoped to the captured project view: other projects and
    // refreshed unrelated inputs must not retire a valid result.
    #[tokio::test]
    async fn unrelated_open_inputs_keep_a_retained_result_current() -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        let revisions = vec![fence(&backend, &project.main_uri)?];
        retain(&backend, &project.main_uri, &snapshot, revisions);
        let other = tempfile::tempdir()?;
        let other_path = other.path().join("other.bend");
        fs::write(&other_path, NEW_INPUT)?;
        let other_uri =
            Url::from_file_path(&other_path).map_err(|()| io::Error::other("other file URI"))?;
        open(&backend, &other_uri, 1, NEW_INPUT, other_path.clone()).await?;
        open(&backend, &other_uri, 2, NEW_INPUT, other_path).await?;
        assert!(
            backend.current_lint_result(&project.main_uri).is_some(),
            "an unrelated project input retired a still-current result"
        );
        Ok(())
    }

    // Every current entry that indexes the target contributes. An empty own
    // report at a newer epoch must not erase the sibling entry's import.
    #[tokio::test]
    async fn an_older_imported_contribution_survives_a_newer_empty_own_result()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let imported = diagnostic("imported rule");
        let mut imported_findings = no_findings();
        imported_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![imported.clone()]);
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, no_findings()),
        );
        assert_eq!(
            backend.lint_diagnostics(&project.main_uri),
            vec![imported],
            "a newer empty own report must not erase a current imported contribution"
        );
        assert!(
            backend
                .current_lint_result(&project.main_uri)
                .is_some_and(|result| result.epoch == 2),
            "status must keep the newest own entry as the primary result"
        );
        Ok(())
    }

    // Distinct contributions from two current entries are published together,
    // and an identical diagnostic reported by both appears once.
    #[tokio::test]
    async fn distinct_contributions_from_two_entries_union_and_dedup() -> Result<(), Box<dyn Error>>
    {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let own = diagnostic("own rule");
        let imported = diagnostic("imported rule");
        let shared = diagnostic("shared rule");
        let mut own_findings = no_findings();
        own_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![own.clone(), shared.clone()]);
        let mut imported_findings = no_findings();
        imported_findings.by_uri.insert(
            project.main_uri.clone(),
            vec![imported.clone(), shared.clone()],
        );
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, own_findings),
        );
        let diagnostics = backend.lint_diagnostics(&project.main_uri);
        assert_eq!(
            diagnostics.len(),
            3,
            "both current contributors must be published exactly once"
        );
        for expected in [&own, &imported, &shared] {
            assert!(
                diagnostics.contains(expected),
                "missing contributor diagnostic"
            );
        }
        assert_eq!(
            diagnostics.iter().filter(|item| *item == &shared).count(),
            1,
            "an identical diagnostic must be deduplicated"
        );
        Ok(())
    }

    // Replacing one contributor with a clean report clears only that entry's
    // contribution; the other entry's findings stay published.
    #[tokio::test]
    async fn replacing_a_contributor_with_an_empty_report_clears_only_its_findings()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let own = diagnostic("own rule");
        let imported = diagnostic("imported rule");
        let mut own_findings = no_findings();
        own_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![own.clone()]);
        let mut imported_findings = no_findings();
        imported_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![imported.clone()]);
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions.clone(), own_findings),
        );
        let initial = backend.lint_diagnostics(&project.main_uri);
        assert!(initial.contains(&own) && initial.contains(&imported));
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 3, &snapshot, revisions, no_findings()),
        );
        assert_eq!(
            backend.lint_diagnostics(&project.main_uri),
            vec![own],
            "only the replaced contributor's finding may clear"
        );
        Ok(())
    }

    // The imported finding's fix stays available, versioned to the analyzed
    // document, even while the newer own entry reports nothing.
    #[tokio::test]
    async fn an_imported_dangerous_fix_survives_a_newer_empty_own_result()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions.clone(),
                actionable("imported rule", &project.main_uri),
            ),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, no_findings()),
        );
        let params = dangerous_params(&project.main_uri)?;
        let actions = backend.lint_code_actions(&params).await;
        let dangerous = dangerous_actions(&actions);
        let action = dangerous
            .first()
            .copied()
            .ok_or_else(|| io::Error::other("the imported dangerous fix must stay available"))?;
        assert_eq!(
            action.diagnostics.as_ref(),
            Some(&vec![diagnostic("imported rule")]),
            "the action must keep the importing authority's diagnostic"
        );
        let changes = action
            .edit
            .as_ref()
            .and_then(|edit| edit.document_changes.as_ref())
            .ok_or_else(|| io::Error::other("the dangerous fix must carry edits"))?;
        let DocumentChanges::Edits(edits) = changes else {
            return Err(io::Error::other("the dangerous fix must use text edits").into());
        };
        let targeted = edits
            .iter()
            .find(|edit| edit.text_document.uri == project.main_uri)
            .ok_or_else(|| io::Error::other("the imported fix must target the root document"))?;
        assert_eq!(
            targeted.text_document.version,
            Some(1),
            "the fix must stay versioned to the analyzed document"
        );
        assert_eq!(targeted.edits.len(), 1);
        assert!(
            matches!(&targeted.edits[0], OneOf::Left(edit) if edit.new_text == "def identity: U32")
        );
        Ok(())
    }

    // The same fix reported by two current entries is offered once.
    #[tokio::test]
    async fn identical_imported_fixes_from_two_entries_are_deduplicated()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions.clone(),
                actionable("shared rule", &project.main_uri),
            ),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(
                &backend,
                2,
                &snapshot,
                revisions,
                actionable("shared rule", &project.main_uri),
            ),
        );
        let params = dangerous_params(&project.main_uri)?;
        let actions = backend.lint_code_actions(&params).await;
        assert_eq!(
            dangerous_actions(&actions).len(),
            1,
            "identical fixes from two current entries must be deduplicated"
        );
        Ok(())
    }

    async fn status_state(backend: &Backend, uri: &Url) -> Result<String, Box<dyn Error>> {
        let result = backend
            .execute_command(ExecuteCommandParams {
                command: "bend2.lint.status".into(),
                arguments: vec![serde_json::json!({"uri": uri})],
                work_done_progress_params: tower_lsp::lsp_types::WorkDoneProgressParams::default(),
            })
            .await?;
        Ok(result
            .and_then(|value| value["state"].as_str().map(str::to_owned))
            .unwrap_or_default())
    }

    fn record_ready(backend: &Backend, uri: &Url, reason: &str, revision: u64) {
        backend
            .diagnostics
            .lint
            .control
            .record(uri, LintPhase::Ready, reason, revision);
    }

    fn versioned_action(
        actions: &[CodeActionOrCommand],
        uri: &Url,
        version: i32,
        message: &str,
    ) -> bool {
        dangerous_actions(actions).iter().any(|action| {
            action.diagnostics.as_ref().is_some_and(|diagnostics| {
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message == message)
            }) && action
                .edit
                .as_ref()
                .and_then(|edit| edit.document_changes.as_ref())
                .is_some_and(|changes| match changes {
                    DocumentChanges::Edits(edits) => edits.iter().any(|edit| {
                        edit.text_document.uri == *uri
                            && edit.text_document.version == Some(version)
                    }),
                    DocumentChanges::Operations(_) => false,
                })
        })
    }

    fn pause_committed_close(
        backend: Backend,
        uri: Url,
        reached: Arc<Notify>,
        release: Arc<Notify>,
    ) -> tokio::task::JoinHandle<bool> {
        let mutation = backend.clone();
        let close_uri = uri.clone();
        tokio::spawn(async move {
            backend
                .apply_lint_change(LintChange::Close { uri: close_uri }, async move {
                    let Some(closed) = mutation.workspace.begin_close(&uri) else {
                        return false;
                    };
                    if !mutation.close_workspace_document(uri, closed).await {
                        return false;
                    }
                    reached.notify_one();
                    release.notified().await;
                    true
                })
                .await
        })
    }

    // A committed mutation can await before its plan finishes. A newer
    // revision that publishes during that window owns its versioned action and
    // status; the older plan's cleanup may only retire results from its own
    // revision.
    #[tokio::test]
    async fn late_close_cleanup_keeps_a_newer_versioned_action_and_ready_status()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        // The old plan must have a real same-project result to own and replace.
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.main_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions,
                actionable("older rule", &project.main_uri),
            ),
        );
        let params = dangerous_params(&project.main_uri)?;
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                1,
                "older rule"
            ),
            "the older revision's versioned action exists before the close starts"
        );
        let reached = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let close = pause_committed_close(
            backend.clone(),
            project.main_uri.clone(),
            Arc::clone(&reached),
            Arc::clone(&release),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified()).await?;
        // The close is committed and the older plan is paused before its
        // cleanup. Reopen through the real workspace callback, then complete a
        // newer revision over the current open view.
        open(&backend, &project.main_uri, 3, MAIN, project.main.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let key = Backend::root_key(&snapshot.root);
        let newer_revision = backend
            .diagnostics
            .lint
            .project_revision(&key)
            .ok_or_else(|| io::Error::other("project revision"))?;
        retain_entry(
            &backend,
            &project.main_uri,
            entry_at(
                &backend,
                2,
                newer_revision,
                &snapshot,
                revisions,
                actionable("newer rule", &project.main_uri),
            ),
        );
        record_ready(
            &backend,
            &project.main_uri,
            "newer analysis",
            newer_revision,
        );
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                3,
                "newer rule"
            ),
            "the newer revision's versioned action is observable before the older plan finishes"
        );
        assert_eq!(status_state(&backend, &project.main_uri).await?, "ready");
        release.notify_one();
        let applied = close.await?;
        assert!(applied);
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                3,
                "newer rule"
            ),
            "late cleanup from an older plan must not delete the newer versioned action"
        );
        assert_eq!(status_state(&backend, &project.main_uri).await?, "ready");
        Ok(())
    }

    // A watcher plan fences its projects synchronously and only then awaits the
    // mutation. While that plan still awaits, the retired revision's
    // contributor, fix and status must already be ineligible; a newer
    // publication that lands in that window owns the visible result, and the
    // older plan's cleanup may not remove it.
    #[tokio::test]
    async fn a_staged_watcher_fence_hides_retired_results_before_cleanup_and_keeps_a_newer_publication()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![fence(&backend, &project.main_uri)?];
        retain_entry(
            &backend,
            &project.main_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions.clone(),
                actionable("older rule", &project.main_uri),
            ),
        );
        record_ready(&backend, &project.main_uri, "older completion", 0);
        let params = dangerous_params(&project.main_uri)?;
        let actions = backend.lint_code_actions(&params).await;
        assert!(
            versioned_action(&actions, &project.main_uri, 1, "older rule"),
            "the retained revision's fix exists before the fence"
        );
        let reached = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let plan = {
            let backend = backend.clone();
            let reached = Arc::clone(&reached);
            let release = Arc::clone(&release);
            let config = project.directory.path().join("bend-lint.json");
            tokio::spawn(async move {
                backend
                    .apply_lint_change(LintChange::Watched(vec![config]), async move {
                        reached.notify_one();
                        release.notified().await;
                        true
                    })
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified()).await?;
        // Only the fence has run: this plan's cleanup is still awaiting.
        assert!(
            backend.current_lint_result(&project.main_uri).is_none(),
            "a retired revision's contributor is ineligible before its cleanup runs"
        );
        assert!(backend.lint_diagnostics(&project.main_uri).is_empty());
        let actions = backend.lint_code_actions(&params).await;
        assert!(
            !versioned_action(&actions, &project.main_uri, 1, "older rule"),
            "a retired revision's fix is ineligible before its cleanup runs"
        );
        assert_ne!(status_state(&backend, &project.main_uri).await?, "ready");
        let key = Backend::root_key(&snapshot.root);
        let newer_revision = backend
            .diagnostics
            .lint
            .project_revision(&key)
            .ok_or_else(|| io::Error::other("project revision"))?;
        retain_entry(
            &backend,
            &project.main_uri,
            entry_at(
                &backend,
                2,
                newer_revision,
                &snapshot,
                revisions,
                actionable("newer rule", &project.main_uri),
            ),
        );
        record_ready(
            &backend,
            &project.main_uri,
            "newer analysis",
            newer_revision,
        );
        let actions = backend.lint_code_actions(&params).await;
        assert!(
            versioned_action(&actions, &project.main_uri, 1, "newer rule"),
            "a concurrent newer publication owns the visible result"
        );
        assert_eq!(status_state(&backend, &project.main_uri).await?, "ready");
        release.notify_one();
        let applied = plan.await?;
        assert!(applied);
        let current = backend.current_lint_result(&project.main_uri);
        assert_eq!(
            current.map(|result| result.revision),
            Some(newer_revision),
            "the older plan's cleanup preserves the newer publication"
        );
        let actions = backend.lint_code_actions(&params).await;
        assert!(
            versioned_action(&actions, &project.main_uri, 1, "newer rule"),
            "late cleanup from the retired plan must not delete the newer fix"
        );
        Ok(())
    }

    // Freshness invariant: a retained result whose owning project state is gone
    // is orphaned. It must not be served as a current contributor, fix, or
    // status even though its captured view and open-input generations still
    // match, because nothing will ever retire it again.
    #[tokio::test]
    async fn an_orphaned_retained_result_is_not_current() -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![fence(&backend, &project.main_uri)?];
        let orphan = entry(
            &backend,
            1,
            &snapshot,
            revisions,
            actionable("orphan rule", &project.main_uri),
        );
        backend
            .diagnostics
            .lint
            .results
            .write()
            .insert(project.main_uri.clone(), orphan);
        record_ready(&backend, &project.main_uri, "orphan completion", 0);
        assert!(backend.current_lint_result(&project.main_uri).is_none());
        assert!(backend.lint_diagnostics(&project.main_uri).is_empty());
        let params = dangerous_params(&project.main_uri)?;
        assert!(
            !versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                1,
                "orphan rule"
            ),
            "an orphaned result's fix is not offered"
        );
        assert_ne!(status_state(&backend, &project.main_uri).await?, "ready");
        Ok(())
    }

    // Regression: a preparation worker can really publish its completion before
    // the attempt that owns it records its handle. The waiting check must then
    // settle with that completion instead of remaining Running with no worker
    // that could still publish.
    #[tokio::test]
    async fn an_immediately_failed_preparation_settles_the_waiting_check()
    -> Result<(), Box<dyn Error>> {
        let backend = backend()?;
        let workspace = tempfile::tempdir()?;
        let config = LintConfig {
            path: workspace
                .path()
                .join("absent-bend-lint")
                .to_string_lossy()
                .into_owned(),
            ..lint_config()?
        };
        let key = PreparationKey {
            root: workspace.path().to_path_buf(),
            config,
            inputs: [0; 32],
        };
        let project = Backend::root_key(workspace.path());
        backend
            .diagnostics
            .lint
            .projects
            .write()
            .insert(project.clone(), ProjectState::default());
        let mut changed = backend.diagnostics.lint.preparation_changed.subscribe();
        PREPARATION_GATE.arm(&project);
        let waiting = {
            let backend = backend.clone();
            let key = key.clone();
            let project = project.clone();
            tokio::spawn(async move { backend.ensure_prepared(&project, 0, key).await })
        };
        // The real worker fails while this attempt's registration is held.
        tokio::time::timeout(std::time::Duration::from_secs(5), changed.changed()).await??;
        PREPARATION_GATE.disarm();
        let Ok(settled) = tokio::time::timeout(std::time::Duration::from_secs(5), waiting).await
        else {
            panic!("the waiting check settles instead of waiting for a worker that cannot publish");
        };
        let prepared = settled?;
        assert!(
            prepared.is_err(),
            "the check reports the preparation failure: {prepared:?}"
        );
        Ok(())
    }

    // A completed older attempt must never overwrite the newer attempt that
    // replaced it, even when both use the same preparation key: the identity a
    // check and the status surface see has to stay the newer preparation's.
    #[tokio::test]
    async fn an_older_same_key_preparation_completion_keeps_the_newer_attempt_visible()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        let config = LintConfig {
            path: project
                .directory
                .path()
                .join("absent-bend-lint")
                .to_string_lossy()
                .into_owned(),
            ..lint_config()?
        };
        let key = PreparationKey {
            root: project.directory.path().to_path_buf(),
            config: config.clone(),
            inputs: [0; 32],
        };
        let project_key = Backend::root_key(project.directory.path());
        backend
            .diagnostics
            .lint
            .projects
            .write()
            .insert(project_key.clone(), ProjectState::default());
        // The older attempt is real and cannot finish while this lease is held.
        let lease = backend
            .diagnostics
            .lint
            .prep_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        let older = {
            let backend = backend.clone();
            let project_key = project_key.clone();
            let key = key.clone();
            tokio::spawn(async move {
                backend
                    .run_preparation(project_key, key, PreparationToken::new(1))
                    .await;
            })
        };
        let newer = "newer analysis identity".to_owned();
        {
            let mut projects = backend.diagnostics.lint.projects.write();
            let state = projects
                .get_mut(&project_key)
                .ok_or_else(|| io::Error::other("project state"))?;
            state.preparation = Some(Preparation {
                token: PreparationToken::new(2),
                key: key.clone(),
                phase: PreparationPhase::Ready(newer.clone()),
                task: None,
            });
        }
        drop(lease);
        older.await?;
        assert_eq!(
            backend.lint_tool_identity(&config, &project.main_uri),
            newer,
            "an older same-key completion never replaces the newer attempt"
        );
        assert_eq!(
            backend.ensure_prepared(&project_key, 0, key).await?,
            newer,
            "the next check reuses the newer preparation"
        );
        Ok(())
    }
}
