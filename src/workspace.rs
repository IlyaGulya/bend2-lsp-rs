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
    ExternalReferenceGroup, GlobalSymbolId, PreparedSemanticSnapshot, WorkspaceCallGroup,
    WorkspaceIndexStats, WorkspaceOccurrence, WorkspaceSymbol, prepare_semantic_snapshot,
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

/// Workspace-lifetime file identity, shared by normalized path and URI aliases.
///
/// Entries are never removed or recycled. A deleted file retains a tombstone
/// (identity and reverse-import links, but no effective snapshot); recreating that path
/// reuses its `FileId`. Closing an overlay restores the disk snapshot, not a new
/// file identity. IDs are meaningful only within the `WorkspaceDb` that issued them.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileId(usize);

/// Membership for dense, workspace-owned identities; iteration visits only members.
#[derive(Default)]
struct FileSet {
    members: Vec<FileId>,
    present: Vec<bool>,
}

impl FileSet {
    fn contains(&self, id: FileId) -> bool {
        self.present.get(id.0).copied().unwrap_or(false)
    }

    fn insert(&mut self, id: FileId) -> bool {
        if self.contains(id) {
            return false;
        }
        if id.0 >= self.present.len() {
            self.present.resize(id.0 + 1, false);
        }
        self.present[id.0] = true;
        self.members.push(id);
        true
    }

    fn iter(&self) -> std::slice::Iter<'_, FileId> {
        self.members.iter()
    }

    fn extend(&mut self, ids: impl IntoIterator<Item = FileId>) {
        for id in ids {
            self.insert(id);
        }
    }
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
    disk_snapshot: Option<Arc<DocumentSnapshot>>,
    imports: Box<[ImportEdge]>,
    import_by_range: HashMap<analysis::TextRange, FileId>,
    reverse_imports: Vec<FileId>,
    semantic_snapshot: Option<Arc<DocumentSnapshot>>,
    semantic_epoch: Option<u64>,
    semantic_active: bool,
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
    reachable: FileSet,
    reachability_dirty: bool,
    semantic: semantic::SemanticIndex,
}

impl WorkspaceDb {
    /// Register compiler-owned Base source and its provenance for navigation and
    /// indexed call resolution. Retain its backing file for the lifetime of the
    /// workspace index, not just the active compiler cache.
    pub(crate) fn register_compiler_document(
        &mut self,
        uri: Url,
        path: PathBuf,
        semantics: PreparedSemanticSnapshot,
        directory: tempfile::TempDir,
    ) {
        let id = self.intern(uri, Some(path));
        self.entries[id.0].disk_snapshot = Some(semantics.snapshot().clone());
        self.compiler_documents
            .get_or_insert_with(HashMap::new)
            .insert(id, directory);
        self.semantic.compiler_base = Some(id);
        if self.entries[id.0].open_snapshot.is_none() {
            self.install_semantics(id, Some(semantics), false);
        }
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
        let target = *self.entries[source_id.0]
            .import_by_range
            .get(&import_path)?;
        self.entries[target.0].document()
    }

    pub fn open_documents(&self) -> Vec<Document> {
        self.entries
            .iter()
            .filter(|entry| entry.open_snapshot.is_some())
            .filter_map(FileEntry::document)
            .collect()
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
        let semantics = prepare_semantic_snapshot(document.snapshot.clone());
        self.set_open_document_prepared(document, path, imports, semantics)
    }

    pub(crate) fn set_open_document_prepared(
        &mut self,
        document: Document,
        path: Option<PathBuf>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
        semantics: PreparedSemanticSnapshot,
    ) -> FileId {
        let id = self.intern(document.uri.clone(), path);
        let entry = &mut self.entries[id.0];
        entry.language_id = document.language_id;
        entry.open_snapshot = Some(semantics.snapshot().clone());
        let imports_changed = self.refresh_imports_with_targets(id, imports);
        self.install_semantics(id, Some(semantics), imports_changed);
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
        let semantics = prepare_semantic_snapshot(snapshot);
        self.update_open_snapshot_prepared(uri, semantics, imports)
    }

    pub(crate) fn update_open_snapshot_prepared(
        &mut self,
        uri: &Url,
        semantics: PreparedSemanticSnapshot,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> Option<(FileId, bool)> {
        let id = *self.by_uri.get(uri)?;
        let entry = self.entries.get_mut(id.0)?;
        entry.open_snapshot = Some(semantics.snapshot().clone());
        let imports_changed = self.refresh_imports_with_targets(id, imports);
        self.install_semantics(id, Some(semantics), imports_changed);
        Some((id, imports_changed))
    }

    pub fn close_document(&mut self, uri: &Url) -> Option<FileId> {
        let disk = self.disk_document(uri);
        let imports = disk.as_ref().map_or_else(Vec::new, |(path, snapshot)| {
            resolve_import_targets(path, snapshot)
        });
        let semantics = disk.map(|(_, snapshot)| prepare_semantic_snapshot(snapshot));
        self.close_document_prepared(uri, imports, semantics)
    }

    pub(crate) fn close_document_prepared(
        &mut self,
        uri: &Url,
        imports: Vec<(analysis::TextRange, PathBuf)>,
        semantics: Option<PreparedSemanticSnapshot>,
    ) -> Option<FileId> {
        let id = *self.by_uri.get(uri)?;
        let entry = self.entries.get_mut(id.0)?;
        let restored = semantics
            .as_ref()
            .map(|prepared| prepared.snapshot().clone());
        if !semantic::same_snapshot(entry.disk_snapshot.as_ref(), restored.as_ref()) {
            return None;
        }
        if entry.open_snapshot.take().is_some() {
            self.reachability_dirty = true;
        }
        self.entries[id.0].language_id = "bend".into();
        let imports_changed = self.refresh_imports_with_targets(id, imports);
        self.install_semantics(id, semantics, imports_changed);
        Some(id)
    }

    pub fn sync_disk_path(&mut self, path: &Path, text: Option<String>) -> Option<(FileId, bool)> {
        let snapshot =
            text.map(|text| Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text)));
        let imports = snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| resolve_import_targets(path, snapshot));
        let semantics = snapshot.map(prepare_semantic_snapshot);
        self.sync_disk_snapshot_prepared(path, semantics, imports)
    }

    pub(crate) fn sync_disk_snapshot_prepared(
        &mut self,
        path: &Path,
        semantics: Option<PreparedSemanticSnapshot>,
        imports: Vec<(analysis::TextRange, PathBuf)>,
    ) -> Option<(FileId, bool)> {
        let path = normalize_path(path);
        let uri = Url::from_file_path(&path).ok()?;
        let id = self.intern(uri, Some(path));
        self.entries[id.0].disk_snapshot = semantics
            .as_ref()
            .map(|prepared| prepared.snapshot().clone());
        // The prepared imports belong to the disk snapshot, not an open overlay
        // that may have changed while the disk snapshot was being constructed.
        let imports_changed = self.entries[id.0].open_snapshot.is_none()
            && self.refresh_imports_with_targets(id, imports);
        if self.entries[id.0].open_snapshot.is_none() {
            self.install_semantics(id, semantics, imports_changed);
        }
        Some((id, imports_changed))
    }

    pub(crate) fn document_path(&self, uri: &Url) -> Option<PathBuf> {
        self.entries.get(self.by_uri.get(uri)?.0)?.path.clone()
    }

    pub(crate) fn disk_document(&self, uri: &Url) -> Option<(PathBuf, Arc<DocumentSnapshot>)> {
        let entry = self.entries.get(self.by_uri.get(uri)?.0)?;
        Some((entry.path.clone()?, entry.disk_snapshot.clone()?))
    }

    pub(crate) fn missing_disk_paths(&self, roots: &[FileId]) -> Vec<PathBuf> {
        let mut pending = roots.to_vec();
        let mut visited = FileSet::default();
        let mut paths = Vec::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
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
        let extend_reachable = roots
            .iter()
            .any(|id| self.entries[id.0].open_snapshot.is_some() || self.reachable.contains(*id));
        let mut pending = roots.to_vec();
        let mut visited = FileSet::default();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
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
        if self.reachability_dirty {
            self.recompute_reachable();
        } else if extend_reachable {
            self.reachable.extend(visited.members);
        }
    }

    pub(crate) fn finish_load_reachable(&mut self, roots: &[FileId]) {
        let extend_reachable = roots
            .iter()
            .any(|id| self.entries[id.0].open_snapshot.is_some() || self.reachable.contains(*id));
        let mut pending = roots.to_vec();
        let mut visited = FileSet::default();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let Some(entry) = self.entries.get(id.0) else {
                continue;
            };
            pending.extend(entry.imports.iter().map(|edge| edge.target));
        }
        if self.reachability_dirty {
            self.recompute_reachable();
        } else if extend_reachable {
            self.reachable.extend(visited.members);
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

    #[must_use]
    pub fn indexed_documents(&self) -> Vec<Document> {
        self.reachable
            .iter()
            .filter_map(|id| self.entries[id.0].document())
            .collect()
    }

    fn recompute_reachable(&mut self) {
        let mut pending: Vec<FileId> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.open_snapshot.is_some())
            .map(|(index, _)| FileId(index))
            .collect();
        let mut reachable = FileSet::default();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            pending.extend(self.entries[id.0].imports.iter().map(|edge| edge.target));
        }
        self.reachable = reachable;
        self.reachability_dirty = false;
    }

    #[must_use]
    pub fn dependents(&self, changed: FileId) -> Vec<Document> {
        if self.reachability_dirty || !self.reachable.contains(changed) {
            return Vec::new();
        }
        let mut pending = vec![changed];
        let mut visited = FileSet::default();
        visited.insert(changed);
        let mut documents = Vec::new();
        while let Some(id) = pending.pop() {
            for dependent in &self.entries[id.0].reverse_imports {
                if !self.reachable.contains(*dependent) || !visited.insert(*dependent) {
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
            disk_snapshot: None,
            imports: Box::default(),
            import_by_range: HashMap::new(),
            reverse_imports: Vec::new(),
            semantic_snapshot: None,
            semantic_epoch: Some(0),
            semantic_active: false,
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
        let mut imports = Vec::new();
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
        let old_imports = std::mem::take(&mut self.entries[source.0].imports);
        let import_by_range: HashMap<_, _> = imports
            .iter()
            .map(|edge| (edge.path, edge.target))
            .collect();
        if old_imports
            .iter()
            .any(|old| import_by_range.get(&old.path) != Some(&old.target))
        {
            self.reachability_dirty = true;
        }
        for edge in old_imports {
            self.entries[edge.target.0]
                .reverse_imports
                .retain(|dependent| *dependent != source);
        }
        for edge in &imports {
            let reverse = &mut self.entries[edge.target.0].reverse_imports;
            if !reverse.contains(&source) {
                reverse.push(source);
            }
        }
        self.entries[source.0].import_by_range = import_by_range;
        self.entries[source.0].imports = imports.into_boxed_slice();
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
