use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::analysis::{self, DocumentSnapshot, Revision};
use url::Url;

mod semantic;
pub use semantic::{
    SemanticBase, SemanticBindingId, SemanticCall, SemanticCompletion, SemanticDeclaration,
    SemanticEntity, SemanticEntityId, SemanticIssue, SemanticOccurrence, SemanticSite,
    SemanticTarget, SemanticView, SemanticViewInput,
};

#[derive(Clone)]
pub struct Document {
    pub uri: Url,
    pub language_id: String,
    pub snapshot: Arc<DocumentSnapshot>,
}

impl Document {
    #[must_use]
    pub fn new(uri: Url, language_id: String, revision: Revision, text: String) -> Self {
        Self {
            uri,
            language_id,
            snapshot: Arc::new(DocumentSnapshot::new(revision, text)),
        }
    }

    #[must_use]
    pub fn with_snapshot(uri: Url, language_id: String, snapshot: Arc<DocumentSnapshot>) -> Self {
        Self {
            uri,
            language_id,
            snapshot,
        }
    }
}

impl Deref for Document {
    type Target = DocumentSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileId(usize);

impl FileId {
    /// Reserved prelude identity when Base is not a stored workspace entry.
    pub(crate) const BASE: Self = Self(usize::MAX);
}

pub(crate) type DocumentImportEdits = (Document, Vec<(analysis::TextRange, String)>);

#[derive(Clone)]
pub(crate) struct PathRename {
    pub(crate) old: PathBuf,
    pub(crate) new: PathBuf,
}

pub(crate) fn renamed_path(path: &Path, renames: &[PathRename]) -> PathBuf {
    renames
        .iter()
        .filter_map(|rename| {
            path.strip_prefix(&rename.old)
                .ok()
                .map(|suffix| (rename, suffix))
        })
        .max_by_key(|(rename, _)| rename.old.components().count())
        .map_or_else(
            || path.to_path_buf(),
            |(rename, suffix)| {
                if suffix.as_os_str().is_empty() {
                    rename.new.clone()
                } else {
                    rename.new.join(suffix)
                }
            },
        )
}

pub(crate) type ImportRenameTarget<'a> = (&'a Path, &'a Path, &'a [PathRename]);

pub(crate) fn import_rename_target<'a>(
    logical: &'a Path,
    physical: &'a Path,
    logical_source: &'a Path,
    physical_source: &'a Path,
    renames: &'a [PathRename],
    projected_renames: &'a [PathRename],
) -> ImportRenameTarget<'a> {
    let matches = |path: &Path, mappings: &[PathRename]| {
        mappings.iter().any(|rename| path.starts_with(&rename.old))
    };
    if matches(logical, renames) {
        (logical, logical_source, renames)
    } else if matches(logical, projected_renames) {
        (logical, physical_source, projected_renames)
    } else if matches(physical, projected_renames) {
        (physical, physical_source, projected_renames)
    } else if matches(logical_source, renames) {
        (logical, logical_source, renames)
    } else if matches(physical_source, projected_renames) {
        (
            if logical_source == physical_source {
                logical
            } else {
                physical
            },
            physical_source,
            projected_renames,
        )
    } else {
        (logical, logical_source, renames)
    }
}

pub(crate) fn relative_import_path(source: &Path, target: &Path) -> Option<String> {
    let mut source = source.parent()?.components();
    let mut target = target.components();
    let mut common = 0;
    let (source_tail, target_head) = loop {
        let pair = (source.next(), target.next());
        if let (Some(left), Some(right)) = pair
            && left == right
        {
            common += 1;
        } else {
            break pair;
        }
    };
    if common == 0 {
        return None;
    }
    let parents = source.count() + usize::from(source_tail.is_some());
    if parents == 0 && target_head.is_none() {
        return None;
    }
    let capacity = parents * 3
        + 2
        + target_head.map_or(0, |part| part.as_os_str().as_encoded_bytes().len())
        + target.as_path().as_os_str().as_encoded_bytes().len();
    let mut path = String::with_capacity(capacity);
    if parents == 0 || (parents == 1 && target_head.is_none()) {
        path.push_str("./");
    }
    for index in 0..parents {
        if index > 0 {
            path.push('/');
        }
        path.push_str("..");
    }
    for component in target_head.into_iter().chain(target) {
        let text = component.as_os_str().to_str()?;
        if text
            .chars()
            .any(|character| character.is_whitespace() || character == '#')
        {
            return None;
        }
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(text);
    }
    Some(path)
}

pub(crate) fn import_rename_edits<'a>(
    source: &Path,
    snapshot: &DocumentSnapshot,
    renames: &[PathRename],
    target_for: impl Fn(analysis::TextRange) -> Option<ImportRenameTarget<'a>>,
) -> Option<Vec<(analysis::TextRange, String)>> {
    let mut edits = Vec::new();
    for import in analysis::imports(snapshot) {
        let imported = import.path_text(&snapshot.text);
        if imported.is_empty() || imported == "Base" || is_hub_import_path(imported) {
            continue;
        }
        let fallback;
        let (target, source, renames) = if let Some(coordinates) = target_for(import.path) {
            coordinates
        } else {
            let candidate = normalize_path(&source.parent()?.join(imported));
            fallback = if candidate.extension().is_none() {
                candidate.with_extension("bend")
            } else {
                candidate
            };
            (fallback.as_path(), source, renames)
        };
        let new_source = renamed_path(source, renames);
        let new_target = renamed_path(target, renames);
        if new_source == source && new_target == target {
            continue;
        }
        let current_target = normalize_path(&new_source.parent()?.join(imported));
        let extensionless = Path::new(imported).extension().is_none()
            && target
                .extension()
                .is_some_and(|extension| extension == "bend");
        if current_target == new_target
            || (extensionless && current_target.with_extension("bend") == new_target)
        {
            continue;
        }
        let mut replacement = if Path::new(imported).is_absolute() {
            let text = new_target.to_str()?;
            if text
                .chars()
                .any(|character| character.is_whitespace() || character == '#')
            {
                return None;
            }
            #[cfg(windows)]
            let replacement = text.replace('\\', "/");
            #[cfg(not(windows))]
            let replacement = text.to_owned();
            replacement
        } else {
            relative_import_path(&new_source, &new_target)?
        };
        if extensionless && replacement.ends_with(".bend") {
            replacement.truncate(replacement.len() - 5);
        }
        if replacement != imported {
            edits.push((import.path, replacement));
        }
    }
    Some(edits)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImportEdge {
    pub(crate) path: analysis::TextRange,
    pub(crate) target: FileId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedImportTarget {
    Native {
        range: analysis::TextRange,
        path: PathBuf,
        physical_path: PathBuf,
        rename_path: Option<PathBuf>,
    },
    Editor {
        range: analysis::TextRange,
        path: PathBuf,
        rename_path: Option<PathBuf>,
    },
}

pub(crate) type PreparedEditorImportPaths = HashMap<PathBuf, Option<PathBuf>>;

#[derive(Clone)]
pub(crate) struct SourceGraphNode {
    pub(crate) id: FileId,
    pub(crate) path: PathBuf,
    pub(crate) snapshot: Option<Arc<DocumentSnapshot>>,
    pub(crate) imports: Box<[ImportEdge]>,
}

#[derive(Clone)]
pub(crate) struct SourceGraph {
    pub(crate) root: FileId,
    pub(crate) nodes: Vec<SourceGraphNode>,
}

impl SourceGraph {
    pub(crate) fn root_node(&self) -> Option<&SourceGraphNode> {
        self.nodes.iter().find(|node| node.id == self.root)
    }
}

struct PhysicalAliases {
    first: FileId,
    others: Vec<FileId>,
}

impl PhysicalAliases {
    fn iter(&self) -> impl Iterator<Item = &FileId> {
        std::iter::once(&self.first).chain(self.others.iter())
    }

    /// Remove an identity, returning whether this physical source has no owners.
    fn remove(&mut self, file: FileId) -> bool {
        if self.first == file {
            let Some(first) = self.others.pop() else {
                return true;
            };
            self.first = first;
        } else {
            self.others.retain(|candidate| *candidate != file);
        }
        false
    }
}
struct ImportWatchDependents {
    first: FileId,
    others: HashSet<FileId>,
}

impl ImportWatchDependents {
    fn iter(&self) -> impl Iterator<Item = &FileId> {
        std::iter::once(&self.first).chain(self.others.iter())
    }

    fn insert(&mut self, file: FileId) {
        if self.first != file {
            self.others.insert(file);
        }
    }

    fn remove(&mut self, file: FileId) -> bool {
        if self.first == file {
            let Some(first) = self.others.iter().next().copied() else {
                return true;
            };
            self.others.remove(&first);
            self.first = first;
        } else {
            self.others.remove(&file);
        }
        false
    }
}

struct FileEntry {
    uri: Url,
    path: Option<PathBuf>,
    physical_path: Option<PathBuf>,
    language_id: String,
    open_snapshot: Option<Arc<DocumentSnapshot>>,
    disk_generation: u64,
    semantic_generation: u64,
    disk_snapshot: Option<Arc<DocumentSnapshot>>,
    retired: bool,
    imports: Box<[ImportEdge]>,
    // One logical resolved path per edge; physical target identities may alias.
    import_paths: Box<[PathBuf]>,
    import_rename_paths: Box<[Option<PathBuf>]>,
    import_watch_paths: Box<[PathBuf]>,
    editor_import_paths: Option<PreparedEditorImportPaths>,
    reverse_imports: Vec<FileId>,
}

impl FileEntry {
    fn snapshot(&self) -> Option<&Arc<DocumentSnapshot>> {
        self.open_snapshot.as_ref().or(self.disk_snapshot.as_ref())
    }

    fn document(&self) -> Option<Document> {
        Some(Document::with_snapshot(
            self.uri.clone(),
            self.language_id.clone(),
            self.snapshot()?.clone(),
        ))
    }
}

struct CompilerDocument {
    _source_lease: Arc<tempfile::TempDir>,
    imports: Vec<PreparedImportTarget>,
}

#[derive(Default)]
pub struct WorkspaceDb {
    entries: Vec<FileEntry>,
    by_uri: HashMap<Url, FileId>,
    by_path: HashMap<PathBuf, FileId>,
    compiler_documents: Option<HashMap<FileId, CompilerDocument>>,
    // User file identities stay interned; graph liveness governs their payloads.
    reachable: HashSet<FileId>,
    discovered: HashSet<FileId>,
    retired: Vec<FileId>,
    semantic_views: HashMap<FileId, Arc<SemanticView>>,
    semantic_dependents: HashMap<FileId, HashSet<FileId>>,
    dirty_semantic_roots: HashSet<FileId>,
    physical_files: HashMap<PathBuf, PhysicalAliases>,
    physical_generations: HashMap<PathBuf, u64>,
    semantic_physical_dependents: HashMap<PathBuf, HashSet<FileId>>,
    semantic_library_path: Option<PathBuf>,
    semantic_base_importers: HashSet<FileId>,
    import_path_dependents: HashMap<PathBuf, ImportWatchDependents>,
}

impl WorkspaceDb {
    pub(crate) fn is_discovered(&self, id: FileId) -> bool {
        self.discovered.contains(&id)
    }
    pub(crate) fn discovered_roots(&self) -> Vec<FileId> {
        self.discovered.iter().copied().collect()
    }
    pub(crate) fn has_discovered_snapshot(&self, path: &Path) -> bool {
        self.by_path
            .get(path)
            .is_some_and(|id| self.is_discovered(*id) && self.entries[id.0].disk_snapshot.is_some())
    }
    pub(crate) fn admit_discovered_path(&mut self, path: PathBuf) -> Option<FileId> {
        let uri = Url::from_file_path(&path).ok()?;
        let id = self.intern(uri, Some(path));
        if self
            .compiler_documents
            .as_ref()
            .is_some_and(|documents| documents.contains_key(&id))
        {
            return None;
        }
        let admitted = self.discovered.insert(id);
        self.extend_reachable(id);
        if admitted {
            self.refresh_semantic_root(id);
        }
        Some(id)
    }
    pub(crate) fn discover_snapshot(
        &mut self,
        path: &Path,
        physical_path: &Path,
        snapshot: Arc<DocumentSnapshot>,
        imports: Vec<PreparedImportTarget>,
    ) -> Option<FileId> {
        let id = self.admit_discovered_path(path.to_path_buf())?;
        self.sync_disk_snapshot_prepared(path, physical_path, Some(snapshot), imports)?;
        Some(id)
    }
    pub(crate) fn retain_discovered_roots(&mut self, roots: &[PathBuf]) {
        let mut removed = Vec::new();
        let entries = &self.entries;
        self.discovered.retain(|id| {
            let retained = entries[id.0]
                .path
                .as_ref()
                .is_some_and(|path| roots.iter().any(|root| path.starts_with(root)));
            if !retained {
                removed.push(*id);
            }
            retained
        });
        if !removed.is_empty() {
            for id in removed {
                self.remove_semantic_view(id);
            }
            self.recompute_reachable();
            self.finish_load_reachable();
        }
    }
    /// Register compiler-owned source for navigation, retaining its backing file
    /// for the lifetime of the workspace index, not just the active compiler cache.
    pub(crate) fn register_compiler_document(
        &mut self,
        uri: Url,
        path: PathBuf,
        physical_path: PathBuf,
        snapshot: Arc<DocumentSnapshot>,
        imports: Vec<PreparedImportTarget>,
        directory: Arc<tempfile::TempDir>,
    ) {
        let id = self.intern(uri, Some(path));
        self.release_retired_payload(id);
        self.update_physical_path(id, Some(physical_path));
        self.invalidate_semantic_file(id);
        self.entries[id.0].disk_snapshot = Some(snapshot);
        self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
        self.refresh_semantic_root(id);
        let editor_paths = imports
            .iter()
            .filter_map(|import| match import {
                PreparedImportTarget::Editor {
                    path, rename_path, ..
                } => Some((path.clone(), rename_path.clone())),
                PreparedImportTarget::Native { .. } => None,
            })
            .collect();
        self.entries[id.0].editor_import_paths = Some(editor_paths);
        self.refresh_import_watch_paths(id);
        self.compiler_documents
            .get_or_insert_with(HashMap::new)
            .insert(
                id,
                CompilerDocument {
                    _source_lease: directory,
                    imports,
                },
            );
    }

    #[must_use]
    pub fn open_document(&self, uri: &Url) -> Option<Document> {
        let id = self.by_uri.get(uri)?;
        let entry = &self.entries[id.0];
        if entry.uri != *uri {
            return None;
        }
        entry.open_snapshot.as_ref()?;
        entry.document()
    }

    pub(crate) fn is_document_open(&self, uri: &Url) -> bool {
        self.by_uri
            .get(uri)
            .is_some_and(|id| self.entries[id.0].open_snapshot.is_some())
    }

    pub(crate) fn is_file_open(&self, file: FileId) -> bool {
        self.entries
            .get(file.0)
            .is_some_and(|entry| entry.open_snapshot.is_some())
    }

    pub(crate) fn is_compiler_document(&self, uri: &Url) -> bool {
        self.file_id_by_uri(uri).is_some_and(|id| {
            self.compiler_documents
                .as_ref()
                .is_some_and(|documents| documents.contains_key(&id))
        })
    }

    pub(crate) fn imports_prelude(&self, uri: &Url) -> bool {
        self.by_uri
            .get(uri)
            .and_then(|id| self.entries[id.0].snapshot())
            .is_some_and(|snapshot| {
                analysis::imports(snapshot).iter().any(|import| {
                    import.valid_syntax
                        && import.alias.is_none()
                        && import.path_text(&snapshot.text) == "Base"
                })
            })
    }

    pub(crate) fn workspace_imports_prelude(&self) -> bool {
        self.semantic_base_importers
            .iter()
            .any(|file| self.is_needed(*file))
    }

    #[must_use]
    pub fn cached_document(&self, uri: &Url) -> Option<Document> {
        self.entries.get(self.by_uri.get(uri)?.0)?.document()
    }

    pub(crate) fn source_snapshot(&self, id: FileId) -> Option<&Arc<DocumentSnapshot>> {
        self.entries.get(id.0)?.snapshot()
    }

    pub(crate) fn open_snapshot(&self, id: FileId) -> Option<&Arc<DocumentSnapshot>> {
        self.entries.get(id.0)?.open_snapshot.as_ref()
    }

    #[must_use]
    pub fn import_target(
        &self,
        source: &Url,
        import_path: analysis::TextRange,
    ) -> Option<Document> {
        let source_id = *self.by_uri.get(source)?;
        let target = self.entries[source_id.0]
            .imports
            .iter()
            .find(|edge| edge.path == import_path)?
            .target;
        self.entries[target.0].document()
    }

    pub(crate) fn import_logical_uri(
        &self,
        source: &Url,
        import_path: analysis::TextRange,
    ) -> Option<Url> {
        let entry = self.entries.get(self.by_uri.get(source)?.0)?;
        let (index, edge) = entry
            .imports
            .iter()
            .enumerate()
            .find(|(_, edge)| edge.path == import_path)?;
        self.entries.get(edge.target.0)?.snapshot()?;
        Url::from_file_path(entry.import_paths.get(index)?).ok()
    }

    pub(crate) fn editor_import_paths_ready(&self, source: &Url) -> bool {
        let Some(file) = self.by_uri.get(source) else {
            return true;
        };
        let entry = &self.entries[file.0];
        entry.path.is_none()
            || entry.snapshot().is_none()
            || (!self.is_needed(*file) && !self.is_compiler_document(source))
            || entry.editor_import_paths.is_some()
    }

    pub(crate) fn set_editor_import_paths(
        &mut self,
        file: FileId,
        expected: &Arc<DocumentSnapshot>,
        paths: PreparedEditorImportPaths,
    ) -> bool {
        let Some(entry) = self.entries.get(file.0) else {
            return false;
        };
        if entry.path.is_none()
            || (!self.is_needed(file) && !self.is_compiler_document(&entry.uri))
            || !entry
                .snapshot()
                .is_some_and(|snapshot| Arc::ptr_eq(snapshot, expected))
        {
            return false;
        }
        self.entries[file.0].editor_import_paths = Some(paths);
        self.refresh_import_watch_paths(file);
        true
    }

    pub(crate) fn invalidate_editor_import_path(&mut self, path: &Path) {
        let path = normalize_path(path);
        if let Some(sources) = self.import_path_dependents.get(&path) {
            for source in sources.iter() {
                self.entries[source.0].editor_import_paths = None;
            }
        }
    }

    /// Best-effort editor navigation only; never contributes semantic edges.
    /// Cold preparation and discovery supply indexed logical target paths.
    pub(crate) fn editor_import_uri(
        &self,
        source: &Url,
        import_path: analysis::TextRange,
    ) -> Option<Url> {
        Url::from_file_path(self.editor_import_path(source, import_path)?).ok()
    }

    pub(crate) fn editor_import_path(
        &self,
        source: &Url,
        import_path: analysis::TextRange,
    ) -> Option<&Path> {
        let entry = self.entries.get(self.by_uri.get(source)?.0)?;
        let snapshot = entry.snapshot()?;
        let import = snapshot
            .syntax
            .imports()
            .iter()
            .find(|import| import.path == import_path)?;
        let written = import.path_text(&snapshot.text);
        self.editor_import_target_path(entry.path.as_deref()?, written)
    }

    pub(crate) fn editor_import_target_path(&self, source: &Path, written: &str) -> Option<&Path> {
        if written.is_empty() || written == "Base" || is_hub_import_path(written) {
            return None;
        }
        let prepared = self
            .by_path
            .get(source)
            .and_then(|file| self.entries[file.0].editor_import_paths.as_ref());
        let candidate = normalize_path(&source.parent()?.join(written));
        if let Some((path, _)) = prepared.and_then(|paths| paths.get_key_value(&candidate)) {
            return Some(path.as_path());
        }
        if let Some((path, id)) = self.by_path.get_key_value(&candidate)
            && self.is_needed(*id)
            && self.entries[id.0].snapshot().is_some()
        {
            return Some(path.as_path());
        }
        if candidate.extension().is_none() {
            let candidate = candidate.with_extension("bend");
            if let Some((path, _)) = prepared.and_then(|paths| paths.get_key_value(&candidate)) {
                return Some(path.as_path());
            }
            if let Some((path, id)) = self.by_path.get_key_value(&candidate)
                && self.is_needed(*id)
                && self.entries[id.0].snapshot().is_some()
            {
                return Some(path.as_path());
            }
        }
        None
    }

    pub(crate) fn editor_import_rename_target<'a>(
        &'a self,
        source: &'a Path,
        written: &str,
        renames: &'a [PathRename],
        projected_renames: &'a [PathRename],
    ) -> Option<ImportRenameTarget<'a>> {
        let file = self.by_path.get(source)?;
        let logical = self.editor_import_target_path(source, written)?;
        let physical = self.entries[file.0]
            .editor_import_paths
            .as_ref()
            .and_then(|paths| paths.get(logical))
            .and_then(Option::as_deref)
            .unwrap_or(logical);
        Some(import_rename_target(
            logical,
            physical,
            source,
            self.entries[file.0]
                .physical_path
                .as_deref()
                .unwrap_or(source),
            renames,
            projected_renames,
        ))
    }

    pub fn open_documents(&self) -> Vec<Document> {
        self.entries
            .iter()
            .filter(|entry| entry.open_snapshot.is_some())
            .filter_map(FileEntry::document)
            .collect()
    }

    pub(crate) fn loaded_documents(&self) -> Vec<Document> {
        self.entries
            .iter()
            .filter_map(FileEntry::document)
            .filter(|document| !self.is_compiler_document(&document.uri))
            .collect()
    }

    pub(crate) fn available_import_targets(&self, source: &Url) -> Vec<String> {
        let mut targets = vec!["Base".into()];
        let Some(source_path) = self
            .file_id_by_uri(source)
            .and_then(|id| self.entries[id.0].path.as_deref())
        else {
            return targets;
        };
        self.visit_indexed_entries(|id, entry| {
            let Some(snapshot) = entry.snapshot() else {
                return;
            };
            for import in analysis::imports(snapshot) {
                let path = import.path_text(&snapshot.text);
                if is_hub_import_path(path)
                    && entry.imports.iter().any(|edge| {
                        edge.path == import.path && self.entries[edge.target.0].snapshot().is_some()
                    })
                {
                    targets.push(path.into());
                }
            }
            if entry.uri == *source
                || self
                    .compiler_documents
                    .as_ref()
                    .is_some_and(|documents| documents.contains_key(&id))
            {
                return;
            }
            if let Some(path) = entry.path.as_deref()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "bend")
                && let Some(mut relative) = self
                    .known_hub_import(id)
                    .or_else(|| relative_import_path(source_path, path))
            {
                if relative.starts_with("./") && !relative[2..].contains('/') {
                    relative.drain(..2);
                }
                targets.push(relative);
            }
        });
        targets.sort_unstable();
        targets.dedup();
        targets
    }

    fn known_hub_import(&self, target: FileId) -> Option<String> {
        for source in &self.entries[target.0].reverse_imports {
            let entry = &self.entries[source.0];
            let snapshot = entry.snapshot()?;
            for edge in &entry.imports {
                if edge.target != target {
                    continue;
                }
                let path = snapshot.text.get(edge.path.start..edge.path.end)?;
                if is_hub_import_path(path) {
                    return Some(path.into());
                }
            }
        }
        None
    }

    pub(crate) fn import_candidates(&self, source: &Url, name: &str) -> Vec<(Document, String)> {
        self.import_candidates_matching(source, |snapshot| {
            analysis::declaration_range(snapshot, name).map(|_| ())
        })
        .into_iter()
        .map(|(document, path, ())| (document, path))
        .collect()
    }

    /// Find matching symbols or constructors in already indexed importable documents.
    /// This query does not discover files, download packages, or rebuild snapshots.
    #[must_use]
    pub fn import_completion_candidates(
        &self,
        source: &Url,
        prefix: &str,
        in_case_pattern: bool,
    ) -> Vec<(Document, String, Vec<analysis::Completion>)> {
        self.import_candidates_matching(source, |snapshot| {
            let items = if in_case_pattern {
                analysis::constructor_completion_items(snapshot, None, prefix)
            } else {
                analysis::module_completion_items(snapshot, prefix)
            };
            (!items.is_empty()).then_some(items)
        })
    }

    fn import_candidates_matching<T>(
        &self,
        source: &Url,
        mut matching: impl FnMut(&DocumentSnapshot) -> Option<T>,
    ) -> Vec<(Document, String, T)> {
        let Some(source_id) = self.file_id_by_uri(source) else {
            return Vec::new();
        };
        let Some(source_path) = self.entries[source_id.0].path.as_deref() else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        self.visit_indexed_entries(|id, entry| {
            let Some(snapshot) = entry.snapshot() else {
                return;
            };
            if id == source_id
                || self
                    .compiler_documents
                    .as_ref()
                    .is_some_and(|documents| documents.contains_key(&id))
            {
                return;
            }
            let Some(items) = matching(snapshot) else {
                return;
            };
            let Some(path) = entry.path.as_deref().filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "bend")
            }) else {
                return;
            };
            let Some(relative) = self
                .known_hub_import(id)
                .or_else(|| relative_import_path(source_path, path))
            else {
                return;
            };
            let document = Document::with_snapshot(
                entry.uri.clone(),
                entry.language_id.clone(),
                snapshot.clone(),
            );
            candidates.push((document, relative, items));
        });
        candidates.sort_unstable_by(|left, right| left.1.cmp(&right.1));
        candidates
    }

    pub(crate) fn rename_import_edits(
        &self,
        renames: &[PathRename],
        projected_renames: &[PathRename],
    ) -> Option<Vec<DocumentImportEdits>> {
        let mut changes = Vec::new();
        for document in self.loaded_documents() {
            let id = self.file_id_by_uri(&document.uri)?;
            if self
                .compiler_documents
                .as_ref()
                .is_some_and(|documents| documents.contains_key(&id))
            {
                continue;
            }
            let Some(source) = self.entries[id.0].path.as_deref() else {
                continue;
            };
            let edits = import_rename_edits(source, &document, renames, |range| {
                self.entries[id.0]
                    .imports
                    .iter()
                    .enumerate()
                    .find(|(_, edge)| edge.path == range)
                    .and_then(|(index, edge)| {
                        let logical = self.entries[id.0].import_paths.get(index)?.as_path();
                        let physical = self.entries[id.0]
                            .import_rename_paths
                            .get(index)
                            .and_then(Option::as_deref)
                            .or(self.entries[edge.target.0].physical_path.as_deref())
                            .unwrap_or(logical);
                        Some(import_rename_target(
                            logical,
                            physical,
                            source,
                            self.entries[id.0]
                                .physical_path
                                .as_deref()
                                .unwrap_or(source),
                            renames,
                            projected_renames,
                        ))
                    })
                    .or_else(|| {
                        self.editor_import_rename_target(
                            source,
                            document.text.get(range.start..range.end)?,
                            renames,
                            projected_renames,
                        )
                    })
            })?;
            if !edits.is_empty() {
                changes.push((document, edits));
            }
        }
        changes.sort_unstable_by(|(left, _), (right, _)| left.uri.cmp(&right.uri));
        Some(changes)
    }

    pub(crate) fn rename_paths_compatible(&self, renames: &[PathRename]) -> bool {
        if self.compiler_documents.as_ref().is_some_and(|documents| {
            documents.keys().any(|id| {
                self.entries[id.0].path.as_deref().is_some_and(|path| {
                    renamed_path(path, renames) != path
                        || renames.iter().any(|rename| path.starts_with(&rename.new))
                })
            })
        }) {
            return false;
        }
        let mut destinations: HashMap<PathBuf, FileId> = HashMap::new();
        let moving: HashSet<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if self.is_compiler_document(&entry.uri) {
                    return None;
                }
                let path = entry.path.as_deref()?;
                (renamed_path(path, renames) != path).then_some(FileId(index))
            })
            .collect();
        for id in &moving {
            let entry = &self.entries[id.0];
            let Some(path) = entry.path.as_deref() else {
                continue;
            };
            let destination = renamed_path(path, renames);
            if let Some(previous) = destinations.get(&destination).copied() {
                let previous_entry: &FileEntry = &self.entries[previous.0];
                if entry
                    .open_snapshot
                    .as_ref()
                    .zip(previous_entry.open_snapshot.as_ref())
                    .is_some_and(|(source, target)| {
                        source.revision != target.revision || source.text != target.text
                    })
                {
                    return false;
                }
                if previous_entry.open_snapshot.is_none() && entry.open_snapshot.is_some() {
                    destinations.insert(destination.clone(), *id);
                }
            } else {
                destinations.insert(destination.clone(), *id);
            }
            if let Some(other) = self.by_path.get(&destination)
                && !moving.contains(other)
                && let (Some(source), Some(target)) = (
                    entry.open_snapshot.as_ref(),
                    self.entries[other.0].open_snapshot.as_ref(),
                )
                && (source.revision != target.revision || source.text != target.text)
            {
                return false;
            }
        }
        true
    }

    /// Relocate identities only; the client owns filesystem and buffer edits.
    /// Return merged identities so revision synchronization follows an already
    /// opened destination buffer rather than the retired source identity.
    pub(crate) fn rename_paths(
        &mut self,
        renames: &[PathRename],
        physical_paths: &HashMap<PathBuf, PathBuf>,
    ) -> Vec<(FileId, FileId)> {
        let moves: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let id = FileId(index);
                if self
                    .compiler_documents
                    .as_ref()
                    .is_some_and(|documents| documents.contains_key(&id))
                {
                    return None;
                }
                let old = entry.path.as_ref()?;
                let new = renamed_path(old, renames);
                (new != *old).then_some((id, old.clone(), new))
            })
            .collect();
        for (id, old, _) in &moves {
            self.release_retired_payload(*id);
            self.invalidate_semantic_file(*id);
            self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
            if self.by_path.get(old) == Some(id) {
                self.by_path.remove(old);
            }
            self.by_uri.remove(&self.entries[id.0].uri);
        }
        let mut merged = Vec::new();
        for (id, _, path) in moves {
            let Ok(uri) = Url::from_file_path(&path) else {
                continue;
            };
            let destination = self
                .by_path
                .get(&path)
                .copied()
                .filter(|other| *other != id);
            let canonical = destination
                .filter(|other| self.entries[other.0].open_snapshot.is_some())
                .unwrap_or(id);
            if let Some(destination) = destination {
                self.release_retired_payload(destination);
                self.invalidate_semantic_file(destination);
                self.entries[destination.0].disk_generation =
                    self.entries[destination.0].disk_generation.wrapping_add(1);
                let retired = if canonical == id { destination } else { id };
                self.merge_renamed_entry(canonical, retired);
                merged.push((retired, canonical));
            }
            self.entries[canonical.0].uri = uri.clone();
            self.entries[canonical.0].path = Some(path.clone());
            self.update_physical_path(canonical, physical_paths.get(&path).cloned());
            self.refresh_import_watch_paths(canonical);
            self.refresh_semantic_root(canonical);
            self.by_path.insert(path, canonical);
            self.by_uri.insert(uri, canonical);
        }
        for entry in &mut self.entries {
            for path in &mut entry.import_paths {
                if renames.iter().any(|rename| path.starts_with(&rename.old)) {
                    *path = renamed_path(path, renames);
                }
            }
            for path in entry.import_rename_paths.iter_mut().flatten() {
                if renames.iter().any(|rename| path.starts_with(&rename.old)) {
                    *path = renamed_path(path, renames);
                }
            }
            for edge in &mut entry.imports {
                if let Some((_, canonical)) =
                    merged.iter().find(|(retired, _)| *retired == edge.target)
                {
                    edge.target = *canonical;
                }
            }
            entry.reverse_imports.clear();
        }
        for index in 0..self.entries.len() {
            for edge_index in 0..self.entries[index].imports.len() {
                let edge = self.entries[index].imports[edge_index];
                let reverse = &mut self.entries[edge.target.0].reverse_imports;
                let source = FileId(index);
                if !reverse.contains(&source) {
                    reverse.push(source);
                }
            }
        }
        self.recompute_reachable();
        merged
    }

    fn merge_renamed_entry(&mut self, canonical: FileId, retired: FileId) {
        if self.discovered.remove(&retired) {
            self.discovered.insert(canonical);
        }
        // Import edges belong to the effective snapshot, including an open
        // destination loaded by the client's preceding edit.
        let transfer_imports = self.entries[canonical.0].snapshot().is_none()
            || (self.entries[canonical.0].open_snapshot.is_none()
                && self.entries[retired.0].open_snapshot.is_some());
        if self.entries[canonical.0].disk_snapshot.is_none() {
            self.entries[canonical.0].disk_snapshot = self.entries[retired.0].disk_snapshot.take();
        }
        if self.entries[canonical.0].open_snapshot.is_none() {
            self.entries[canonical.0].open_snapshot = self.entries[retired.0].open_snapshot.take();
        }
        if transfer_imports {
            self.entries[canonical.0].imports =
                std::mem::take(&mut self.entries[retired.0].imports);
            self.entries[canonical.0].import_paths =
                std::mem::take(&mut self.entries[retired.0].import_paths);
            self.entries[canonical.0].import_rename_paths =
                std::mem::take(&mut self.entries[retired.0].import_rename_paths);
        }
        self.entries[retired.0].open_snapshot = None;
        self.entries[retired.0].disk_snapshot = None;
        self.entries[retired.0].path = None;
        self.entries[retired.0].imports = Box::default();
        self.entries[retired.0].import_paths = Box::default();
        self.entries[retired.0].import_rename_paths = Box::default();
        self.clear_import_watch_paths(retired);
        self.update_physical_path(retired, None);
        self.dirty_semantic_roots.remove(&retired);
    }

    pub(crate) fn source_graph(&self, root: FileId) -> Option<SourceGraph> {
        // Compiler-owned source is an indexed navigation target, not a
        // standalone user program. Preserve this policy across editor events.
        if self
            .compiler_documents
            .as_ref()
            .is_some_and(|documents| documents.contains_key(&root))
        {
            return None;
        }
        self.entries.get(root.0)?.snapshot()?;
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        let mut nodes = Vec::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let entry = self.entries.get(id.0)?;
            let snapshot = entry.snapshot().cloned();
            let imports = entry.imports.clone();
            if let Some(snapshot) = &snapshot {
                for edge in &imports {
                    let imported = snapshot.text.get(edge.path.start..edge.path.end)?;
                    if imported != "Base" && !is_hub_import_path(imported) {
                        pending.push(edge.target);
                    }
                }
            }
            nodes.push(SourceGraphNode {
                id,
                path: entry.path.clone()?,
                snapshot,
                imports,
            });
        }
        nodes.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        Some(SourceGraph { root, nodes })
    }

    pub fn set_open_document(&mut self, document: Document, path: Option<PathBuf>) -> FileId {
        let physical_path = path.as_deref().map(canonical_source_path);
        let imports = path
            .as_deref()
            .zip(physical_path.as_deref())
            .map_or_else(Vec::new, |(path, physical)| {
                resolve_import_targets(path, physical, &document.snapshot)
            });
        self.set_open_document_prepared(document, path, physical_path, imports)
    }

    pub(crate) fn set_open_document_prepared(
        &mut self,
        document: Document,
        path: Option<PathBuf>,
        physical_path: Option<PathBuf>,
        imports: Vec<PreparedImportTarget>,
    ) -> FileId {
        let id = self.intern(document.uri.clone(), path);
        self.update_physical_path(id, physical_path);
        self.release_retired_payload(id);
        self.invalidate_semantic_file(id);
        let entry = &mut self.entries[id.0];
        entry.language_id = document.language_id;
        entry.open_snapshot = Some(document.snapshot);
        self.dirty_semantic_roots.insert(id);
        self.refresh_imports_with_targets(id, imports);
        self.extend_reachable(id);
        id
    }

    pub fn update_open_snapshot(
        &mut self,
        uri: &Url,
        snapshot: Arc<DocumentSnapshot>,
    ) -> Option<(FileId, bool)> {
        let path = self.entries.get(self.by_uri.get(uri)?.0)?.path.as_deref();
        let physical_path = path.map(canonical_source_path);
        let imports = path
            .zip(physical_path.as_deref())
            .map_or_else(Vec::new, |(path, physical)| {
                resolve_import_targets(path, physical, &snapshot)
            });
        self.update_open_snapshot_prepared(uri, snapshot, physical_path, imports)
    }

    pub(crate) fn update_open_snapshot_prepared(
        &mut self,
        uri: &Url,
        snapshot: Arc<DocumentSnapshot>,
        physical_path: Option<PathBuf>,
        imports: Vec<PreparedImportTarget>,
    ) -> Option<(FileId, bool)> {
        let id = *self.by_uri.get(uri)?;
        if self.entries[id.0].uri != *uri {
            return None;
        }
        self.update_physical_path(id, physical_path);
        self.release_retired_payload(id);
        self.invalidate_semantic_file(id);
        let entry = self.entries.get_mut(id.0)?;
        let was_open = entry.open_snapshot.is_some();
        entry.open_snapshot = Some(snapshot);
        self.dirty_semantic_roots.insert(id);
        let imports_changed = self.refresh_imports_with_targets(id, imports);
        if !was_open {
            self.extend_reachable(id);
        }
        Some((id, imports_changed))
    }

    pub fn close_document(&mut self, uri: &Url) -> Option<FileId> {
        self.close_document_prepared(uri)
    }

    pub(crate) fn close_document_prepared(&mut self, uri: &Url) -> Option<FileId> {
        let id = *self.by_uri.get(uri)?;
        let compiler_owned = self.is_compiler_document(uri);
        let entry = self.entries.get_mut(id.0)?;
        if entry.uri != *uri {
            return None;
        }
        entry.open_snapshot.take()?;
        // Generated navigation source has independent workspace ownership.
        // User disk snapshots and overlay imports must be loaded afresh.
        if !compiler_owned {
            entry.disk_snapshot = None;
        }
        entry.disk_generation = entry.disk_generation.wrapping_add(1);
        entry.language_id = "bend".into();
        self.invalidate_semantic_file(id);
        self.dirty_semantic_roots.remove(&id);
        // Restore cold compiler disk imports rather than the closing overlay's
        // edges. Their physical identities were resolved before publication.
        let imports = self
            .compiler_documents
            .as_ref()
            .and_then(|documents| documents.get(&id))
            .map_or_else(Vec::new, |document| document.imports.clone());
        let imports_changed = self.refresh_imports_with_targets(id, imports);
        if compiler_owned || !imports_changed {
            self.recompute_reachable();
        }
        Some(id)
    }

    pub fn sync_disk_path(&mut self, path: &Path, text: Option<String>) -> Option<(FileId, bool)> {
        let path = normalize_path(path);
        let uri = Url::from_file_path(&path).ok()?;
        let id = self.intern(uri, Some(path.clone()));
        if !self.is_needed(id) {
            return Some((id, false));
        }
        let snapshot =
            text.map(|text| Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text)));
        let physical_path = canonical_source_path(&path);
        let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            resolve_import_targets(&path, &physical_path, snapshot)
        });
        self.sync_disk_snapshot_prepared(&path, &physical_path, snapshot, imports)
    }

    pub(crate) fn sync_disk_snapshot_prepared(
        &mut self,
        path: &Path,
        physical_path: &Path,
        snapshot: Option<Arc<DocumentSnapshot>>,
        imports: Vec<PreparedImportTarget>,
    ) -> Option<(FileId, bool)> {
        let path = normalize_path(path);
        let id = *self.by_path.get(&path)?;
        if !self.is_needed(id) {
            return None;
        }
        if self.entries[id.0].physical_path.as_deref() != Some(physical_path) {
            self.update_physical_path(id, Some(physical_path.to_path_buf()));
        }
        self.invalidate_semantic_file(id);
        self.entries[id.0].disk_snapshot = snapshot;
        self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
        self.refresh_semantic_root(id);
        // The prepared imports belong to the disk snapshot, not an open overlay
        // that may have changed while the disk snapshot was being constructed.
        let imports_changed = self.entries[id.0].open_snapshot.is_none()
            && self.refresh_imports_with_targets(id, imports);
        Some((id, imports_changed))
    }

    pub(crate) fn document_path(&self, uri: &Url) -> Option<PathBuf> {
        self.entries.get(self.by_uri.get(uri)?.0)?.path.clone()
    }

    pub(crate) fn indexed_paths(&self) -> impl Iterator<Item = (&Path, bool)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry.path.as_deref().map(|path| {
                    (
                        path,
                        self.compiler_documents
                            .as_ref()
                            .is_some_and(|documents| documents.contains_key(&FileId(index))),
                    )
                })
            })
    }

    pub(crate) fn missing_disk_paths(&self, roots: &[FileId]) -> Vec<PathBuf> {
        let mut pending: Vec<_> = roots
            .iter()
            .copied()
            .filter(|id| self.is_needed(*id))
            .collect();
        let mut visited = HashSet::new();
        let mut paths = Vec::new();
        while let Some(id) = pending.pop() {
            if !self.is_needed(id) || !visited.insert(id) {
                continue;
            }
            let Some(entry) = self.entries.get(id.0) else {
                continue;
            };
            if entry.snapshot().is_none()
                && let Some(path) = &entry.path
            {
                paths.push(path.clone());
            }
            pending.extend(entry.imports.iter().map(|edge| edge.target));
        }
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    pub fn load_reachable(&mut self, roots: &[FileId]) {
        let mut pending: Vec<_> = roots
            .iter()
            .copied()
            .filter(|id| self.is_needed(*id))
            .collect();
        let mut visited = HashSet::new();
        while let Some(id) = pending.pop() {
            if !self.is_needed(id) || !visited.insert(id) {
                continue;
            }
            if self.entries[id.0].snapshot().is_none()
                && let Some(path) = self.entries[id.0].path.clone()
            {
                let text = std::fs::read_to_string(&path).ok();
                self.sync_disk_path(&path, text);
            }
            pending.extend(self.entries[id.0].imports.iter().map(|edge| edge.target));
        }
        self.finish_load_reachable();
    }

    pub(crate) fn finish_load_reachable(&mut self) {
        // In-flight readers retain their Arcs; release only database ownership
        // once the lifecycle transaction has finished.
        while let Some(id) = self.retired.pop() {
            if self.is_needed(id) {
                continue;
            }
            self.release_retired_payload(id);
        }
    }

    fn release_retired_payload(&mut self, id: FileId) {
        let entry = &mut self.entries[id.0];
        if !std::mem::take(&mut entry.retired) {
            return;
        }
        let compiler_owned = self
            .compiler_documents
            .as_ref()
            .is_some_and(|documents| documents.contains_key(&id));
        // Compiler navigation payloads and their canonical disk import edges
        // have independent ownership and must remain traversable on reactivation.
        if compiler_owned {
            return;
        }
        if entry.disk_snapshot.take().is_some() {
            entry.disk_generation = entry.disk_generation.wrapping_add(1);
        }
        let imports = std::mem::take(&mut entry.imports);
        entry.import_paths = Box::default();
        entry.import_rename_paths = Box::default();
        self.clear_import_watch_paths(id);
        // Incoming links may belong to a new revision; remove only this
        // retired source's outgoing reverse links.
        for edge in imports {
            self.entries[edge.target.0]
                .reverse_imports
                .retain(|dependent| *dependent != id);
        }
    }

    pub(crate) fn dependencies(&self, root: FileId) -> HashSet<FileId> {
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            if let Some(entry) = self.entries.get(id.0) {
                pending.extend(entry.imports.iter().map(|edge| edge.target));
            }
        }
        visited
    }

    fn visit_indexed_entries(&self, mut visit: impl FnMut(FileId, &FileEntry)) {
        for &id in &self.reachable {
            visit(id, &self.entries[id.0]);
        }
    }

    #[must_use]
    pub fn indexed_documents(&self) -> Vec<Document> {
        let mut documents = Vec::with_capacity(self.reachable.len());
        self.visit_indexed_entries(|_, entry| {
            if let Some(document) = entry.document() {
                documents.push(document);
            }
        });
        documents
    }

    pub(crate) fn is_needed(&self, id: FileId) -> bool {
        self.reachable.contains(&id)
    }

    pub(crate) fn disk_generation(&self, id: FileId) -> Option<u64> {
        Some(self.entries.get(id.0)?.disk_generation)
    }

    fn extend_reachable(&mut self, root: FileId) {
        if self.reachable.contains(&root) {
            return;
        }
        self.release_retired_payload(root);
        self.reachable.insert(root);
        self.refresh_semantic_root(root);
        let mut pending: Vec<_> = self.entries[root.0]
            .imports
            .iter()
            .map(|edge| edge.target)
            .collect();
        while let Some(id) = pending.pop() {
            if !self.reachable.insert(id) {
                continue;
            }
            self.release_retired_payload(id);
            self.refresh_semantic_root(id);
            pending.extend(self.entries[id.0].imports.iter().map(|edge| edge.target));
        }
    }

    fn recompute_reachable(&mut self) {
        let mut pending: Vec<FileId> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.open_snapshot.is_some())
            .map(|(index, _)| FileId(index))
            .collect();
        pending.extend(self.discovered.iter().copied());
        let mut reachable = HashSet::new();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            self.release_retired_payload(id);
            pending.extend(self.entries[id.0].imports.iter().map(|edge| edge.target));
        }
        let previous = std::mem::replace(&mut self.reachable, reachable);
        let entries = &self.entries;
        self.dirty_semantic_roots.extend(
            self.reachable
                .difference(&previous)
                .copied()
                .filter(|id| entries[id.0].snapshot().is_some()),
        );
        for id in previous {
            if self.reachable.contains(&id) {
                continue;
            }
            self.invalidate_semantic_file(id);
            self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
            self.entries[id.0].retired = true;
            self.retired.push(id);
        }
    }

    #[must_use]
    pub fn dependents(&self, changed: FileId) -> Vec<Document> {
        if !self.reachable.contains(&changed) {
            return Vec::new();
        }
        let dependent_bound = self.reachable.len() - 1;
        let capacity = self.entries[changed.0]
            .reverse_imports
            .len()
            .min(dependent_bound);
        if capacity == 0 {
            return Vec::new();
        }
        let mut pending = Vec::new();
        let mut documents = Vec::with_capacity(capacity.max(4).min(dependent_bound));
        // Reverse edges are unique. Terminal frontiers need no visited set
        // or allocated traversal stack.
        for dependent in &self.entries[changed.0].reverse_imports {
            if *dependent == changed || !self.reachable.contains(dependent) {
                continue;
            }
            let entry = &self.entries[dependent.0];
            if let Some(document) = entry.document() {
                documents.push(document);
            }
            if !entry.reverse_imports.is_empty() {
                pending.push(*dependent);
            }
        }
        if pending.is_empty() {
            return documents;
        }
        let mut visited = HashSet::with_capacity(capacity + 1);
        visited.insert(changed);
        visited.extend(
            self.entries[changed.0]
                .reverse_imports
                .iter()
                .copied()
                .filter(|id| self.reachable.contains(id)),
        );
        while let Some(id) = pending.pop() {
            for dependent in &self.entries[id.0].reverse_imports {
                if !self.reachable.contains(dependent) || !visited.insert(*dependent) {
                    continue;
                }
                let entry = &self.entries[dependent.0];
                if let Some(document) = entry.document() {
                    documents.push(document);
                }
                if !entry.reverse_imports.is_empty() {
                    pending.push(*dependent);
                }
            }
        }
        documents
    }

    #[must_use]
    pub fn file_id_by_uri(&self, uri: &Url) -> Option<FileId> {
        self.by_uri.get(uri).copied()
    }
    pub(crate) fn ensure_file_id(&mut self, uri: Url, path: Option<PathBuf>) -> FileId {
        self.intern(uri, path)
    }

    #[must_use]
    pub fn file_id_by_path(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(&normalize_path(path)).copied()
    }

    fn intern(&mut self, uri: Url, path: Option<PathBuf>) -> FileId {
        if let Some(&id) = self.by_uri.get(&uri)
            && self.entries[id.0].uri == uri
            && self.entries[id.0].path == path
        {
            return id;
        }
        let path = path.map(|path| normalize_path(&path));
        let existing = self
            .by_uri
            .get(&uri)
            .copied()
            .filter(|id| self.entries[id.0].uri == uri)
            .or_else(|| {
                path.as_ref()
                    .and_then(|path| self.by_path.get(path).copied())
                    .filter(|id| self.entries[id.0].uri == uri)
            });
        if let Some(id) = existing {
            if self.entries[id.0].path != path {
                self.invalidate_semantic_file(id);
            }
            let entry = &mut self.entries[id.0];
            if let Some(path) = path {
                if entry.path.as_ref() != Some(&path)
                    && let Some(old_path) = entry.path.take()
                    && self.by_path.get(&old_path) == Some(&id)
                {
                    self.by_path.remove(&old_path);
                }
                entry.path = Some(path.clone());
                self.by_path.insert(path, id);
            } else if let Some(old_path) = entry.path.take()
                && self.by_path.get(&old_path) == Some(&id)
            {
                self.by_path.remove(&old_path);
            }
            self.by_uri.insert(uri, id);
            return id;
        }
        self.allocate_file(uri, path)
    }

    fn allocate_file(&mut self, uri: Url, path: Option<PathBuf>) -> FileId {
        let id = FileId(self.entries.len());
        self.entries.push(FileEntry {
            uri: uri.clone(),
            path: path.clone(),
            physical_path: None,
            language_id: "bend".into(),
            open_snapshot: None,
            disk_generation: 0,
            semantic_generation: 0,
            disk_snapshot: None,
            retired: false,
            imports: Box::default(),
            import_paths: Box::default(),
            import_rename_paths: Box::default(),
            import_watch_paths: Box::default(),
            editor_import_paths: None,
            reverse_imports: Vec::new(),
        });
        self.by_uri.insert(uri, id);
        if let Some(path) = path {
            self.by_path.insert(path, id);
        }
        id
    }

    fn refresh_imports_with_targets(
        &mut self,
        source: FileId,
        targets: Vec<PreparedImportTarget>,
    ) -> bool {
        let mut imports = Vec::with_capacity(targets.len());
        let mut import_paths = Vec::with_capacity(targets.len());
        let mut import_rename_paths = Vec::with_capacity(targets.len());
        let mut editor_paths = HashMap::new();
        for prepared in targets {
            match prepared {
                PreparedImportTarget::Native {
                    range,
                    path,
                    physical_path,
                    rename_path,
                } => {
                    let logical_path = path.clone();
                    if let Some(target) = self.intern_import(path, physical_path) {
                        import_paths.push(logical_path);
                        import_rename_paths.push(rename_path);
                        imports.push(ImportEdge {
                            path: range,
                            target,
                        });
                    }
                }
                PreparedImportTarget::Editor {
                    path, rename_path, ..
                } => {
                    editor_paths.insert(path, rename_path);
                }
            }
        }
        self.entries[source.0].editor_import_paths = Some(editor_paths);
        self.refresh_import_watch_paths(source);
        if self.entries[source.0].imports.as_ref() == imports.as_slice()
            && self.entries[source.0].import_paths.as_ref() == import_paths.as_slice()
            && self.entries[source.0].import_rename_paths.as_ref() == import_rename_paths.as_slice()
        {
            return false;
        }
        self.entries[source.0].import_paths = import_paths.into_boxed_slice();
        self.entries[source.0].import_rename_paths = import_rename_paths.into_boxed_slice();
        let old_imports = std::mem::replace(
            &mut self.entries[source.0].imports,
            imports.into_boxed_slice(),
        );
        let removed_target = old_imports.iter().any(|old| {
            !self.entries[source.0]
                .imports
                .iter()
                .any(|new| new.target == old.target)
        });
        for edge in old_imports {
            if !self.entries[source.0]
                .imports
                .iter()
                .any(|new| new.target == edge.target)
            {
                self.entries[edge.target.0]
                    .reverse_imports
                    .retain(|dependent| *dependent != source);
            }
        }
        for index in 0..self.entries[source.0].imports.len() {
            let target = self.entries[source.0].imports[index].target;
            let reverse = &mut self.entries[target.0].reverse_imports;
            if !reverse.contains(&source) {
                reverse.push(source);
            }
        }
        if removed_target {
            self.recompute_reachable();
        } else if self.is_needed(source) {
            for index in 0..self.entries[source.0].imports.len() {
                let target = self.entries[source.0].imports[index].target;
                self.extend_reachable(target);
            }
        }
        true
    }
}
pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir().unwrap_or_default().join(path);
        &absolute
    };
    let mut normalized = PathBuf::with_capacity(path.as_os_str().len());
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve directory identities without following a symlink in the final leaf.
/// Destinations may include folders that the client has not created yet.
pub(crate) fn canonical_directory_entry_path(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    let Some(parent) = normalized.parent() else {
        return normalized;
    };
    for ancestor in parent.ancestors() {
        if let Ok(canonical) = std::fs::canonicalize(ancestor)
            && let Ok(suffix) = normalized.strip_prefix(ancestor)
        {
            return canonical.join(suffix);
        }
    }
    normalized
}

pub(crate) fn canonical_source_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let normalized = normalize_path(path);
        normalized
            .parent()
            .zip(normalized.file_name())
            .and_then(|(parent, name)| {
                std::fs::canonicalize(parent)
                    .ok()
                    .map(|parent| parent.join(name))
            })
            .unwrap_or(normalized)
    })
}

pub(crate) fn canonical_library_path() -> Option<PathBuf> {
    hub_library_path().map(|path| canonical_source_path(&path))
}
fn hub_library_path() -> Option<PathBuf> {
    let path = std::env::var_os("BEND_LIB")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".bend").join("lib"))
        })?;
    Some(if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    })
}

fn is_hash_namespace_component(package: &str) -> bool {
    package.strip_prefix("0x").is_some_and(|hash| {
        !hash.is_empty()
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_module_word(word: &str) -> bool {
    word.as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Native loader namespace grammar, after removing a source path's `.bend`.
pub(super) fn valid_module_namespace(namespace: &str, in_library: bool) -> bool {
    let (hub, mut relative) = namespace
        .split_once('/')
        .filter(|(package, _)| is_hash_namespace_component(package))
        .map_or((false, namespace), |(_, relative)| (true, relative));
    if hub != in_library {
        return false;
    }
    if let Some(absolute) = relative.strip_prefix('/') {
        relative = absolute;
    } else {
        while let Some(parent) = relative.strip_prefix("../") {
            relative = parent;
        }
    }
    relative.split('/').all(valid_module_word)
}

/// Validate a real imported path without allocating its relative namespace.
fn valid_import_namespace_path(
    target: &Path,
    top: &Path,
    library: Option<&Path>,
    source_is_hub: bool,
) -> bool {
    if let Some(relative) = library.and_then(|library| target.strip_prefix(library).ok()) {
        return relative
            .to_str()
            .and_then(|relative| relative.strip_suffix(".bend"))
            .is_some_and(|namespace| valid_module_namespace(namespace, true));
    }
    if source_is_hub {
        return false;
    }
    let shared = target
        .components()
        .zip(top.components())
        .take_while(|(left, right)| left == right)
        .count();
    let mut components = target.components().skip(shared).peekable();
    if components.peek().is_none() {
        return false;
    }
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return false;
        };
        let Some(word) = component.to_str() else {
            return false;
        };
        let word = if components.peek().is_none() {
            let Some(stem) = word.strip_suffix(".bend") else {
                return false;
            };
            stem
        } else {
            word
        };
        if !valid_module_word(word) {
            return false;
        }
    }
    true
}

fn is_hash_package(package: &str) -> bool {
    package.len() == 34 && is_hash_namespace_component(package)
}

fn is_named_hub_package(package: &str) -> bool {
    let Some((name, version)) = package.split_once('@') else {
        return false;
    };
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return false;
    }
    let components: Vec<&str> = version.split('.').collect();
    components.len() == 4
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (*component == "0" || !component.starts_with('0'))
        })
}

pub(super) fn is_hub_import_path(imported: &str) -> bool {
    if imported.starts_with("./") || imported.starts_with("../") {
        return false;
    }
    let Some((package, _)) = imported.split_once('/') else {
        return false;
    };
    is_hash_namespace_component(package) || package.contains('@')
}

fn hub_cached_path(imported: &str, library: &Path) -> Option<PathBuf> {
    let (package, relative) = imported.split_once('/')?;
    if relative.is_empty() || !relative.ends_with(".bend") {
        return None;
    }
    let package = if is_hash_namespace_component(package) {
        package.to_owned()
    } else if is_named_hub_package(package) {
        let hash = std::fs::read_to_string(library.join("names").join(package))
            .ok()?
            .trim()
            .to_owned();
        if !is_hash_package(&hash) {
            return None;
        }
        hash
    } else {
        return None;
    };
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(
        relative
            .components()
            .fold(library.join(package), |path, component| {
                path.join(component.as_os_str())
            }),
    )
}

pub(crate) fn resolve_import_targets(
    source: &Path,
    physical_source: &Path,
    snapshot: &DocumentSnapshot,
) -> Vec<PreparedImportTarget> {
    analysis::imports(snapshot)
        .iter()
        .filter_map(|import| {
            let imported = import.path_text(&snapshot.text);
            if import.valid_syntax
                && let Some((path, physical_path, rename_path)) =
                    resolve_native_import_target(source, physical_source, imported)
            {
                return Some(PreparedImportTarget::Native {
                    range: import.path,
                    path,
                    physical_path,
                    rename_path,
                });
            }
            resolve_editor_import_target(source, imported).map(|(path, rename_path)| {
                PreparedImportTarget::Editor {
                    range: import.path,
                    path,
                    rename_path,
                }
            })
        })
        .collect()
}

fn resolve_native_import_target(
    source: &Path,
    physical_source: &Path,
    imported: &str,
) -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
    if imported == "Base" {
        return None;
    }
    let (candidate, physical_path) = if is_hub_import_path(imported) {
        let library = hub_library_path()?;
        let candidate = hub_cached_path(imported, &library)?;
        let namespace = candidate
            .strip_prefix(&library)
            .ok()?
            .to_str()?
            .strip_suffix(".bend")?;
        if !valid_module_namespace(namespace, true) {
            return None;
        }
        let physical = canonical_source_path(&candidate);
        (candidate, physical)
    } else {
        let namespace = imported.strip_suffix(".bend")?;
        let namespace = namespace.strip_prefix("./").unwrap_or(namespace);
        if !valid_module_namespace(namespace, false) {
            return None;
        }
        let candidate = normalize_path(&source.parent()?.join(imported));
        let physical = if source.parent() == physical_source.parent() {
            canonical_source_path(&candidate)
        } else {
            canonical_source_path(&physical_source.parent()?.join(imported))
        };
        (candidate, physical)
    };
    let path = if candidate == physical_path
        || source.parent() == physical_source.parent()
        || canonical_source_path(&candidate) == physical_path
    {
        candidate
    } else {
        physical_path.clone()
    };
    // Resolve directories, but never follow a symlink in the imported leaf:
    // moving a directory target must update its aliases, while renaming a
    // separately named symlink leaf must not rewrite the real file's imports.
    let rename_path = if path == physical_path {
        None
    } else {
        let rename_path = canonical_directory_entry_path(&path);
        (rename_path != physical_path).then_some(rename_path)
    };
    Some((path, physical_path, rename_path))
}

fn resolve_editor_import_target(
    source: &Path,
    imported: &str,
) -> Option<(PathBuf, Option<PathBuf>)> {
    if imported.is_empty() || imported == "Base" || is_hub_import_path(imported) {
        return None;
    }
    let path = normalize_path(&source.parent()?.join(imported));
    let path = if path.is_file() {
        path
    } else if path.extension().is_none() {
        let path = path.with_extension("bend");
        if !path.is_file() {
            return None;
        }
        path
    } else {
        return None;
    };
    let rename_path = canonical_directory_entry_path(&path);
    let rename_path = (rename_path != path).then_some(rename_path);
    Some((path, rename_path))
}

pub(crate) fn resolve_editor_import_paths(
    source: &Path,
    snapshot: &DocumentSnapshot,
) -> PreparedEditorImportPaths {
    analysis::imports(snapshot)
        .iter()
        .filter_map(|import| resolve_editor_import_target(source, import.path_text(&snapshot.text)))
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Document, PathRename, WorkspaceDb};
    use crate::analysis::Revision;
    use std::{fs, io};
    use url::Url;

    fn projected_renames(renames: &[PathRename]) -> Vec<PathRename> {
        renames
            .iter()
            .map(|rename| PathRename {
                old: super::canonical_directory_entry_path(&rename.old),
                new: super::canonical_directory_entry_path(&rename.new),
            })
            .collect()
    }

    #[test]
    fn editor_directory_alias_renames_use_prepared_coordinates_without_native_edges()
    -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let physical_workspace = directory.path().join("workspace");
        fs::create_dir(&physical_workspace)?;
        let workspace = directory.path().join("workspace_alias");
        std::os::unix::fs::symlink(&physical_workspace, &workspace)?;
        let canonical_workspace = fs::canonicalize(&workspace)?;
        let real = workspace.join("real");
        let leaf = workspace.join("leaf");
        fs::create_dir(&real)?;
        fs::create_dir(&leaf)?;
        for name in ["module", "módulo.bend"] {
            fs::write(real.join(name), "def value() -> U32:\n  0\n")?;
        }
        std::os::unix::fs::symlink(&real, workspace.join("link"))?;
        std::os::unix::fs::symlink(real.join("module"), leaf.join("module"))?;
        let source = "import ./link/module as A\nimport ./leaf/module as L\nimport ./real/module as R\nimport ./link/módulo.bend as U\nimport ./real/módulo.bend as V\ndef main() -> U32:\n  0\n";
        for source_workspace in [&workspace, &canonical_workspace] {
            let main = source_workspace.join("main.bend");
            let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("entry URI"))?;
            let mut database = WorkspaceDb::default();
            let root = database.set_open_document(
                Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
                Some(main),
            );
            assert!(database.editor_import_paths_ready(&uri));
            assert!(database.file_id_by_path(&real.join("module")).is_none());
            assert_eq!(
                database
                    .capture_semantic_view(root, None)
                    .ok_or_else(|| io::Error::other("editor-only entry"))?
                    .build()
                    .documents()
                    .len(),
                1
            );
            for (old, new, expected) in [
                (
                    real.join("module"),
                    real.join("renamed"),
                    ["./real/renamed", "./real/renamed"].as_slice(),
                ),
                (
                    real.join("módulo.bend"),
                    workspace.join("missing/nested/nuevo.bend"),
                    ["./missing/nested/nuevo.bend", "./missing/nested/nuevo.bend"].as_slice(),
                ),
                (
                    workspace.join("link"),
                    workspace.join("renamed_link"),
                    ["./renamed_link/module", "./renamed_link/módulo.bend"].as_slice(),
                ),
                (
                    leaf.clone(),
                    workspace.join("renamed_leaf"),
                    ["./renamed_leaf/module"].as_slice(),
                ),
            ] {
                let renames = [PathRename { old, new }];
                let projected = projected_renames(&renames);
                let changes = database
                    .rename_import_edits(&renames, &projected)
                    .ok_or_else(|| io::Error::other("editor rename edits"))?;
                let (_, edits) = changes
                    .iter()
                    .find(|(document, _)| document.uri == uri)
                    .ok_or_else(|| io::Error::other("editor source edits"))?;
                assert_eq!(
                    edits
                        .iter()
                        .map(|(_, text)| text.as_str())
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
        Ok(())
    }

    #[test]
    fn prepared_editor_paths_have_no_native_ownership_and_watch_absent_fallbacks() -> io::Result<()>
    {
        use crate::analysis::DocumentSnapshot;
        use std::sync::Arc;

        let directory = tempfile::tempdir()?;
        for name in ["module", "módulo.bend", "fallback.bend"] {
            fs::write(directory.path().join(name), "def value() -> U32:\n  0\n")?;
        }
        let main = directory.path().join("main.bend");
        let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("file URI"))?;
        let source = "import ./module as E\nimport ./módulo.bend as U\nimport ./fallback as F\nimport ./later as L\nimport ./missing.bend as M\n";
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
            Some(main),
        );
        database.load_reachable(&[root]);
        let document = database
            .cached_document(&uri)
            .ok_or_else(|| io::Error::other("entry document"))?;
        let source_path = uri
            .to_file_path()
            .map_err(|()| io::Error::other("entry path"))?;
        let imports = document.syntax.imports();
        assert!(database.editor_import_paths_ready(&uri));
        for (index, name) in [(0, "module"), (1, "módulo.bend"), (2, "fallback.bend")] {
            let path = directory.path().join(name);
            assert_eq!(
                database.editor_import_path(&uri, imports[index].path),
                Some(path.as_path())
            );
            assert!(database.import_target(&uri, imports[index].path).is_none());
            assert!(database.file_id_by_path(&path).is_none());
        }
        assert!(database.editor_import_uri(&uri, imports[3].path).is_none());
        assert!(database.editor_import_uri(&uri, imports[4].path).is_none());
        let captured = database
            .capture_semantic_view(root, None)
            .ok_or_else(|| io::Error::other("strict entry capture"))?;
        assert_eq!(captured.build().documents().len(), 1);

        let module = directory.path().join("module");
        fs::remove_file(&module)?;
        database.invalidate_editor_import_path(&module);
        assert!(!database.editor_import_paths_ready(&uri));
        let paths = super::resolve_editor_import_paths(&source_path, &document.snapshot);
        assert!(database.set_editor_import_paths(root, &document.snapshot, paths));
        assert!(database.editor_import_uri(&uri, imports[0].path).is_none());

        let later = directory.path().join("later.bend");
        fs::write(&later, "def value() -> U32:\n  0\n")?;
        database.invalidate_editor_import_path(&later);
        assert!(!database.editor_import_paths_ready(&uri));
        let paths = super::resolve_editor_import_paths(&source_path, &document.snapshot);
        assert!(database.set_editor_import_paths(root, &document.snapshot, paths));
        assert_eq!(
            database.editor_import_path(&uri, imports[3].path),
            Some(later.as_path())
        );
        database
            .update_open_snapshot(
                &uri,
                Arc::new(DocumentSnapshot::new(Revision(2), String::new())),
            )
            .ok_or_else(|| io::Error::other("new source revision"))?;
        assert!(!database.set_editor_import_paths(
            root,
            &document.snapshot,
            std::collections::HashMap::new(),
        ));
        assert!(database.editor_import_paths_ready(&uri));
        database
            .close_document(&uri)
            .ok_or_else(|| io::Error::other("closing entry"))?;
        assert!(database.editor_import_paths_ready(&uri));
        Ok(())
    }

    #[test]
    fn reverse_entry_roots_join_physical_aliases_and_index_base_importers() -> io::Result<()> {
        use crate::analysis::DocumentSnapshot;
        use std::sync::Arc;

        let directory = tempfile::tempdir()?;
        let real = directory.path().join("real.bend");
        let alias = directory.path().join("alias.bend");
        let value = "def value() -> U32:\n  0\n";
        fs::write(&real, value)?;
        std::os::unix::fs::symlink(&real, &alias)?;
        let mut database = WorkspaceDb::default();
        let mut consumers = Vec::new();
        for (name, imported, prelude) in [
            ("first.bend", "real.bend", "import Base\n"),
            ("second.bend", "alias.bend", ""),
        ] {
            let path = directory.path().join(name);
            let uri = Url::from_file_path(&path).map_err(|()| io::Error::other("file URI"))?;
            let root = database.set_open_document(
                Document::new(
                    uri.clone(),
                    "bend".into(),
                    Revision(1),
                    format!("{prelude}import ./{imported} as D\ndef main() -> U32:\n  D.value()\n"),
                ),
                Some(path),
            );
            database.load_reachable(&[root]);
            consumers.push((root, uri));
        }
        let physical = database
            .file_id_by_path(&real)
            .ok_or_else(|| io::Error::other("physical source"))?;
        let overlay_uri = Url::from_file_path(&alias).map_err(|()| io::Error::other("file URI"))?;
        let overlay = database.set_open_document(
            Document::new(
                overlay_uri.clone(),
                "bend".into(),
                Revision(1),
                value.into(),
            ),
            Some(alias),
        );
        let unrelated = directory.path().join("unrelated.bend");
        let unrelated_uri =
            Url::from_file_path(&unrelated).map_err(|()| io::Error::other("file URI"))?;
        let unrelated = database.set_open_document(
            Document::new(unrelated_uri, "bend".into(), Revision(1), value.into()),
            Some(unrelated),
        );
        let expected: Vec<_> = database
            .semantic_roots()
            .into_iter()
            .filter(|file| *file != unrelated)
            .collect();
        assert_eq!(database.semantic_entry_roots_for_file(physical), expected);
        assert_eq!(database.semantic_entry_roots_for_file(overlay), expected);
        assert_eq!(
            database.semantic_entry_roots_for_file(super::FileId::BASE),
            [consumers[0].0]
        );
        assert!(database.workspace_imports_prelude());
        database
            .update_open_snapshot(
                &consumers[0].1,
                Arc::new(DocumentSnapshot::new(
                    Revision(2),
                    "import ./real.bend as D\ndef main() -> U32:\n  D.value()\n".into(),
                )),
            )
            .ok_or_else(|| io::Error::other("removing Base import"))?;
        assert!(!database.workspace_imports_prelude());
        assert!(
            database
                .semantic_entry_roots_for_file(super::FileId::BASE)
                .is_empty()
        );
        for (_, uri) in consumers {
            database
                .close_document(&uri)
                .ok_or_else(|| io::Error::other("closing consumer"))?;
        }
        database
            .close_document(&overlay_uri)
            .ok_or_else(|| io::Error::other("closing physical overlay"))?;
        database.finish_load_reachable();
        assert!(database.semantic_entry_roots_for_file(physical).is_empty());
        Ok(())
    }

    #[test]
    fn unopened_needed_dependencies_have_their_own_prepared_entry_views() -> io::Result<()> {
        use std::sync::Arc;

        let directory = tempfile::tempdir()?;
        let dependency = directory.path().join("dep.bend");
        let main = directory.path().join("main.bend");
        fs::write(&dependency, "def value() -> U32:\n  0\n")?;
        let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("file URI"))?;
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(
                uri.clone(),
                "bend".into(),
                Revision(1),
                "import ./dep.bend as D\ndef main() -> U32:\n  D.value()\n".into(),
            ),
            Some(main),
        );
        database.load_reachable(&[root]);
        let dependency = database
            .file_id_by_path(&dependency)
            .ok_or_else(|| io::Error::other("dependency identity"))?;
        assert_eq!(database.semantic_roots(), [root, dependency]);
        assert_eq!(database.missing_semantic_roots(), [root, dependency]);
        assert_eq!(database.missing_open_semantic_roots(), [root]);
        assert!(!database.semantic_views_ready_for(Some(dependency)));
        for entry in [root, dependency] {
            let view = database
                .capture_semantic_view(entry, None)
                .ok_or_else(|| io::Error::other("entry capture"))?
                .build();
            assert!(database.publish_semantic_view(Arc::new(view), None));
        }
        assert!(database.semantic_views_ready_for(None));
        database
            .close_document(&uri)
            .ok_or_else(|| io::Error::other("closing entry"))?;
        database.finish_load_reachable();
        assert!(database.semantic_roots().is_empty());
        assert!(database.missing_semantic_roots().is_empty());
        assert!(database.semantic_view(root).is_none());
        assert!(database.semantic_view(dependency).is_none());
        Ok(())
    }

    #[test]
    fn discovered_snapshots_are_prepared_roots_until_deleted_or_removed() -> io::Result<()> {
        use crate::analysis::DocumentSnapshot;
        use std::sync::Arc;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("discovered.bend");
        let physical = super::canonical_source_path(&path);
        let snapshot = Arc::new(DocumentSnapshot::new(
            Revision::UNVERSIONED,
            "def value() -> U32:\n  0\n".into(),
        ));
        let mut database = WorkspaceDb::default();
        let root = database
            .discover_snapshot(&path, &physical, snapshot.clone(), Vec::new())
            .ok_or_else(|| io::Error::other("discovered source"))?;
        assert_eq!(database.semantic_roots(), [root]);
        assert_eq!(database.missing_semantic_roots(), [root]);
        assert!(database.missing_open_semantic_roots().is_empty());
        assert!(!database.semantic_views_ready_for(Some(root)));
        let view = Arc::new(
            database
                .capture_semantic_view(root, None)
                .ok_or_else(|| io::Error::other("discovered capture"))?
                .build(),
        );
        assert!(database.publish_semantic_view(view.clone(), None));
        assert!(database.semantic_views_ready_for(None));
        database
            .sync_disk_snapshot_prepared(&path, &physical, None, Vec::new())
            .ok_or_else(|| io::Error::other("deleted source"))?;
        assert!(database.semantic_view(root).is_none());
        assert!(database.semantic_roots().is_empty());
        assert!(database.missing_semantic_roots().is_empty());
        assert!(!database.validate_semantic_view(&view, None));
        database
            .discover_snapshot(&path, &physical, snapshot, Vec::new())
            .ok_or_else(|| io::Error::other("restored source"))?;
        assert_eq!(database.missing_semantic_roots(), [root]);
        let pending = database
            .capture_semantic_view(root, None)
            .ok_or_else(|| io::Error::other("restored capture"))?;
        database.retain_discovered_roots(&[]);
        assert!(database.semantic_roots().is_empty());
        assert!(database.missing_semantic_roots().is_empty());
        assert!(!database.publish_semantic_view(Arc::new(pending.build()), None));
        assert!(database.semantic_views_ready_for(None));
        Ok(())
    }

    #[test]
    fn editor_import_paths_require_live_snapshots_without_native_path_relaxation() -> io::Result<()>
    {
        use crate::analysis::DocumentSnapshot;
        use std::sync::Arc;

        let directory = tempfile::tempdir()?;
        let main = directory.path().join("main.bend");
        let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("file URI"))?;
        let source =
            "import ./missing.bend as M\nimport ./module as E\nimport ./módulo.bend as U\n";
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
            Some(main),
        );
        database.load_reachable(&[root]);
        for name in ["module", "módulo.bend"] {
            let path = directory.path().join(name);
            fs::write(&path, "def value() -> U32:\n  0\n")?;
            let snapshot = Arc::new(DocumentSnapshot::new(
                Revision::UNVERSIONED,
                "def value() -> U32:\n  0\n".into(),
            ));
            database
                .discover_snapshot(
                    &path,
                    &super::canonical_source_path(&path),
                    snapshot,
                    Vec::new(),
                )
                .ok_or_else(|| io::Error::other("editor source"))?;
        }
        let document = database
            .cached_document(&uri)
            .ok_or_else(|| io::Error::other("entry document"))?;
        let imports = document.syntax.imports();
        assert!(database.import_target(&uri, imports[0].path).is_none());
        assert!(database.editor_import_uri(&uri, imports[0].path).is_none());
        for (index, name) in [(1, "module"), (2, "módulo.bend")] {
            assert!(database.import_target(&uri, imports[index].path).is_none());
            assert_eq!(
                database.editor_import_path(&uri, imports[index].path),
                Some(directory.path().join(name).as_path())
            );
        }
        assert_eq!(
            database
                .capture_semantic_view(root, None)
                .ok_or_else(|| io::Error::other("strict semantic capture"))?
                .build()
                .documents()
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn file_rename_preserves_each_imports_logical_symlink_spelling() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let real = directory.path().join("real.bend");
        let alias = directory.path().join("alias.bend");
        let main = directory.path().join("main.bend");
        fs::write(&real, "def value() -> U32:\n  0\n")?;
        std::os::unix::fs::symlink(&real, &alias)?;
        let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("file URI"))?;
        for alias_first in [true, false] {
            let imports = if alias_first {
                "import ./alias.bend as A\nimport ./real.bend as R\n"
            } else {
                "import ./real.bend as R\nimport ./alias.bend as A\n"
            };
            let source = format!("{imports}def main() -> U32:\n  A.value()\n");
            for rename_alias in [true, false] {
                let mut database = WorkspaceDb::default();
                let root = database.set_open_document(
                    Document::new(uri.clone(), "bend".into(), Revision(1), source.clone()),
                    Some(main.clone()),
                );
                database.load_reachable(&[root]);
                let selected = if rename_alias { &alias } else { &real };
                let destination = directory.path().join(if rename_alias {
                    "renamed_alias.bend"
                } else {
                    "renamed_real.bend"
                });
                let renames = [PathRename {
                    old: selected.clone(),
                    new: destination,
                }];
                let projected = projected_renames(&renames);
                let changes = database
                    .rename_import_edits(&renames, &projected)
                    .ok_or_else(|| io::Error::other("import rename edits"))?;
                let (_, edits) = changes
                    .iter()
                    .find(|(document, _)| document.uri == uri)
                    .ok_or_else(|| io::Error::other("entry import edit"))?;
                assert_eq!(
                    edits.len(),
                    1,
                    "alias_first={alias_first}, rename_alias={rename_alias}"
                );
                let (range, replacement) = &edits[0];
                assert_eq!(
                    &source[range.start..range.end],
                    if rename_alias {
                        "./alias.bend"
                    } else {
                        "./real.bend"
                    },
                );
                assert_eq!(
                    replacement,
                    if rename_alias {
                        "./renamed_alias.bend"
                    } else {
                        "./renamed_real.bend"
                    },
                );
                let imports = database
                    .cached_document(&uri)
                    .ok_or_else(|| io::Error::other("entry snapshot"))?;
                let paths = imports.syntax.imports();
                let first = database
                    .import_target(&uri, paths[0].path)
                    .ok_or_else(|| io::Error::other("first imported source"))?;
                let second = database
                    .import_target(&uri, paths[1].path)
                    .ok_or_else(|| io::Error::other("second imported source"))?;
                assert!(std::sync::Arc::ptr_eq(&first.snapshot, &second.snapshot));
            }
        }
        Ok(())
    }

    #[test]
    fn directory_alias_renames_preserve_leaf_symlinks_and_import_order() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let physical_workspace = directory.path().join("workspace");
        fs::create_dir(&physical_workspace)?;
        let workspace = directory.path().join("workspace_alias");
        std::os::unix::fs::symlink(&physical_workspace, &workspace)?;
        let canonical_workspace = fs::canonicalize(&workspace)?;
        let real_directory = workspace.join("real");
        let leaf_directory = workspace.join("leaf");
        fs::create_dir(&real_directory)?;
        fs::create_dir(&leaf_directory)?;
        let real = real_directory.join("dep.bend");
        fs::write(&real, "def value() -> U32:\n  0\n")?;
        std::os::unix::fs::symlink(&real_directory, workspace.join("link"))?;
        std::os::unix::fs::symlink(&real, leaf_directory.join("dep.bend"))?;
        for source_workspace in [&workspace, &canonical_workspace] {
            let main = source_workspace.join("main.bend");
            let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("entry URI"))?;
            for directory_alias_first in [true, false] {
                let imports = if directory_alias_first {
                    "import ./link/dep.bend as A\nimport ./leaf/dep.bend as L\nimport ./real/dep.bend as R\n"
                } else {
                    "import ./real/dep.bend as R\nimport ./leaf/dep.bend as L\nimport ./link/dep.bend as A\n"
                };
                let source = format!("{imports}def main() -> U32:\n  A.value()\n");
                let mut database = WorkspaceDb::default();
                let root = database.set_open_document(
                    Document::new(uri.clone(), "bend".into(), Revision(1), source.clone()),
                    Some(main.clone()),
                );
                database.load_reachable(&[root]);
                for (old, new, replacement, count) in [
                    (
                        real.clone(),
                        real_directory.join("renamed.bend"),
                        "./real/renamed.bend",
                        2,
                    ),
                    (
                        workspace.join("link"),
                        workspace.join("renamed_link"),
                        "./renamed_link/dep.bend",
                        1,
                    ),
                    (
                        leaf_directory.clone(),
                        workspace.join("renamed_leaf"),
                        "./renamed_leaf/dep.bend",
                        1,
                    ),
                ] {
                    let renames = [PathRename { old, new }];
                    let projected = projected_renames(&renames);
                    let changes = database
                        .rename_import_edits(&renames, &projected)
                        .ok_or_else(|| io::Error::other("directory alias edits"))?;
                    let (_, edits) = changes
                        .iter()
                        .find(|(document, _)| document.uri == uri)
                        .ok_or_else(|| io::Error::other("entry edits"))?;
                    assert_eq!(
                        edits.len(),
                        count,
                        "directory_alias_first={directory_alias_first}"
                    );
                    assert!(edits.iter().all(|(_, text)| text == replacement));
                    if count == 2 {
                        assert!(edits.iter().all(|(range, _)| {
                            &source[range.start..range.end] != "./leaf/dep.bend"
                        }));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn native_module_paths_validate_suffixes_and_real_namespaces() {
        for (namespace, library, valid) in [
            ("dep", false, true),
            ("../dep", false, true),
            ("/workspace/project/dep", false, true),
            ("nested-name/dep_2", false, true),
            ("dep.member", false, false),
            ("dep/../other", false, false),
            ("2dep", false, false),
            ("módulo", false, false),
            ("0xab/dep", false, false),
            ("0xab/dep", true, true),
            ("dep", true, false),
        ] {
            assert_eq!(
                super::valid_module_namespace(namespace, library),
                valid,
                "{namespace}, library={library}"
            );
        }
        let main = std::path::PathBuf::from("/workspace/project/main.bend");
        let dependency = std::path::PathBuf::from("/workspace/project/dep.bend");
        let Ok(uri) = Url::from_file_path(&main) else {
            panic!("absolute file URI");
        };
        let source = "import /workspace/project/dep.bend as D\ndef main() -> U32:\n  D.value()\n";
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(uri, "bend".into(), Revision(1), source.into()),
            Some(main),
        );
        assert!(
            database
                .sync_disk_path(&dependency, Some("def value() -> U32:\n  0\n".into()),)
                .is_some()
        );
        let Some(input) = database.capture_semantic_view(root, None) else {
            panic!("absolute import capture");
        };
        let view = input.build();
        let Some(offset) = source.rfind("D.value") else {
            panic!("absolute import source spelling");
        };
        let Some(occurrence) = view.resolve_at(root, offset) else {
            panic!("absolute import occurrence");
        };
        assert!(matches!(
            occurrence.target,
            super::SemanticTarget::Global(_)
        ));
        assert_eq!(
            database.source_graph(root).map(|graph| graph.nodes.len()),
            Some(2)
        );
    }

    #[test]
    fn editor_only_imports_keep_navigation_and_literal_path_rename_edits() -> io::Result<()> {
        let main = std::path::PathBuf::from("/workspace/project/main.bend");
        let dependency = std::path::PathBuf::from("/workspace/project/dep.bend");
        let unicode = std::path::PathBuf::from("/workspace/project/módulo.bend");
        let uri = Url::from_file_path(&main).map_err(|()| io::Error::other("entry URI"))?;
        let source =
            "import ./dep as D\nimport ./módulo.bend as U\ndef main() -> U32:\n  D.value()\n";
        let mut database = WorkspaceDb::default();
        assert!(database.admit_discovered_path(dependency.clone()).is_some());
        assert!(database.admit_discovered_path(unicode.clone()).is_some());
        // Open overlays prove editor availability without requiring disk files;
        // discovery admission alone does not supply a source payload.
        for target in [&dependency, &unicode] {
            let target_uri =
                Url::from_file_path(target).map_err(|()| io::Error::other("target URI"))?;
            database.set_open_document(
                Document::new(
                    target_uri,
                    "bend".into(),
                    Revision(1),
                    "def value() -> U32:\n  0\n".into(),
                ),
                Some(target.clone()),
            );
        }
        let root = database.set_open_document(
            Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
            Some(main.clone()),
        );
        let document = database
            .open_document(&uri)
            .ok_or_else(|| io::Error::other("entry snapshot"))?;
        let imports = document.syntax.imports();
        assert_eq!(imports.len(), 2);
        for (import, expected) in imports.iter().zip([&dependency, &unicode]) {
            assert!(database.import_target(&uri, import.path).is_none());
            assert_eq!(
                database.editor_import_target_path(&main, import.path_text(&document.text)),
                Some(expected.as_path())
            );
            assert_eq!(
                database.editor_import_uri(&uri, import.path),
                Url::from_file_path(expected).ok()
            );
        }
        let renames = [
            PathRename {
                old: dependency,
                new: main.with_file_name("renamed_dep.bend"),
            },
            PathRename {
                old: unicode,
                new: main.with_file_name("renamed_módulo.bend"),
            },
        ];
        let changes = database
            .rename_import_edits(&renames, &renames)
            .ok_or_else(|| io::Error::other("indexed editor path edits"))?;
        let (_, indexed_edits) = changes
            .iter()
            .find(|(document, _)| document.uri == uri)
            .ok_or_else(|| io::Error::other("entry path edits"))?;
        let literal_edits = super::import_rename_edits(&main, &document, &renames, |_| None)
            .ok_or_else(|| io::Error::other("staged literal path edits"))?;
        assert_eq!(*indexed_edits, literal_edits);
        assert_eq!(
            literal_edits
                .iter()
                .map(|(_, replacement)| replacement.as_str())
                .collect::<Vec<_>>(),
            ["./renamed_dep", "./renamed_módulo.bend"]
        );
        let input = database
            .capture_semantic_view(root, None)
            .ok_or_else(|| io::Error::other("strict semantic capture"))?;
        assert_eq!(input.build().documents().len(), 1);
        Ok(())
    }
}
