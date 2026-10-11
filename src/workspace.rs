use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::analysis::{self, DocumentSnapshot, Revision};
use url::Url;

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
    target_for: impl Fn(analysis::TextRange) -> Option<&'a Path>,
) -> Option<Vec<(analysis::TextRange, String)>> {
    let new_source = renamed_path(source, renames);
    let mut edits = Vec::new();
    for import in analysis::imports(snapshot) {
        let imported = import.path_text(&snapshot.text);
        if imported == "Base" || is_hub_import_path(imported) {
            continue;
        }
        let fallback;
        let target = if let Some(target) = target_for(import.path) {
            target
        } else {
            fallback = normalize_path(&source.parent()?.join(imported));
            &fallback
        };
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

struct FileEntry {
    uri: Url,
    path: Option<PathBuf>,
    language_id: String,
    open_snapshot: Option<Arc<DocumentSnapshot>>,
    disk_generation: u64,
    disk_snapshot: Option<Arc<DocumentSnapshot>>,
    retired: bool,
    imports: Box<[ImportEdge]>,
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

#[derive(Default)]
pub struct WorkspaceDb {
    entries: Vec<FileEntry>,
    by_uri: HashMap<Url, FileId>,
    by_path: HashMap<PathBuf, FileId>,
    compiler_documents: Option<HashMap<FileId, tempfile::TempDir>>,
    // User file identities stay interned; graph liveness governs their payloads.
    reachable: HashSet<FileId>,
    discovered: HashSet<FileId>,
    retired: Vec<FileId>,
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
        self.discovered.insert(id);
        self.extend_reachable(id);
        Some(id)
    }
    pub(crate) fn discover_snapshot(
        &mut self,
        path: &Path,
        snapshot: Arc<DocumentSnapshot>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> Option<FileId> {
        let id = self.admit_discovered_path(path.to_path_buf())?;
        self.sync_disk_snapshot_prepared(path, Some(snapshot), imports)?;
        Some(id)
    }
    pub(crate) fn retain_discovered_roots(&mut self, roots: &[PathBuf]) {
        let previous = self.discovered.len();
        let entries = &self.entries;
        self.discovered.retain(|id| {
            entries[id.0]
                .path
                .as_ref()
                .is_some_and(|path| roots.iter().any(|root| path.starts_with(root)))
        });
        if self.discovered.len() != previous {
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
        snapshot: Arc<DocumentSnapshot>,
        directory: tempfile::TempDir,
    ) {
        let id = self.intern(uri, Some(path));
        self.release_retired_payload(id);
        self.entries[id.0].disk_snapshot = Some(snapshot);
        self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
        self.compiler_documents
            .get_or_insert_with(HashMap::new)
            .insert(id, directory);
    }

    #[must_use]
    pub fn open_document(&self, uri: &Url) -> Option<Document> {
        let id = self.by_uri.get(uri)?;
        let entry = &self.entries[id.0];
        entry.open_snapshot.as_ref()?;
        entry.document()
    }

    pub(crate) fn is_document_open(&self, uri: &Url) -> bool {
        self.by_uri
            .get(uri)
            .is_some_and(|id| self.entries[id.0].open_snapshot.is_some())
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
                analysis::imports(snapshot)
                    .iter()
                    .any(|import| import.path_text(&snapshot.text) == "Base")
            })
    }

    pub(crate) fn workspace_imports_prelude(&self) -> bool {
        self.entries.iter().enumerate().any(|(index, entry)| {
            (entry.open_snapshot.is_some() || self.reachable.contains(&FileId(index)))
                && entry.snapshot().is_some_and(|snapshot| {
                    analysis::imports(snapshot)
                        .iter()
                        .any(|import| import.path_text(&snapshot.text) == "Base")
                })
        })
    }

    #[must_use]
    pub fn cached_document(&self, uri: &Url) -> Option<Document> {
        self.entries.get(self.by_uri.get(uri)?.0)?.document()
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
                let path = snapshot.text.get(edge.path.start..edge.path.end)?;
                if edge.target == target && is_hub_import_path(path) {
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
        let Some(source_path) = self
            .file_id_by_uri(source)
            .and_then(|id| self.entries[id.0].path.as_deref())
        else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        self.visit_indexed_entries(|id, entry| {
            let Some(snapshot) = entry.snapshot() else {
                return;
            };
            if entry.uri == *source
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
            if let Some(document) = entry.document() {
                candidates.push((document, relative, items));
            }
        });
        candidates.sort_unstable_by(|left, right| left.1.cmp(&right.1));
        candidates
    }

    pub(crate) fn rename_import_edits(
        &self,
        renames: &[PathRename],
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
                    .find(|edge| edge.path == range)
                    .and_then(|edge| self.entries[edge.target.0].path.as_deref())
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
    pub(crate) fn rename_paths(&mut self, renames: &[PathRename]) -> Vec<(FileId, FileId)> {
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
                self.entries[destination.0].disk_generation =
                    self.entries[destination.0].disk_generation.wrapping_add(1);
                let retired = if canonical == id { destination } else { id };
                if self.discovered.remove(&retired) {
                    self.discovered.insert(canonical);
                }
                // Import edges belong to the effective snapshot. A destination
                // may already have been loaded by the client's preceding edit.
                let transfer_imports = self.entries[canonical.0].snapshot().is_none()
                    || (self.entries[canonical.0].open_snapshot.is_none()
                        && self.entries[retired.0].open_snapshot.is_some());
                if self.entries[canonical.0].disk_snapshot.is_none() {
                    self.entries[canonical.0].disk_snapshot =
                        self.entries[retired.0].disk_snapshot.take();
                }
                if self.entries[canonical.0].open_snapshot.is_none() {
                    self.entries[canonical.0].open_snapshot =
                        self.entries[retired.0].open_snapshot.take();
                }
                if transfer_imports {
                    self.entries[canonical.0].imports =
                        std::mem::take(&mut self.entries[retired.0].imports);
                }
                self.entries[retired.0].open_snapshot = None;
                self.entries[retired.0].disk_snapshot = None;
                self.entries[retired.0].path = None;
                self.entries[retired.0].imports = Box::default();
                merged.push((retired, canonical));
            }
            self.entries[canonical.0].uri = uri.clone();
            self.entries[canonical.0].path = Some(path.clone());
            self.by_path.insert(path, canonical);
            self.by_uri.insert(uri, canonical);
        }
        for entry in &mut self.entries {
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
                    if imported != "Base"
                        && !Path::new(imported).is_absolute()
                        && !is_hub_import_path(imported)
                    {
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
        let imports = path.as_deref().map_or_else(Vec::new, |path| {
            resolve_import_targets(path, &document.snapshot)
        });
        self.set_open_document_prepared(document, path, imports)
    }

    pub(crate) fn set_open_document_prepared(
        &mut self,
        document: Document,
        path: Option<PathBuf>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> FileId {
        let id = self.intern(document.uri.clone(), path);
        self.release_retired_payload(id);
        let entry = &mut self.entries[id.0];
        entry.language_id = document.language_id;
        entry.open_snapshot = Some(document.snapshot);
        self.refresh_imports_with_targets(id, imports);
        self.extend_reachable(id);
        id
    }

    pub fn update_open_snapshot(
        &mut self,
        uri: &Url,
        snapshot: Arc<DocumentSnapshot>,
    ) -> Option<(FileId, bool)> {
        let imports = self
            .document_path(uri)
            .map_or_else(Vec::new, |path| resolve_import_targets(&path, &snapshot));
        self.update_open_snapshot_prepared(uri, snapshot, imports)
    }

    pub(crate) fn update_open_snapshot_prepared(
        &mut self,
        uri: &Url,
        snapshot: Arc<DocumentSnapshot>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> Option<(FileId, bool)> {
        let id = *self.by_uri.get(uri)?;
        self.release_retired_payload(id);
        let entry = self.entries.get_mut(id.0)?;
        let was_open = entry.open_snapshot.is_some();
        entry.open_snapshot = Some(snapshot);
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
        entry.open_snapshot.take()?;
        // Generated navigation source has independent workspace ownership.
        // User disk snapshots and overlay imports must be loaded afresh.
        if !compiler_owned {
            entry.disk_snapshot = None;
        }
        entry.disk_generation = entry.disk_generation.wrapping_add(1);
        entry.language_id = "bend".into();
        let imports = if compiler_owned {
            entry
                .path
                .as_deref()
                .zip(entry.disk_snapshot.as_ref())
                .map_or_else(Vec::new, |(path, snapshot)| {
                    resolve_import_targets(path, snapshot)
                })
        } else {
            Vec::new()
        };
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
        let imports = snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| resolve_import_targets(&path, snapshot));
        self.sync_disk_snapshot_prepared(&path, snapshot, imports)
    }

    pub(crate) fn sync_disk_snapshot_prepared(
        &mut self,
        path: &Path,
        snapshot: Option<Arc<DocumentSnapshot>>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> Option<(FileId, bool)> {
        let path = normalize_path(path);
        let id = *self.by_path.get(&path)?;
        if !self.is_needed(id) {
            return None;
        }
        self.entries[id.0].disk_snapshot = snapshot;
        self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
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
        let mut documents = Vec::new();
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
        for id in self.reachable.difference(&reachable) {
            self.entries[id.0].disk_generation = self.entries[id.0].disk_generation.wrapping_add(1);
            self.entries[id.0].retired = true;
            self.retired.push(*id);
        }
        self.reachable = reachable;
    }

    #[must_use]
    pub fn dependents(&self, changed: FileId) -> Vec<Document> {
        if !self.reachable.contains(&changed) {
            return Vec::new();
        }
        let mut pending = vec![changed];
        let mut visited = HashSet::from([changed]);
        let mut documents = Vec::new();
        while let Some(id) = pending.pop() {
            for dependent in &self.entries[id.0].reverse_imports {
                if !self.reachable.contains(dependent) || !visited.insert(*dependent) {
                    continue;
                }
                let entry = &self.entries[dependent.0];
                if let Some(document) = entry.document() {
                    documents.push(document);
                }
                pending.push(*dependent);
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
        let path = path.map(|path| normalize_path(&path));
        let existing = path
            .as_ref()
            .and_then(|path| self.by_path.get(path).copied())
            .or_else(|| self.by_uri.get(&uri).copied());
        if let Some(id) = existing {
            let entry = &mut self.entries[id.0];
            if entry.uri != uri {
                if self.by_uri.get(&entry.uri) == Some(&id) {
                    self.by_uri.remove(&entry.uri);
                }
                entry.uri = uri.clone();
            }
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
        let id = FileId(self.entries.len());
        self.entries.push(FileEntry {
            uri: uri.clone(),
            path: path.clone(),
            language_id: "bend".into(),
            open_snapshot: None,
            disk_generation: 0,
            disk_snapshot: None,
            retired: false,
            imports: Box::default(),
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
        targets: Vec<(analysis::TextRange, PathBuf)>,
    ) -> bool {
        let mut imports = Vec::with_capacity(targets.len());
        for (import_path, target_path) in targets {
            if let Ok(uri) = Url::from_file_path(&target_path) {
                let target = self.intern(uri, Some(target_path));
                imports.push(ImportEdge {
                    path: import_path,
                    target,
                });
            }
        }
        if self.entries[source.0].imports.as_ref() == imports.as_slice() {
            return false;
        }
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
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
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

fn is_hash_package(package: &str) -> bool {
    package.strip_prefix("0x").is_some_and(|hash| {
        hash.len() == 32
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
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
    let hash = package.strip_prefix("0x").unwrap_or("");
    is_hash_package(package)
        || !hash.is_empty()
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || package.contains('@')
}

fn hub_cached_path(imported: &str) -> Option<PathBuf> {
    let library = hub_library_path()?;
    let (package, relative) = imported.split_once('/')?;
    if relative.is_empty() || !relative.ends_with(".bend") {
        return None;
    }
    let package = if is_hash_package(package) {
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

pub(super) fn import_target_path(source: &Path, imported: &str) -> Option<PathBuf> {
    if imported == "Base" || Path::new(imported).is_absolute() {
        return None;
    }
    if let Some(path) = hub_cached_path(imported) {
        return Some(path);
    }
    if is_hub_import_path(imported) {
        return None;
    }
    Some(normalize_path(&source.parent()?.join(imported)))
}
pub(crate) fn resolve_import_targets(
    source: &Path,
    snapshot: &DocumentSnapshot,
) -> Vec<(analysis::TextRange, PathBuf)> {
    analysis::imports(snapshot)
        .iter()
        .filter_map(|import| {
            import_target_path(source, import.path_text(&snapshot.text))
                .map(|target| (import.path, target))
        })
        .collect()
}
