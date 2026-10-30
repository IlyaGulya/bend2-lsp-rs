use std::{
    collections::HashMap,
    iter::FusedIterator,
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{Document, FileId, ImportEdge, WorkspaceDb};
use crate::analysis::{DeclarationKind, DocumentSnapshot, ReferenceRole, SymbolId, TextRange};

mod build;
mod completion;
mod imports;
mod index;
mod physical;
mod rename;
mod watch;

use index::CompactMap;

#[derive(Default)]
struct PhysicalCapture<'a> {
    generations: HashMap<Arc<Path>, u64>,
    conflicts: std::collections::HashSet<std::path::PathBuf>,
    selected: HashMap<&'a Path, FileId>,
    overlays: HashMap<&'a Path, bool>,
}

impl PhysicalCapture<'_> {
    fn shared_path(&self, path: &Path) -> Arc<Path> {
        self.generations
            .get_key_value(path)
            .map_or_else(|| Arc::from(path), |(shared, _)| Arc::clone(shared))
    }
}

static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);

/// Entry views share the compiler prelude's immutable document snapshot.
#[derive(Clone)]
pub struct SemanticBase {
    pub document: Document,
    pub generation: u64,
    pub provenance: Arc<str>,
    pub source_lease: Option<Arc<tempfile::TempDir>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SemanticEntityId {
    pub view: u64,
    pub index: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SemanticBindingId {
    pub view: u64,
    pub file: FileId,
    pub symbol: SymbolId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticTarget {
    Global(SemanticEntityId),
    Local(SemanticBindingId),
    Unresolved,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SemanticSite {
    pub file: FileId,
    pub generation: u64,
    pub range: TextRange,
}

#[derive(Clone, Debug)]
pub struct SemanticDeclaration {
    pub site: SemanticSite,
    pub symbol: SymbolId,
    pub kind: DeclarationKind,
    pub uncertain: bool,
    pub target: SemanticTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCompletion {
    pub name: String,
    pub target: SemanticTarget,
}

struct SemanticFileContext {
    namespace: String,
    aliases: HashMap<String, String>,
    available_end: usize,
}

#[derive(Clone, Debug)]
pub struct SemanticEntity {
    pub id: SemanticEntityId,
    pub name: String,
    pub declaration: SemanticSite,
    pub signature_owner: SemanticSite,
    pub body_owner: Option<SemanticSite>,
    pub kind: DeclarationKind,
    pub base: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticOccurrence {
    pub site: SemanticSite,
    pub full_range: TextRange,
    pub role: ReferenceRole,
    pub in_type: bool,
    pub uncertain: bool,
    pub synthetic: bool,
    pub target: SemanticTarget,
    pub enclosing: Option<SemanticEntityId>,
}

/// A borrowed view of one indexed call. Every call fact already lives in the
/// view's single occurrence arena, so this wrapper stays pointer-sized and
/// carries no copied range, target or role payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticCall<'a> {
    occurrence: &'a SemanticOccurrence,
}

impl SemanticCall<'_> {
    #[must_use]
    pub const fn site(self) -> SemanticSite {
        self.occurrence.site
    }

    #[must_use]
    pub const fn full_range(self) -> TextRange {
        self.occurrence.full_range
    }

    #[must_use]
    pub const fn caller(self) -> Option<SemanticEntityId> {
        self.occurrence.enclosing
    }

    #[must_use]
    pub const fn target(self) -> SemanticTarget {
        self.occurrence.target
    }

    #[must_use]
    pub const fn role(self) -> ReferenceRole {
        self.occurrence.role
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticIssue {
    MissingImport { file: FileId, range: TextRange },
    ImportCycle { file: FileId },
    DuplicateAlias { file: FileId, range: TextRange },
    InvalidDeclaration { site: SemanticSite },
    AmbiguousName { site: SemanticSite },
    ConflictingOverlays { path: std::path::PathBuf },
}

/// Document identity shared with the workspace entry; the text and syntax stay
/// behind the snapshot `Arc` and are never copied into a captured file.
#[derive(Clone)]
struct CapturedDocument {
    uri: Arc<url::Url>,
    language_id: String,
    snapshot: Arc<DocumentSnapshot>,
}

/// One file captured for cold build; the build-only fields are dropped once the
/// view is published.
struct CapturedFile {
    id: FileId,
    document: Option<CapturedDocument>,
    // Shared with the matching `physical_generations` key: one path per file.
    path: Option<Arc<Path>>,
    physical_uri: Arc<url::Url>,
    generation: u64,
    disk_generation: u64,
    imports: Box<[ImportEdge]>,
    semantic_imports: Box<[ImportEdge]>,
}

impl CapturedFile {
    /// Keep only the facts a published view validates, reports and invalidates on.
    /// `physical_uri`, `semantic_imports` and the document `language_id` belong to
    /// the cold build, which hands the text and syntax to the view's `documents`.
    fn into_receipt(self) -> CapturedFileReceipt {
        CapturedFileReceipt {
            id: self.id,
            document: self.document.map(|document| CapturedIdentity {
                uri: document.uri,
                snapshot: document.snapshot,
            }),
            path: self.path,
            generation: self.generation,
            disk_generation: self.disk_generation,
            imports: self.imports,
        }
    }
}

/// The one captured URI and snapshot `Arc` a published view keeps per input file.
struct CapturedIdentity {
    uri: Arc<url::Url>,
    snapshot: Arc<DocumentSnapshot>,
}

/// Retained per-file capture facts of a built view.
struct CapturedFileReceipt {
    id: FileId,
    document: Option<CapturedIdentity>,
    path: Option<Arc<Path>>,
    generation: u64,
    disk_generation: u64,
    imports: Box<[ImportEdge]>,
}

/// The capture facts a published view keeps after cold build consumed everything else.
struct SemanticCapture {
    serial: u64,
    root: FileId,
    files: Box<[CapturedFileReceipt]>,
    library: Option<std::path::PathBuf>,
    base: Option<SemanticBase>,
    base_file: Option<FileId>,
    physical_generations: HashMap<Arc<Path>, u64>,
    conflicting_paths: std::collections::HashSet<std::path::PathBuf>,
}

/// Captured under the workspace read lock; `build` performs no I/O or parsing.
pub struct SemanticViewInput {
    serial: u64,
    root: FileId,
    files: Vec<CapturedFile>,
    file_indices: HashMap<FileId, usize>,
    library: Option<std::path::PathBuf>,
    base: Option<SemanticBase>,
    base_file: Option<FileId>,
    physical_generations: HashMap<Arc<Path>, u64>,
    conflicting_paths: std::collections::HashSet<std::path::PathBuf>,
}

impl SemanticViewInput {
    #[must_use]
    pub fn build(self) -> SemanticView {
        build::build(self)
    }

    #[must_use]
    pub fn requires_base(&self) -> bool {
        self.files
            .iter()
            .filter_map(|file| file.document.as_ref())
            .any(|document| imports_base(&document.snapshot))
    }
}

impl SemanticCapture {
    #[must_use]
    fn requires_base(&self) -> bool {
        self.files
            .iter()
            .filter_map(|file| file.document.as_ref())
            .any(|document| imports_base(&document.snapshot))
    }
}

/// An unaliased `Base` import decides whether a view owns the compiler prelude.
fn imports_base(document: &DocumentSnapshot) -> bool {
    document.syntax.imports().iter().any(|import| {
        import.valid_syntax && import.alias.is_none() && import.path_text(&document.text) == "Base"
    })
}

/// An immutable entry-qualified Book. All text and syntax facts remain in Arcs.
pub struct SemanticView {
    capture: SemanticCapture,
    documents: Vec<Document>,
    document_indices: CompactMap<FileId, usize>,
    by_uri: CompactMap<Arc<url::Url>, FileId>,
    physical_uris: CompactMap<FileId, Arc<url::Url>>,
    by_physical_uri: CompactMap<Arc<url::Url>, FileId>,
    declaration_indices: CompactMap<(FileId, SymbolId), usize>,
    names: CompactMap<String, SemanticTarget>,
    entities: Vec<SemanticEntity>,
    namespace_indices: CompactMap<String, Box<[usize]>>,
    contexts: CompactMap<FileId, SemanticFileContext>,
    declarations: Vec<SemanticDeclaration>,
    // One target-grouped record arena; file traversal keeps only source-order indices.
    occurrences: Box<[SemanticOccurrence]>,
    occurrence_indices: CompactMap<FileId, Vec<usize>>,
    reference_ranges: CompactMap<SemanticTarget, Range<usize>>,
    // Occurrence-arena indices of calls in their original build order.
    call_indices: Box<[usize]>,
    incoming: CompactMap<SemanticTarget, Vec<usize>>,
    outgoing: CompactMap<SemanticEntityId, Vec<usize>>,
    bindings: CompactMap<SemanticBindingId, SemanticSite>,
    parameter_hints: CompactMap<FileId, Box<[(usize, String)]>>,
    issues: Vec<SemanticIssue>,
}

impl SemanticView {
    #[must_use]
    pub const fn serial(&self) -> u64 {
        self.capture.serial
    }

    #[must_use]
    pub const fn root(&self) -> FileId {
        self.capture.root
    }

    #[must_use]
    pub fn root_uri(&self) -> Option<&url::Url> {
        self.document(self.root()).map(|document| &document.uri)
    }

    #[must_use]
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    #[must_use]
    pub fn document(&self, file: FileId) -> Option<&Document> {
        self.documents.get(*self.document_indices.get(&file)?)
    }

    /// Return only hints inside the requested inclusive offset range.
    /// Signature ownership and argument matching are resolved during cold build.
    #[must_use]
    pub fn parameter_hints(&self, file: FileId, range: TextRange) -> Vec<(usize, String)> {
        let Some(hints) = self.parameter_hints.get(&file) else {
            return Vec::new();
        };
        let start = hints.partition_point(|(offset, _)| *offset < range.start);
        let hints = &hints[start..];
        let end = hints.partition_point(|(offset, _)| *offset <= range.end);
        hints[..end].to_vec()
    }

    #[must_use]
    pub fn file_id(&self, uri: &url::Url) -> Option<FileId> {
        self.by_uri.get(uri).copied()
    }

    #[must_use]
    pub fn physical_uri(&self, file: FileId) -> Option<&url::Url> {
        self.physical_uris.get(&file).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn physical_file_id(&self, uri: &url::Url) -> Option<FileId> {
        self.by_physical_uri.get(uri).copied()
    }

    #[must_use]
    pub fn declarations(&self) -> &[SemanticDeclaration] {
        &self.declarations
    }

    #[must_use]
    pub fn declaration(&self, file: FileId, symbol: SymbolId) -> Option<&SemanticDeclaration> {
        self.declarations
            .get(*self.declaration_indices.get(&(file, symbol))?)
    }

    #[must_use]
    pub fn entities(&self) -> &[SemanticEntity] {
        &self.entities
    }

    #[must_use]
    pub fn entity(&self, id: SemanticEntityId) -> Option<&SemanticEntity> {
        (id.view == self.serial())
            .then(|| self.entities.get(id.index))
            .flatten()
    }

    /// Borrow occurrences in source order, with synthetic records first at equal starts.
    pub fn occurrences(
        &self,
        file: FileId,
    ) -> impl ExactSizeIterator<Item = &SemanticOccurrence> + DoubleEndedIterator + Clone + FusedIterator
    {
        let indices = self
            .occurrence_indices
            .get(&file)
            .map_or(&[][..], Vec::as_slice);
        self.indexed_occurrences(indices)
    }

    /// Borrow the source-ordered suffix beginning at the inclusive start offset.
    pub(crate) fn occurrences_from(
        &self,
        file: FileId,
        start: usize,
    ) -> impl ExactSizeIterator<Item = &SemanticOccurrence> + DoubleEndedIterator + Clone + FusedIterator
    {
        let indices = self
            .occurrence_indices
            .get(&file)
            .map_or(&[][..], Vec::as_slice);
        let begin =
            indices.partition_point(|index| self.occurrences[*index].full_range.start < start);
        self.indexed_occurrences(&indices[begin..])
    }

    fn indexed_occurrences<'a>(
        &'a self,
        indices: &'a [usize],
    ) -> impl ExactSizeIterator<Item = &'a SemanticOccurrence>
    + DoubleEndedIterator
    + Clone
    + FusedIterator
    + 'a {
        indices.iter().map(move |index| &self.occurrences[*index])
    }

    #[must_use]
    pub fn resolve_at(&self, file: FileId, offset: usize) -> Option<&SemanticOccurrence> {
        let indices = self.occurrence_indices.get(&file)?;
        let index = indices
            .partition_point(|index| self.occurrences[*index].full_range.start <= offset)
            .checked_sub(1)?;
        let entry = &self.occurrences[indices[index]];
        (entry.full_range.contains(offset) || entry.site.range.end == offset).then_some(entry)
    }

    #[must_use]
    pub fn target_site(&self, target: SemanticTarget) -> Option<SemanticSite> {
        match target {
            SemanticTarget::Global(id) => self.entity(id).map(|entity| entity.declaration),
            SemanticTarget::Local(id) => self.bindings.get(&id).copied(),
            SemanticTarget::Unresolved | SemanticTarget::Ambiguous => None,
        }
    }

    /// Borrow resolved references in their original build order.
    #[must_use]
    pub fn references(&self, target: SemanticTarget) -> &[SemanticOccurrence] {
        self.reference_ranges
            .get(&target)
            .map_or(&[], |range| &self.occurrences[range.clone()])
    }

    /// Borrow calls in their original build order without copying call facts.
    #[must_use]
    pub fn calls(
        &self,
    ) -> impl ExactSizeIterator<Item = SemanticCall<'_>> + DoubleEndedIterator + Clone + FusedIterator
    {
        self.call_indices.iter().map(|index| SemanticCall {
            occurrence: &self.occurrences[*index],
        })
    }

    pub fn calls_to(&self, target: SemanticTarget) -> impl Iterator<Item = SemanticCall<'_>> {
        self.incoming
            .get(&target)
            .into_iter()
            .flat_map(|indices| indices.iter())
            .map(|index| SemanticCall {
                occurrence: &self.occurrences[*index],
            })
    }

    pub fn calls_from(&self, caller: SemanticEntityId) -> impl Iterator<Item = SemanticCall<'_>> {
        self.outgoing
            .get(&caller)
            .into_iter()
            .flat_map(|indices| indices.iter())
            .map(|index| SemanticCall {
                occurrence: &self.occurrences[*index],
            })
    }

    #[must_use]
    pub fn issues(&self) -> &[SemanticIssue] {
        &self.issues
    }

    /// Refuse an edit if a shared physical occurrence has another interpretation.
    #[must_use]
    pub fn rename_conflicts(&self, target: SemanticTarget, other: &Self) -> bool {
        let expected = self.target_site(target);
        self.references(target).iter().any(|occurrence| {
            let Some(uri) = self.physical_uri(occurrence.site.file) else {
                return true;
            };
            let Some(file) = other.physical_file_id(uri) else {
                return false;
            };
            let Some(found) = other.resolve_at(file, occurrence.site.range.start) else {
                return true;
            };
            let actual = other.target_site(found.target);
            match (expected, actual) {
                (Some(left), Some(right)) => {
                    let left_uri = self.physical_uri(left.file);
                    let right_uri = other.physical_uri(right.file);
                    left_uri != right_uri
                        || left.range != right.range
                        || left.generation != right.generation
                }
                _ => true,
            }
        })
    }

    /// Stable JSON shape for context-qualified hierarchy handles and metadata.
    #[must_use]
    pub fn provenance(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "view": self.serial(),
            "rootUri": self.root_uri().map(url::Url::as_str),
            "inputs": self.capture.files.iter().map(|file| serde_json::json!({
                "file": file.id.0, "generation": file.generation,
                "uri": file.document.as_ref().map(|document| document.uri.as_str()),
                "physicalUri": self.physical_uri(file.id).map(url::Url::as_str),
                "revision": file.document.as_ref().map(|document| document.snapshot.revision.0),
            })).collect::<Vec<_>>(),
            "base": self.capture.base.as_ref().map(|base| serde_json::json!({
                "uri": base.document.uri.as_str(), "generation": base.generation,
                "provenance": &*base.provenance,
            })),
        })
    }
}

impl WorkspaceDb {
    #[must_use]
    pub fn capture_semantic_view(
        &self,
        root: FileId,
        base: Option<SemanticBase>,
    ) -> Option<SemanticViewInput> {
        if !self.is_needed(root) || self.entries.get(root.0)?.snapshot().is_none() {
            return None;
        }
        let top = self.entries[root.0]
            .physical_path
            .as_deref()
            .or(self.entries[root.0].path.as_deref())
            .and_then(Path::parent);
        let mut files = Vec::new();
        let mut pending = vec![root];
        let mut seen = std::collections::HashSet::new();
        let mut physical = PhysicalCapture::default();
        if let Some(path) = &self.entries[root.0].physical_path {
            physical.selected.insert(path.as_path(), root);
        }
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let entry = self.entries.get(id.0)?;
            if !self.is_needed(id) {
                return None;
            }
            let source_is_hub = id != root
                && entry
                    .physical_path
                    .as_deref()
                    .zip(self.semantic_library_path.as_deref())
                    .and_then(|(path, library)| path.strip_prefix(library).ok())
                    .and_then(Path::to_str)
                    .is_some_and(super::is_hub_import_path);
            let physical_path = if let Some(path) = entry.physical_path.as_deref() {
                let shared = physical.shared_path(path);
                physical.generations.insert(
                    Arc::clone(&shared),
                    self.physical_generations.get(path).copied().unwrap_or(0),
                );
                if *physical
                    .overlays
                    .entry(path)
                    .or_insert_with(|| self.physical_overlay_conflict(path))
                {
                    physical.conflicts.insert(shared.to_path_buf());
                }
                Some(shared)
            } else {
                None
            };
            let semantic_imports =
                self.capture_semantic_imports(&entry.imports, top, source_is_hub, &mut physical);
            pending.extend(semantic_imports.iter().rev().map(|edge| edge.target));
            let path = physical_path
                .or_else(|| entry.path.as_deref().map(|path| physical.shared_path(path)));
            files.push(CapturedFile {
                id,
                document: entry.snapshot().map(|snapshot| CapturedDocument {
                    uri: Arc::clone(&entry.uri),
                    language_id: entry.language_id.clone(),
                    snapshot: Arc::clone(snapshot),
                }),
                path,
                physical_uri: entry.physical_uri.clone(),
                generation: entry.semantic_generation,
                disk_generation: entry.disk_generation,
                imports: entry.imports.clone(),
                semantic_imports,
            });
        }
        // The reserved Base identity has no disk payload or liveness ownership.
        let base_file = base.as_ref().map(|base| {
            self.file_id_by_uri(&base.document.uri)
                .unwrap_or(FileId::BASE)
        });
        let file_indices = files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.id, index))
            .collect();
        let mut input = SemanticViewInput {
            serial: NEXT_VIEW.fetch_add(1, Ordering::Relaxed),
            root,
            files,
            file_indices,
            library: self.semantic_library_path.clone(),
            base,
            base_file,
            physical_generations: physical.generations,
            conflicting_paths: physical.conflicts,
        };
        if !input.requires_base() {
            input.base = None;
            input.base_file = None;
        }
        Some(input)
    }

    fn capture_semantic_imports<'a>(
        &'a self,
        imports: &[ImportEdge],
        top: Option<&Path>,
        source_is_hub: bool,
        physical: &mut PhysicalCapture<'a>,
    ) -> Box<[ImportEdge]> {
        imports
            .iter()
            .filter_map(|edge| {
                let target = self.entries[edge.target.0].physical_path.as_ref().map_or(
                    edge.target,
                    |path| {
                        *physical.selected.entry(path.as_path()).or_insert_with(|| {
                            let (selected, conflict) = self.semantic_source_target(edge.target);
                            physical.overlays.insert(path.as_path(), conflict);
                            selected
                        })
                    },
                );
                let path = self.entries[target.0]
                    .physical_path
                    .as_deref()
                    .or(self.entries[target.0].path.as_deref())?;
                let shared = physical.shared_path(path);
                physical.generations.insert(
                    shared,
                    self.physical_generations.get(path).copied().unwrap_or(0),
                );
                if !super::valid_import_namespace_path(
                    path,
                    top?,
                    self.semantic_library_path.as_deref(),
                    source_is_hub,
                ) {
                    return None;
                }
                Some(ImportEdge {
                    path: edge.path,
                    target,
                })
            })
            .collect()
    }

    #[must_use]
    pub fn validate_semantic_view(&self, view: &SemanticView, base: Option<&SemanticBase>) -> bool {
        if !self.is_needed(view.root()) {
            return false;
        }
        let base_matches = match (&view.capture.base, base) {
            (None, None) => true,
            (None, Some(_)) => !view.capture.requires_base(),
            (Some(captured), Some(current)) => {
                captured.generation == current.generation
                    && captured.provenance == current.provenance
                    && captured.document.uri == current.document.uri
                    && Arc::ptr_eq(&captured.document.snapshot, &current.document.snapshot)
            }
            _ => false,
        };
        let library = &self.semantic_library_path;
        let library_matches = *library == view.capture.library
            || !view
                .capture
                .library_affected(view.capture.library.as_deref(), library.as_deref());
        base_matches
            && library_matches
            && view
                .capture
                .physical_generations
                .iter()
                .all(|(path, generation)| {
                    self.physical_generations.get(&**path).copied().unwrap_or(0) == *generation
                })
            && view.capture.files.iter().all(|file| {
                let Some(entry) = self.entries.get(file.id.0) else {
                    return false;
                };
                self.is_needed(file.id)
                    && entry.semantic_generation == file.generation
                    && entry.disk_generation == file.disk_generation
                    && entry.imports == file.imports
                    && match (entry.snapshot(), file.document.as_ref()) {
                        (Some(current), Some(captured)) => Arc::ptr_eq(current, &captured.snapshot),
                        (None, None) => true,
                        _ => false,
                    }
            })
    }

    pub fn publish_semantic_view(
        &mut self,
        view: Arc<SemanticView>,
        base: Option<&SemanticBase>,
    ) -> bool {
        if !self.validate_semantic_view(&view, base) {
            return false;
        }
        self.remove_semantic_view(view.root());
        for file in &view.capture.files {
            self.semantic_dependents
                .entry(file.id)
                .or_default()
                .insert(view.root());
        }
        if let Some(file) = view.capture.base_file {
            self.semantic_dependents
                .entry(file)
                .or_default()
                .insert(view.root());
        }
        for path in view.capture.physical_generations.keys() {
            self.semantic_physical_dependents
                .entry(path.to_path_buf())
                .or_default()
                .insert(view.root());
        }
        self.dirty_semantic_roots.remove(&view.root());
        self.semantic_views.insert(view.root(), view);
        true
    }

    #[must_use]
    pub fn semantic_view(&self, root: FileId) -> Option<Arc<SemanticView>> {
        self.semantic_views.get(&root).cloned()
    }

    #[must_use]
    pub fn semantic_roots(&self) -> Vec<FileId> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, _)| FileId(index))
            .filter(|root| self.is_semantic_root(*root))
            .collect()
    }

    #[must_use]
    pub fn missing_semantic_roots(&self) -> Vec<FileId> {
        let mut roots: Vec<_> = self.dirty_semantic_roots.iter().copied().collect();
        roots.sort_unstable();
        roots
    }

    #[must_use]
    pub(crate) fn missing_open_semantic_roots(&self) -> Vec<FileId> {
        let mut roots: Vec<_> = self
            .dirty_semantic_roots
            .iter()
            .copied()
            .filter(|root| self.is_file_open(*root))
            .collect();
        roots.sort_unstable();
        roots
    }

    #[must_use]
    pub fn semantic_views_ready_for(&self, root: Option<FileId>) -> bool {
        root.map_or_else(
            || self.dirty_semantic_roots.is_empty(),
            |root| !self.is_semantic_root(root) || self.semantic_views.contains_key(&root),
        )
    }

    #[must_use]
    pub(crate) fn semantic_entry_roots_for_file(&self, file: FileId) -> Vec<FileId> {
        let mut pending = if file == FileId::BASE {
            self.semantic_base_importers.iter().copied().collect()
        } else {
            vec![file]
        };
        let mut seen = std::collections::HashSet::new();
        let mut physical = std::collections::HashSet::new();
        let mut roots = Vec::new();
        while let Some(file) = pending.pop() {
            if !seen.insert(file) {
                continue;
            }
            let Some(entry) = self.entries.get(file.0) else {
                continue;
            };
            if self.is_semantic_root(file) {
                roots.push(file);
            }
            if let Some(path) = entry.physical_path.as_ref()
                && physical.insert(path)
                && let Some(aliases) = self.physical_files.get(path)
            {
                pending.extend(aliases.iter().copied());
            }
            pending.extend(
                entry
                    .reverse_imports
                    .iter()
                    .copied()
                    .filter(|dependent| self.is_needed(*dependent)),
            );
        }
        roots.sort_unstable();
        roots
    }

    #[must_use]
    pub fn views_for_file(&self, file: FileId) -> Vec<Arc<SemanticView>> {
        let physical = self
            .entries
            .get(file.0)
            .and_then(|entry| entry.physical_path.as_ref())
            .and_then(|path| self.semantic_physical_dependents.get(path));
        let mut roots: Vec<_> = self
            .semantic_dependents
            .get(&file)
            .into_iter()
            .chain(physical)
            .flat_map(|roots| roots.iter().copied())
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots
            .into_iter()
            .filter_map(|root| self.semantic_views.get(&root).cloned())
            .collect()
    }

    pub fn invalidate_semantic_base(&mut self) {
        let affected: Vec<_> = self
            .semantic_views
            .iter()
            .filter_map(|(root, view)| view.capture.requires_base().then_some(*root))
            .collect();
        for root in affected {
            self.remove_semantic_view(root);
        }
    }

    fn is_semantic_root(&self, root: FileId) -> bool {
        self.is_needed(root)
            && self
                .entries
                .get(root.0)
                .is_some_and(|entry| entry.snapshot().is_some())
    }

    pub(super) fn refresh_semantic_root(&mut self, root: FileId) {
        if self.is_semantic_root(root) {
            self.dirty_semantic_roots.insert(root);
        } else {
            self.dirty_semantic_roots.remove(&root);
        }
    }

    pub(super) fn remove_semantic_view(&mut self, root: FileId) {
        self.refresh_semantic_root(root);
        if let Some(view) = self.semantic_views.remove(&root) {
            for file in &view.capture.files {
                self.remove_semantic_dependent(file.id, root);
            }
            if let Some(file) = view.capture.base_file {
                self.remove_semantic_dependent(file, root);
            }
            for path in view.capture.physical_generations.keys() {
                if self
                    .semantic_physical_dependents
                    .get_mut(&**path)
                    .is_some_and(|roots| {
                        roots.remove(&root);
                        roots.is_empty()
                    })
                {
                    self.semantic_physical_dependents.remove(&**path);
                }
            }
        }
    }

    fn remove_semantic_dependent(&mut self, file: FileId, root: FileId) {
        if self
            .semantic_dependents
            .get_mut(&file)
            .is_some_and(|roots| {
                roots.remove(&root);
                roots.is_empty()
            })
        {
            self.semantic_dependents.remove(&file);
        }
    }

    pub(super) fn invalidate_semantic_file(&mut self, file: FileId) {
        self.entries[file.0].editor_import_paths = None;
        self.entries[file.0].semantic_generation =
            self.entries[file.0].semantic_generation.wrapping_add(1);
        if let Some(roots) = self.semantic_dependents.remove(&file) {
            for root in roots {
                self.remove_semantic_view(root);
            }
        }
        self.refresh_semantic_root(file);
        self.invalidate_physical_file(file);
    }
}
