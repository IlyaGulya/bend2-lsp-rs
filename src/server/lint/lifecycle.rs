use super::{
    AFFECTED_REASON, CANCELLED_REASON, LintConfig, SUPERSEDED_REASON, WORKSPACE_REASON,
    control::LintControl,
    covered_by,
    project::{PreparationPhase, ProjectKey},
    scheduler::{LintService, join_lint_task},
    snapshot,
};
use crate::{
    server::lsp::Backend,
    workspace::{FileId, WorkspaceDb, normalize_path},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    future::Future,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{MessageType, NumberOrString};
use url::Url;

// The complete editor/workspace surface that can change lint inputs. Handlers
// describe what happened and hand over their mutation future; routing,
// fencing, cleanup, and scheduling stay inside this module.
pub(in crate::server) enum LintChange {
    Open { uri: Url, revision: i32 },
    Change { uri: Url, revision: i32 },
    Save { uri: Url },
    Close { uri: Url },
    Settings(LintSettings),
    Watched(Vec<PathBuf>),
    Workspace,
    Rename(Vec<(PathBuf, PathBuf)>),
    Run { uri: Url },
}

pub(in crate::server) struct LintSettings {
    config: LintConfig,
    warning: Option<String>,
}

impl LintChange {
    // Parse `bend.lint` exactly once from the borrowed settings object: no
    // whole-settings JSON clone and no second parse in the coordinator.
    pub(in crate::server) fn settings(settings: &serde_json::Value) -> Self {
        Self::Settings(LintSettings::parse(settings))
    }
}

impl LintSettings {
    fn parse(settings: &serde_json::Value) -> Self {
        match LintConfig::from_settings(settings) {
            Ok(config) => Self {
                config,
                warning: None,
            },
            Err(message) => Self {
                config: LintConfig::default(),
                warning: Some(format!("bend-lint configuration error: {message}")),
            },
        }
    }
}

// One fenced project of a plan: its retired revision, the scheduler it handed
// back, the progress tokens that revision owned, and every entry whose work has
// to be rechecked instead of being suppressed by the retired revision.
struct ProjectFence {
    key: ProjectKey,
    retired_revision: u64,
    previous: Option<JoinHandle<()>>,
    progress: Vec<NumberOrString>,
    preserved: BTreeSet<Url>,
}

struct LintPlan {
    change: LintChange,
    fences: Vec<ProjectFence>,
    affected: HashSet<Url>,
    // Entries discovered through reverse imports. They are scheduled in this
    // plan's revision even when their project was already fenced.
    entries: BTreeSet<Url>,
}

impl LintService {
    // Fence only the routed projects: bump their revision, retire their
    // scheduler while handing every queued entry back, drop the cached view,
    // and (for settings) abort only preparations whose analyzer key changed.
    // Results are removed after the mutation commits so a rejected event
    // cannot erase a newer coherent result.
    fn fence_projects(
        &self,
        keys: impl IntoIterator<Item = ProjectKey>,
        cancel_preparation: Option<&LintConfig>,
    ) -> Vec<ProjectFence> {
        let mut projects = self.projects.write();
        let results = self.results.read();
        let mut fences = Vec::new();
        for key in keys {
            let state = projects.entry(key.clone()).or_default();
            let retired_revision = state.revision;
            state.revision = state.revision.wrapping_add(1);
            state.view = None;
            state.completed.clear();
            let previous = state.task.take().map(|(_, task)| task);
            if let Some(task) = &previous {
                task.abort();
            }
            let progress = self
                .control
                .take_project_revision_progress(&key, retired_revision);
            let mut preserved = std::mem::take(&mut state.pending);
            if let Some(active) = state.active.take() {
                preserved.insert(active);
            }
            preserved.extend(results.entries_of(&key));
            if let Some(config) = cancel_preparation
                && let Some(preparation) = state.preparation.take()
            {
                if preparation.key.config == *config {
                    state.preparation = Some(preparation);
                } else if let Some(task) = preparation.task {
                    task.abort();
                }
            }
            fences.push(ProjectFence {
                key,
                retired_revision,
                previous,
                progress,
                preserved,
            });
        }
        fences
    }

    fn reset_failed_preparation(&self, key: &ProjectKey) {
        let mut projects = self.projects.write();
        let Some(state) = projects.get_mut(key) else {
            return;
        };
        if !state
            .preparation
            .as_ref()
            .is_some_and(|entry| matches!(&entry.phase, PreparationPhase::Failed(_)))
        {
            return;
        }
        if let Some(preparation) = state.preparation.take()
            && let Some(task) = preparation.task
        {
            task.abort();
        }
        drop(projects);
        self.preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    // Only Settings can change the provider while it is disabled; every other
    // event is a no-op for lint, keeping the default-disabled hot path free.
    fn tracks(&self, change: &LintChange) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && (matches!(change, LintChange::Settings(_)) || self.config.read().enabled)
    }

    fn affected_results<'a>(&self, keys: impl IntoIterator<Item = &'a ProjectKey>) -> HashSet<Url> {
        let results = self.results.read();
        let mut affected = HashSet::new();
        for key in keys {
            affected.extend(results.affected_of(key));
        }
        affected
    }
}

// The coordinator entry for every editor/workspace event that can change lint
// inputs. Fencing happens synchronously before the mutation so no captured view
// can publish across it; the operation is awaited before any lint cleanup, and
// only the routed projects are retired or scheduled afterwards.
impl Backend {
    pub(in crate::server) async fn apply_lint_change<F>(
        &self,
        change: LintChange,
        operation: F,
    ) -> bool
    where
        F: Future<Output = bool>,
    {
        if !self.diagnostics.lint.tracks(&change) {
            return operation.await;
        }
        let plan = self.begin_lint_change(change);
        let applied = operation.await;
        self.finish_lint_change(plan, applied).await;
        applied
    }

    fn begin_lint_change(&self, change: LintChange) -> LintPlan {
        let (fences, affected, entries) = match &change {
            LintChange::Settings(settings) => {
                let config = settings.config.clone();
                self.diagnostics
                    .lint
                    .control
                    .configure(&config, settings.warning.clone());
                *self.diagnostics.lint.config.write() = config.clone();
                let keys = self
                    .diagnostics
                    .lint
                    .projects
                    .read()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let fences = self.diagnostics.lint.fence_projects(keys, Some(&config));
                let affected = self.diagnostics.lint.results.read().affected_keys();
                (fences, affected, BTreeSet::new())
            }
            LintChange::Workspace => {
                self.diagnostics.lint.control.invalidate(WORKSPACE_REASON);
                let keys = self
                    .diagnostics
                    .lint
                    .projects
                    .read()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                let affected = self.diagnostics.lint.results.read().affected_keys();
                (fences, affected, BTreeSet::new())
            }
            LintChange::Open { uri, .. } | LintChange::Change { uri, .. } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), AFFECTED_REASON);
                (fences, HashSet::new(), dependents.into_iter().collect())
            }
            LintChange::Save { uri } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), SUPERSEDED_REASON);
                (fences, HashSet::new(), dependents.into_iter().collect())
            }
            LintChange::Run { uri } => {
                let keys = self.affected_project_keys(std::slice::from_ref(uri));
                for key in &keys {
                    self.diagnostics.lint.reset_failed_preparation(key);
                }
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), SUPERSEDED_REASON);
                (fences, HashSet::new(), BTreeSet::new())
            }
            LintChange::Close { uri } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                self.diagnostics.lint.control.forget(uri);
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, Some(uri), AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
            LintChange::Watched(paths) => {
                let dependents = self.dependent_documents_for_paths(paths);
                let mut keys = self.affected_projects(paths);
                keys.extend(self.affected_project_keys(&dependents));
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, None, AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
            LintChange::Rename(pairs) => {
                let paths: Vec<PathBuf> = pairs
                    .iter()
                    .flat_map(|(old, new)| [old.clone(), new.clone()])
                    .collect();
                let dependents = self.dependent_documents_for_paths(&paths);
                let mut keys = self.affected_projects(&paths);
                keys.extend(self.affected_project_keys(&dependents));
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, None, AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
        };
        LintPlan {
            change,
            fences,
            affected,
            entries,
        }
    }

    fn invalidate_for(&self, fences: &[ProjectFence], extra: Option<&Url>, reason: &str) {
        let mut retired: Vec<(&Url, u64)> = Vec::new();
        for fence in fences {
            retired.extend(
                fence
                    .preserved
                    .iter()
                    .map(|uri| (uri, fence.retired_revision)),
            );
        }
        if let Some(uri) = extra {
            let key = Self::project_key(uri);
            let revision = fences
                .iter()
                .find(|fence| fence.key == key)
                .map_or(u64::MAX, |fence| fence.retired_revision);
            retired.push((uri, revision));
        }
        self.control().invalidate_selected(retired, reason);
    }

    fn control(&self) -> &LintControl {
        &self.diagnostics.lint.control
    }

    async fn finish_lint_change(&self, plan: LintPlan, applied: bool) {
        let mut fences = plan.fences;
        self.retire_fences(&mut fences).await;
        if !applied {
            // A rejected mutation owns nothing: never abort or replace a newer
            // scheduler and never remove results; hand preserved entries back
            // so the newer or next owner drains them.
            self.restore_fences(fences);
            return;
        }
        let mut affected = plan.affected;
        let mut routed = plan.entries;
        // A newly opened document, a created input, or a renamed path is
        // interned by the mutation that just completed, so its reverse imports
        // are only visible here. Fence and re-run those importer projects;
        // otherwise their completed entries would suppress the only check that
        // can read the new input identity.
        let dependents = match &plan.change {
            LintChange::Open { uri, .. } => self.dependent_documents_of(uri),
            LintChange::Watched(paths) => self.dependent_documents_for_paths(paths),
            LintChange::Rename(pairs) => {
                let paths: Vec<PathBuf> = pairs
                    .iter()
                    .flat_map(|(old, new)| [old.clone(), new.clone()])
                    .collect();
                self.dependent_documents_for_paths(&paths)
            }
            LintChange::Settings(_)
            | LintChange::Workspace
            | LintChange::Change { .. }
            | LintChange::Save { .. }
            | LintChange::Run { .. }
            | LintChange::Close { .. } => Vec::new(),
        };
        if !dependents.is_empty() {
            routed.extend(dependents.iter().cloned());
            let planned: BTreeSet<ProjectKey> =
                fences.iter().map(|fence| fence.key.clone()).collect();
            let keys: BTreeSet<ProjectKey> = self
                .affected_project_keys(&dependents)
                .into_iter()
                .filter(|key| !planned.contains(key))
                .collect();
            if !keys.is_empty() {
                let mut extra = self.diagnostics.lint.fence_projects(keys, None);
                self.retire_fences(&mut extra).await;
                self.invalidate_for(&extra, None, AFFECTED_REASON);
                affected.extend(self.remove_retired_lint_results(&extra));
                fences.extend(extra);
            }
        }
        // A watched or renamed file can create or remove a project
        // configuration. That changes which project owns an already-open
        // entry, so the previous ownership's revision is retired and the entry
        // is routed again; otherwise the old revision's completion would
        // suppress the only check that can read the new membership.
        if matches!(&plan.change, LintChange::Watched(_) | LintChange::Rename(_)) {
            let planned: BTreeSet<ProjectKey> =
                fences.iter().map(|fence| fence.key.clone()).collect();
            let stale = self.migrated_ownership(&planned, &mut routed);
            if !stale.is_empty() {
                let mut extra = self.diagnostics.lint.fence_projects(stale, None);
                self.retire_fences(&mut extra).await;
                self.invalidate_for(&extra, None, AFFECTED_REASON);
                affected.extend(self.remove_retired_lint_results(&extra));
                fences.extend(extra);
            }
        }
        match &plan.change {
            LintChange::Settings(_) => {
                let config = self.diagnostics.lint.config.read().clone();
                affected = self
                    .diagnostics
                    .lint
                    .results
                    .write()
                    .remove_stale(|result| result.config != config);
            }
            LintChange::Workspace => {
                affected = self.remove_retired_lint_results(&fences);
            }
            LintChange::Close { .. } | LintChange::Watched(_) | LintChange::Rename(_) => {
                affected.extend(self.remove_retired_lint_results(&fences));
            }
            LintChange::Open { .. }
            | LintChange::Change { .. }
            | LintChange::Save { .. }
            | LintChange::Run { .. } => {}
        }
        if !affected.is_empty() {
            self.publish_coordinated_diagnostics(affected).await;
        }
        if let LintChange::Settings(settings) = &plan.change
            && let Some(warning) = &settings.warning
        {
            self.client.log_message(MessageType::WARNING, warning).await;
            self.client
                .show_message(MessageType::WARNING, warning.clone())
                .await;
        }
        self.schedule_change(&plan.change, &fences, &routed);
        if matches!(
            plan.change,
            LintChange::Close { .. }
                | LintChange::Watched(_)
                | LintChange::Rename(_)
                | LintChange::Workspace
        ) {
            self.prune_unused_projects();
        }
    }

    fn remove_retired_lint_results(&self, fences: &[ProjectFence]) -> HashSet<Url> {
        let mut results = self.diagnostics.lint.results.write();
        let mut removed = HashSet::new();
        for fence in fences {
            removed.extend(results.remove_project_revision(&fence.key, fence.retired_revision));
        }
        removed
    }

    async fn retire_fences(&self, fences: &mut [ProjectFence]) {
        for fence in fences {
            join_lint_task(fence.previous.take()).await;
            let mut progress = std::mem::take(&mut fence.progress);
            progress.extend(
                self.control()
                    .take_project_revision_progress(&fence.key, fence.retired_revision),
            );
            self.end_lint_progress_tokens(progress, CANCELLED_REASON)
                .await;
        }
    }

    fn restore_fences(&self, fences: Vec<ProjectFence>) {
        if !self.diagnostics.lint.config.read().enabled
            || self.diagnostics.lint.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let entries: BTreeSet<Url> = fences
            .into_iter()
            .flat_map(|fence| fence.preserved)
            .collect();
        self.schedule_entries(entries, true);
    }

    fn schedule_change(
        &self,
        change: &LintChange,
        fences: &[ProjectFence],
        routed: &BTreeSet<Url>,
    ) {
        match change {
            LintChange::Settings(_) | LintChange::Workspace => {
                let entries: BTreeSet<Url> = self
                    .workspace_documents()
                    .into_iter()
                    .filter(Self::supported)
                    .map(|document| document.uri.as_ref().clone())
                    .collect();
                self.schedule_entries(entries, true);
            }
            LintChange::Open { uri, revision } | LintChange::Change { uri, revision } => {
                let current = self
                    .document(uri)
                    .is_some_and(|document| document.revision.0 == *revision);
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                if current {
                    entries.insert(uri.clone());
                }
                self.schedule_entries(entries, true);
            }
            LintChange::Save { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                entries.insert(uri.clone());
                self.schedule_entries(entries, false);
            }
            LintChange::Close { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                // The closed spelling may be the only queued/retained entry
                // (for example one half of a conflicted view). Surviving open
                // entries of the affected project must still be rechecked.
                entries.extend(self.fence_open_entries(fences));
                entries.remove(uri);
                self.schedule_entries(entries, true);
            }
            LintChange::Watched(_) | LintChange::Rename(_) => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                self.schedule_entries(entries, true);
            }
            LintChange::Run { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.insert(uri.clone());
                self.schedule_entries(entries, false);
            }
        }
    }

    fn fence_entries(fences: &[ProjectFence]) -> BTreeSet<Url> {
        fences
            .iter()
            .flat_map(|fence| fence.preserved.iter().cloned())
            .collect()
    }

    fn schedule_entries(&self, entries: impl IntoIterator<Item = Url>, debounce: bool) {
        if !self.diagnostics.lint.config.read().enabled
            || self.diagnostics.lint.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let mut groups: BTreeMap<ProjectKey, BTreeSet<Url>> = BTreeMap::new();
        for uri in entries {
            if !self.document(&uri).as_ref().is_some_and(Self::supported) {
                continue;
            }
            groups
                .entry(Self::project_key(&uri))
                .or_default()
                .insert(uri);
        }
        for (key, entries) in groups {
            self.enqueue_project(key, entries, debounce);
        }
    }

    pub(super) fn project_key(uri: &Url) -> ProjectKey {
        uri.to_file_path()
            .ok()
            .and_then(|path| snapshot::paths::project_root(&path))
            .map_or_else(
                || ProjectKey::Document(uri.clone()),
                |root| Self::root_key(&root),
            )
    }

    pub(super) fn root_key(root: &Path) -> ProjectKey {
        let identity = snapshot::paths::input_identity(root);
        ProjectKey::Root(identity.unwrap_or_else(|| normalize_path(root)))
    }

    fn affected_project_keys(&self, uris: &[Url]) -> BTreeSet<ProjectKey> {
        let mut keys = BTreeSet::new();
        let mut paths = Vec::new();
        for uri in uris {
            keys.insert(Self::project_key(uri));
            if let Ok(path) = uri.to_file_path() {
                paths.push(path.clone());
                if let Some(identity) = snapshot::paths::input_identity(&path) {
                    paths.push(identity);
                }
            }
        }
        keys.extend(self.covering_projects(&paths));
        keys
    }

    fn affected_projects(&self, paths: &[PathBuf]) -> BTreeSet<ProjectKey> {
        let mut keys = self.covering_projects(paths);
        for path in paths {
            if let Some(root) = snapshot::paths::project_root(path) {
                keys.insert(Self::root_key(&root));
            }
            if let Some(identity) = snapshot::paths::input_identity(path)
                && let Some(root) = snapshot::paths::project_root(&identity)
            {
                keys.insert(Self::root_key(&root));
            }
        }
        keys
    }

    fn fence_open_entries(&self, fences: &[ProjectFence]) -> BTreeSet<Url> {
        let keys: BTreeSet<ProjectKey> = fences.iter().map(|fence| fence.key.clone()).collect();
        self.workspace_documents()
            .into_iter()
            .filter(Self::supported)
            .filter(|document| keys.contains(&Self::project_key(&document.uri)))
            .map(|document| document.uri.as_ref().clone())
            .collect()
    }

    // A document event also reaches every project that reverse-imports it, so an
    // external or aliased dependency fences and re-runs its importer even when
    // the changed spelling is not part of any retained snapshot.
    fn affected_project_keys_with(&self, uri: &Url, dependents: &[Url]) -> BTreeSet<ProjectKey> {
        let mut uris = Vec::with_capacity(dependents.len() + 1);
        uris.push(uri.clone());
        uris.extend(dependents.iter().cloned());
        self.affected_project_keys(&uris)
    }

    fn dependent_documents_of(&self, uri: &Url) -> Vec<Url> {
        let database = self.workspace.read();
        let id = database.file_id_by_uri(uri).or_else(|| {
            uri.to_file_path()
                .ok()
                .and_then(|path| Self::file_id_by_path(&database, &path))
        });
        let Some(id) = id else {
            return Vec::new();
        };
        database
            .dependents(id)
            .into_iter()
            .map(|document| document.uri.as_ref().clone())
            .collect()
    }

    // Discovery can intern a dependency under a canonical parent directory
    // while an editor or watcher spells the same file through a symlinked
    // parent. Retrying the identity-qualified parent spelling bridges only that
    // difference; the leaf name stays the captured alias key.
    fn file_id_by_path(database: &WorkspaceDb, path: &Path) -> Option<FileId> {
        database.file_id_by_path(path).or_else(|| {
            let name = path.file_name()?;
            let parent = snapshot::paths::input_identity(path.parent()?)?;
            database.file_id_by_path(&parent.join(name))
        })
    }

    // Filesystem events name paths, including spellings that are not project
    // members (an external import alias). Their importers still identify the
    // projects that must re-read the changed identity.
    fn dependent_documents_for_paths(&self, paths: &[PathBuf]) -> Vec<Url> {
        let mut dependents = Vec::new();
        let database = self.workspace.read();
        for path in paths {
            if let Some(id) = Self::file_id_by_path(&database, path) {
                dependents.extend(
                    database
                        .dependents(id)
                        .into_iter()
                        .map(|doc| doc.uri.as_ref().clone()),
                );
            }
        }
        dependents
    }

    // A project's current captured view is coverage even before any result
    // exists: the first check records the saved rule program only there, so an
    // external helper it read is named by no retained snapshot and no workspace
    // import edge. Routing from the view lets that event fence and recheck the
    // entry instead of leaving it skipped.
    fn covering_projects(&self, paths: &[PathBuf]) -> BTreeSet<ProjectKey> {
        // Each event spelling's identity is resolved once for the whole event;
        // the shared pairing predicate then stays pure for every project.
        let spellings: Vec<(PathBuf, Option<PathBuf>)> = paths
            .iter()
            .map(|path| (path.clone(), snapshot::paths::input_identity(path)))
            .collect();
        let mut keys: BTreeSet<ProjectKey> = self
            .diagnostics
            .lint
            .results
            .read()
            .projects_covering(&spellings)
            .into_iter()
            .collect();
        // A project's current captured view is coverage for every spelling it
        // read, including external rule helpers, aliases and inputs that were
        // missing when it was taken.
        let projects = self.diagnostics.lint.projects.read();
        for (key, state) in projects.iter() {
            let view = state.view.as_ref().map(|(_, view)| view.as_ref());
            let covered = view.is_some_and(|view| covered_by(view, &spellings));
            if key.involves(paths) || covered {
                keys.insert(key.clone());
            }
        }
        keys
    }

    // Ownership is discovered from disk, so an open entry can move between a
    // document project and a root project while it stays open. Any key that
    // still owns queued, running, finished, or retained work for an entry whose
    // current key differs is stale: retiring it drops exactly the work that the
    // previous membership produced, and routing the entry again lets the
    // current project check it. A key this plan already fenced needs no second
    // fence; its preserved entries are scheduled under their current key.
    fn migrated_ownership(
        &self,
        planned: &BTreeSet<ProjectKey>,
        routed: &mut BTreeSet<Url>,
    ) -> BTreeSet<ProjectKey> {
        let documents = self.workspace_documents();
        let mut stale = BTreeSet::new();
        {
            let projects = self.diagnostics.lint.projects.read();
            let results = self.diagnostics.lint.results.read();
            for document in documents
                .iter()
                .filter(|document| Self::supported(document))
            {
                let uri = document.uri.as_ref();
                let current = Self::project_key(uri);
                let mut owned: BTreeSet<ProjectKey> = BTreeSet::new();
                for (key, state) in projects.iter() {
                    if state.active.as_ref() == Some(uri)
                        || state.pending.contains(uri)
                        || state.completed.contains(uri)
                    {
                        owned.insert(key.clone());
                    }
                }
                owned.extend(results.project_of(uri).cloned());
                if !owned.iter().any(|key| *key != current) {
                    continue;
                }
                routed.insert(uri.clone());
                for key in owned {
                    if key != current && !planned.contains(&key) {
                        stale.insert(key);
                    }
                }
            }
        }
        stale
    }

    fn prune_unused_projects(&self) {
        let keys: Vec<ProjectKey> = self
            .diagnostics
            .lint
            .projects
            .read()
            .keys()
            .cloned()
            .collect();
        let documents = self.workspace_documents();
        for key in keys {
            let mut projects = self.diagnostics.lint.projects.write();
            let Some(state) = projects.get(&key) else {
                continue;
            };
            let busy = !state.pending.is_empty()
                || state.active.is_some()
                || documents
                    .iter()
                    .any(|document| Self::project_key(&document.uri) == key);
            let retained = !self
                .diagnostics
                .lint
                .results
                .read()
                .entries_of(&key)
                .is_empty();
            if busy || retained {
                continue;
            }
            if let Some(mut state) = projects.remove(&key)
                && let Some(preparation) = state.preparation.take()
                && let Some(task) = preparation.task
            {
                task.abort();
            }
        }
    }
}
