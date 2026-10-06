use std::{collections::HashMap, ops::Range};
use std::{collections::HashSet, sync::Arc};

use crate::analysis::ReferenceKind;
use crate::analysis::{DocumentSnapshot, SymbolId};

use crate::analysis::{NameId, SymbolKind, TextRange, TokenId};

use url::Url;

use super::FileEntry;
use super::{Document, FileId, WorkspaceDb};

/// A snapshot-local symbol with stable file identity and a snapshot epoch.
/// Epochs do not use client versions: disk snapshots can all be UNVERSIONED.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlobalSymbolId {
    file: FileId,
    epoch: u64,
    local: SymbolId,
}

impl GlobalSymbolId {
    #[must_use]
    pub const fn file(self) -> FileId {
        self.file
    }

    #[must_use]
    pub const fn local_symbol(self) -> SymbolId {
        self.local
    }
}

#[derive(Clone)]
pub struct WorkspaceSymbol {
    pub id: GlobalSymbolId,
    pub document: Document,
}

#[derive(Clone)]
pub struct WorkspaceOccurrence {
    pub document: Document,
    pub range: TextRange,
    pub kind: ReferenceKind,
}

impl WorkspaceOccurrence {
    /// Canonical library reference order: URI, then byte range.
    pub fn sort(occurrences: &mut [Self]) {
        occurrences.sort_unstable_by(|left, right| {
            left.document
                .uri
                .as_str()
                .cmp(right.document.uri.as_str())
                .then_with(|| left.range.start.cmp(&right.range.start))
                .then_with(|| left.range.end.cmp(&right.range.end))
        });
    }

    /// Remove adjacent equal URI/range rows after canonical sorting.
    /// Removed rows are destroyed here; retained rows remain owned by the caller.
    pub fn dedup(occurrences: &mut Vec<Self>) {
        occurrences.dedup_by(|left, right| {
            left.document.uri == right.document.uri && left.range == right.range
        });
    }
}

/// Incoming groups describe callers; outgoing groups describe callees.
/// All ranges belong to `source`, never implicitly to `symbol.document`.
#[derive(Clone)]
pub struct WorkspaceCallGroup {
    pub symbol: WorkspaceSymbol,
    pub source: Document,
    pub ranges: Vec<TextRange>,
}

/// Cumulative update work and current index sizes for cold/update benchmarks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceIndexStats {
    pub files_rebuilt: u64,
    pub occurrences: usize,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum TemplateModule {
    Import(TextRange),
    CompilerBase,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TemplateTarget {
    module: TemplateModule,
    name: NameId,
}

#[derive(Clone, Copy)]
struct ReferenceOrdinal(usize);

/// A borrowed external target span in one source's immutable contribution.
/// A source may have multiple groups when import spellings share a target.
pub struct ExternalReferenceGroup<'a> {
    source: FileId,
    occurrences: &'a [ReferenceOrdinal],
    snapshot: &'a DocumentSnapshot,
}

impl ExternalReferenceGroup<'_> {
    #[must_use]
    pub const fn source(&self) -> FileId {
        self.source
    }

    #[must_use]
    pub const fn occurrence_count(&self) -> usize {
        self.occurrences.len()
    }

    /// Resolve cold-prepared ordinals against their own immutable snapshot.
    /// This borrows syntax rows without cloning documents or URIs.
    #[must_use]
    pub fn occurrences(&self) -> impl ExactSizeIterator<Item = &crate::analysis::Reference> {
        self.occurrences
            .iter()
            .map(|ordinal| &self.snapshot.syntax.reference_entries()[ordinal.0])
    }
}

struct PreparedTarget {
    target: TemplateTarget,

    occurrences: Range<usize>,
}

/// Cold per-file semantic delta, prepared without a workspace lock.
///
/// Local references remain in immutable syntax spans, preserving duplicate
/// declarations and lexical bindings. Imported members remain symbolic until
/// commit. Target spans select flat reference-ordinal columns;
/// occurrence ranges and kinds are read from the immutable syntax snapshot.
/// Commit binds distinct groups and moves their arrays; it never scans tokens,
/// source, raw calls, or importer files.
/// External buckets use stable file/name identities, so changing an imported
/// snapshot does not require reconstructing any importing file's contribution.
pub struct PreparedSemanticSnapshot {
    snapshot: Arc<DocumentSnapshot>,

    targets: Vec<PreparedTarget>,

    occurrences: Vec<ReferenceOrdinal>,

    imports_prelude: bool,
}

impl PreparedSemanticSnapshot {
    #[must_use]
    pub fn snapshot(&self) -> &Arc<DocumentSnapshot> {
        &self.snapshot
    }
}

/// Build the per-file delta on the staging worker alongside snapshot/import work.
#[must_use]
pub fn prepare_semantic_snapshot(snapshot: Arc<DocumentSnapshot>) -> PreparedSemanticSnapshot {
    let imports_prelude = snapshot
        .syntax
        .imports()
        .iter()
        .any(|import| import.path_text(&snapshot.text) == "Base");

    let mut targets = Vec::new();

    let mut target_indices = HashMap::new();

    let mut occurrences = Vec::new();

    prepare_occurrences(
        &snapshot,
        &mut targets,
        &mut target_indices,
        &mut occurrences,
    );

    PreparedSemanticSnapshot {
        snapshot,

        targets,

        occurrences,

        imports_prelude,
    }
}

fn prepared_target(
    targets: &mut Vec<PreparedTarget>,
    indices: &mut HashMap<TemplateTarget, usize>,
    target: TemplateTarget,
) -> usize {
    // Imported occurrences often repeat one member. Avoid hashing the same
    // immutable template again while preserving the map for interleaved targets.
    if let Some(previous) = targets.last()
        && previous.target == target
    {
        return targets.len() - 1;
    }
    if targets.is_empty() {
        targets.push(PreparedTarget {
            target,

            occurrences: 0..0,
        });
        return 0;
    }
    if indices.is_empty() {
        // A single target needs no scratch hash allocation. Populate the map
        // only when a second distinct template requires noncontiguous lookup.
        indices.insert(targets[0].target, 0);
    }
    *indices.entry(target).or_insert_with(|| {
        let index = targets.len();
        targets.push(PreparedTarget {
            target,

            occurrences: 0..0,
        });
        index
    })
}

fn prepare_occurrences(
    snapshot: &DocumentSnapshot,
    targets: &mut Vec<PreparedTarget>,
    indices: &mut HashMap<TemplateTarget, usize>,

    occurrences: &mut Vec<ReferenceOrdinal>,
) {
    let mut contiguous = true;
    for (ordinal, reference) in snapshot.syntax.reference_entries().iter().enumerate() {
        // Own-file references already occupy compact per-symbol syntax spans.
        // Only imported occurrences need an additional workspace contribution.
        if reference.resolved.is_some() {
            continue;
        }
        let target = occurrence_template_target(snapshot, reference);
        if let Some(target) = target {
            let index = prepared_target(targets, indices, target);
            let span = &mut targets[index].occurrences;
            if span.start == span.end {
                span.start = occurrences.len();
                span.end = span.start;
            } else if span.end != occurrences.len() {
                contiguous = false;
            }
            span.end += 1;
            occurrences.push(ReferenceOrdinal(ordinal));
        }
    }
    if !contiguous {
        // Most files already have contiguous target spans. Only interleaving
        // needs a temporary cached-key column; each template is looked up once.
        occurrences.sort_by_cached_key(|&ordinal| {
            let index = snapshot
                .syntax
                .reference_entries()
                .get(ordinal.0)
                .and_then(|reference| occurrence_template_target(snapshot, reference))
                .and_then(|target| indices.get(&target))
                .copied();
            // Every selected ordinal came from a target in this immutable
            // snapshot. A missing key is an internal invariant violation, not
            // an unresolved occurrence to discard or bind to another target.
            assert!(
                index.is_some(),
                "prepared occurrence must retain its target"
            );
            index
        });
        let mut start = 0;
        for target in targets {
            let count = target.occurrences.len();
            target.occurrences = start..start + count;
            start += count;
        }
    }
}

fn occurrence_template_target(
    snapshot: &DocumentSnapshot,
    reference: &crate::analysis::Reference,
) -> Option<TemplateTarget> {
    let qualifier = reference.qualifier?;
    if reference
        .qualifier_token
        .is_some_and(|token| snapshot.syntax.symbol_for_token(token).is_some())
    {
        return None;
    }
    let alias = snapshot.syntax.name_text(&snapshot.text, qualifier);
    template_module(snapshot, alias, false).map(|module| TemplateTarget {
        module,
        name: reference.name,
    })
}

fn template_module(
    snapshot: &DocumentSnapshot,
    alias: &str,
    base_fallback: bool,
) -> Option<TemplateModule> {
    if let Some(import) = snapshot
        .syntax
        .imports()
        .iter()
        .find(|import| import.alias_text(&snapshot.text) == Some(alias))
    {
        return Some(if import.path_text(&snapshot.text) == "Base" {
            TemplateModule::CompilerBase
        } else {
            TemplateModule::Import(import.path)
        });
    }
    (base_fallback && alias == "Base").then_some(TemplateModule::CompilerBase)
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct MemberId(usize);

#[derive(Default)]
struct MemberNames {
    by_name: HashMap<Arc<str>, MemberId>,
}

impl MemberNames {
    fn intern(&mut self, name: &str) -> MemberId {
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = MemberId(self.by_name.len());
        let name: Arc<str> = Arc::from(name);

        self.by_name.insert(name, id);
        id
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ModuleKey {
    File(FileId),
    CompilerBase,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TargetKey {
    module: ModuleKey,
    member: MemberId,
}

struct BoundTarget {
    key: Option<TargetKey>,

    occurrences: Range<usize>,
}

struct Contribution {
    targets: Vec<BoundTarget>,

    occurrences: Vec<ReferenceOrdinal>,

    occurrence_count: usize,
}

/// Most sources have one spelling per bound target. Additional ordinals are
/// needed only when distinct import spellings bind to the same file/member.
struct TargetIndices {
    first: usize,
    additional: Vec<usize>,
}

impl TargetIndices {
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.first).chain(self.additional.iter().copied())
    }
}

type ExternalContributions = HashMap<FileId, TargetIndices>;

#[derive(Default)]
pub(super) struct SemanticIndex {
    names: MemberNames,

    // File IDs are dense workspace ordinals. Only this source column is dense;
    // target buckets remain sparse and contain matching sources alone.
    contributions: Vec<Option<Contribution>>,

    external: HashMap<TargetKey, ExternalContributions>,
    prelude_files: HashSet<FileId>,
    pub(super) compiler_base: Option<FileId>,
    stats: WorkspaceIndexStats,
}

impl SemanticIndex {
    fn contribution(&self, source: FileId) -> Option<&Contribution> {
        self.contributions.get(source.0)?.as_ref()
    }

    fn remove(&mut self, source: FileId) {
        self.prelude_files.remove(&source);

        {
            let Some(old) = self.contributions.get_mut(source.0).and_then(Option::take) else {
                return;
            };

            {
                self.stats.occurrences -= old.occurrence_count;
            }

            // Bound keys remove this source without scanning importing files.
            for target in old.targets.into_iter().filter_map(|target| target.key) {
                if let Some(sources) = self.external.get_mut(&target) {
                    sources.remove(&source);
                    if sources.is_empty() {
                        self.external.remove(&target);
                    }
                }
            }
        }
    }
}

impl WorkspaceDb {
    /// Detach compiler Base resolution without retiring its readable source or
    /// provenance. Symbolic import targets bind again when a new Base is loaded.
    pub(crate) fn clear_compiler_base(&mut self) {
        self.semantic.compiler_base = None;
    }

    #[must_use]
    pub fn semantic_index_stats(&self) -> WorkspaceIndexStats {
        self.semantic.stats
    }

    /// Cold-prepared metadata for the current supported effective snapshot.
    #[must_use]
    pub(crate) fn imports_prelude(&self, uri: &Url) -> bool {
        self.file_id_by_uri(uri)
            .is_some_and(|file| self.semantic.prelude_files.contains(&file))
    }

    /// Reachability changes do not require rebuilding prelude metadata.
    #[must_use]
    pub(crate) fn workspace_imports_prelude(&self) -> bool {
        self.semantic
            .prelude_files
            .iter()
            .any(|file| self.reachable.contains(*file))
    }

    /// Returns None for tombstones, stale local IDs, or an exhausted epoch counter.
    #[must_use]
    pub fn global_symbol_id(&self, uri: &Url, local: SymbolId) -> Option<GlobalSymbolId> {
        self.symbol_identity(self.file_id_by_uri(uri)?, local)
    }

    #[must_use]
    pub fn symbol_by_name(&self, uri: &Url, name: &str) -> Option<WorkspaceSymbol> {
        let file = self.file_id_by_uri(uri)?;
        self.symbol_by_id(self.named_identity(file, name)?)
    }

    /// Stale global IDs are rejected, never reinterpreted in a newer snapshot.
    #[must_use]
    pub fn symbol_by_id(&self, id: GlobalSymbolId) -> Option<WorkspaceSymbol> {
        if self.symbol_identity(id.file, id.local) != Some(id) {
            return None;
        }
        Some(WorkspaceSymbol {
            id,
            document: self.entries[id.file.0].document()?,
        })
    }

    /// Look up current, reachable external reference spans without materializing
    /// documents or occurrences. Local references are not included.
    pub fn external_reference_groups(
        &self,
        target: GlobalSymbolId,
    ) -> impl Iterator<Item = ExternalReferenceGroup<'_>> + '_ {
        (self.symbol_identity(target.file, target.local) == Some(target))
            .then_some(target)
            .into_iter()
            .flat_map(move |target| self.current_external_reference_groups(target))
    }

    /// The caller has already checked the target's current snapshot epoch.
    fn current_external_reference_groups(
        &self,
        target: GlobalSymbolId,
    ) -> impl Iterator<Item = ExternalReferenceGroup<'_>> + '_ {
        self.query_keys(target)
            .filter_map(move |key| self.semantic.external.get(&key))
            .flat_map(|sources| sources.iter())
            .filter_map(move |(&source, indices)| {
                if !self.reachable.contains(source) {
                    return None;
                }
                let contribution = self.semantic.contribution(source)?;
                let snapshot = self.entries[source.0].snapshot()?;
                Some((source, indices, contribution, snapshot))
            })
            .flat_map(|(source, indices, contribution, snapshot)| {
                indices.iter().map(move |index| ExternalReferenceGroup {
                    source,
                    snapshot,
                    occurrences: &contribution.occurrences
                        [contribution.targets[index].occurrences.clone()],
                })
            })
    }

    #[must_use]
    pub fn references(
        &self,
        target: GlobalSymbolId,
        include_declaration: bool,
    ) -> Vec<WorkspaceOccurrence> {
        let mut result = self.references_unsorted(target, include_declaration);
        WorkspaceOccurrence::sort(&mut result);
        WorkspaceOccurrence::dedup(&mut result);
        result
    }

    /// Materialize local and external occurrences without sorting or deduping.
    /// Traversal order is unspecified; use `references` for canonical results.
    #[must_use]
    pub fn references_unsorted(
        &self,
        target: GlobalSymbolId,
        include_declaration: bool,
    ) -> Vec<WorkspaceOccurrence> {
        if self.symbol_identity(target.file, target.local) != Some(target) {
            return Vec::new();
        }
        let mut result = Vec::new();
        let local = &self.entries[target.file.0];
        if self.reachable.contains(target.file)
            && local.semantic_active
            && let Some(snapshot) = local.snapshot()
        {
            result.extend(
                snapshot
                    .syntax
                    .references(target.local)
                    .filter(|reference| {
                        include_declaration || reference.kind != ReferenceKind::Declaration
                    })
                    .map(|reference| WorkspaceOccurrence {
                        document: Document::with_snapshot(
                            local.uri.clone(),
                            local.language_id.clone(),
                            Arc::clone(snapshot),
                        ),
                        range: reference.range,
                        kind: reference.kind,
                    }),
            );
        }
        let mut source_snapshot: Option<(FileId, &FileEntry, &Arc<DocumentSnapshot>)> = None;
        for group in self.current_external_reference_groups(target) {
            if source_snapshot
                .as_ref()
                .is_none_or(|(source, _, _)| *source != group.source)
            {
                let entry = &self.entries[group.source.0];
                source_snapshot = entry
                    .snapshot()
                    .map(|snapshot| (group.source, entry, snapshot));
            }
            let Some((_, entry, snapshot)) = &source_snapshot else {
                continue;
            };
            // Cold preparation selects unresolved rows: declarations always
            // resolve locally. These ordinals index this same immutable snapshot,
            // so the mapped slice retains its exact length without a kind filter.
            result.extend(group.occurrences().map(|occurrence| WorkspaceOccurrence {
                document: Document::with_snapshot(
                    entry.uri.clone(),
                    entry.language_id.clone(),
                    Arc::clone(snapshot),
                ),
                range: occurrence.range,
                kind: occurrence.kind,
            }));
        }
        result
    }

    #[must_use]
    pub fn rename_occurrences(&self, target: GlobalSymbolId) -> Vec<WorkspaceOccurrence> {
        self.references(target, true)
    }

    pub(super) fn install_semantics(
        &mut self,
        changed: FileId,
        prepared: Option<PreparedSemanticSnapshot>,
        imports_changed: bool,
    ) {
        let entry = &mut self.entries[changed.0];
        // Effective snapshots and language may already have changed. Retire
        // cached local totals from the old indexed snapshot and activation.

        if entry.semantic_active
            && entry.semantic_epoch.is_some()
            && let Some(old) = &entry.semantic_snapshot
        {
            {
                self.semantic.stats.occurrences -= old.syntax.local_reference_occurrences();
            }
        }
        self.semantic.remove(changed);
        let snapshot = entry.snapshot().cloned();
        if !same_snapshot(entry.semantic_snapshot.as_ref(), snapshot.as_ref()) {
            entry.semantic_epoch = entry.semantic_epoch.and_then(|epoch| epoch.checked_add(1));
            entry.semantic_snapshot = snapshot;
        }
        entry.semantic_active =
            prepared.is_some() && (entry.language_id == "bend" || entry.language_id == "bend2");

        if entry.semantic_active
            && entry.semantic_epoch.is_some()
            && let Some(snapshot) = &entry.semantic_snapshot
        {
            {
                self.semantic.stats.occurrences += snapshot.syntax.local_reference_occurrences();
            }
        }
        if let Some(prepared) = prepared
            && self.entries[changed.0].semantic_active
        {
            self.install_contribution(changed, prepared);
        }
        if self.reachability_dirty {
            self.recompute_reachable();
        } else if (self.entries[changed.0].open_snapshot.is_some()
            && !self.reachable.contains(changed))
            || (imports_changed && self.reachable.contains(changed))
        {
            self.extend_reachable(changed);
        }
    }

    fn install_contribution(&mut self, source: FileId, prepared: PreparedSemanticSnapshot) {
        self.semantic.stats.files_rebuilt = self.semantic.stats.files_rebuilt.saturating_add(1);
        if prepared.imports_prelude {
            self.semantic.prelude_files.insert(source);
        }
        // Local-only snapshots retain activation and prelude metadata, but no
        // external contribution or dense source-column allocation.

        self.install_external_contribution(source, prepared);
    }

    fn install_external_contribution(
        &mut self,
        source: FileId,
        prepared: PreparedSemanticSnapshot,
    ) {
        if prepared.targets.is_empty() {
            return;
        }
        // Different import spellings can bind to the same key. Each sparse
        // source bucket keeps their canonical group ordinals without copying.

        let mut occurrence_count = 0;

        // Keep bound keys with their spans, not staging-only templates plus a
        // separately allocated key column. Consume the cold rows into warm rows.
        let targets = prepared
            .targets
            .into_iter()
            .enumerate()
            .map(|(index, target)| {
                let key = self.bind_target(source, &prepared.snapshot, target.target);
                if let Some(key) = key {
                    let mut selected = false;

                    {
                        occurrence_count += target.occurrences.len();
                        selected |= !target.occurrences.is_empty();
                    }

                    if selected {
                        self.semantic
                            .external
                            .entry(key)
                            .or_default()
                            .entry(source)
                            .and_modify(|indices| indices.additional.push(index))
                            .or_insert_with(|| TargetIndices {
                                first: index,
                                additional: Vec::new(),
                            });
                    }
                }
                BoundTarget {
                    key,

                    occurrences: target.occurrences,
                }
            })
            .collect();

        {
            self.semantic.stats.occurrences += occurrence_count;
        }

        if self.semantic.contributions.len() <= source.0 {
            self.semantic
                .contributions
                .resize_with(source.0 + 1, || None);
        }
        self.semantic.contributions[source.0] = Some(Contribution {
            targets,

            occurrences: prepared.occurrences,

            occurrence_count,
        });
    }

    fn bind_target(
        &mut self,
        source: FileId,
        snapshot: &DocumentSnapshot,
        target: TemplateTarget,
    ) -> Option<TargetKey> {
        let module = match target.module {
            TemplateModule::CompilerBase => ModuleKey::CompilerBase,
            TemplateModule::Import(path) => {
                ModuleKey::File(*self.entries[source.0].import_by_range.get(&path)?)
            }
        };
        let member = self
            .semantic
            .names
            .intern(snapshot.syntax.name_text(&snapshot.text, target.name));
        Some(TargetKey { module, member })
    }

    fn query_keys(&self, target: GlobalSymbolId) -> impl Iterator<Item = TargetKey> {
        let member = self.entries[target.file.0].snapshot().and_then(|snapshot| {
            let symbol = snapshot.syntax.symbol_by_id(target.local)?;
            // Only the exported declaration selected by symbol_by_name can be
            // targeted by imports. Duplicate local declarations stay separate.
            if snapshot.syntax.symbol_by_name(symbol.name)?.id != target.local {
                return None;
            }
            let name = snapshot.syntax.name_text(&snapshot.text, symbol.name);
            self.semantic.names.by_name.get(name).copied()
        });
        let external = member.map(|member| TargetKey {
            module: ModuleKey::File(target.file),
            member,
        });
        let base = member
            .filter(|_| self.semantic.compiler_base == Some(target.file))
            .map(|member| TargetKey {
                module: ModuleKey::CompilerBase,
                member,
            });
        external.into_iter().chain(base)
    }

    pub(super) fn extend_reachable(&mut self, root: FileId) {
        self.reachable.insert(root);
        // The changed root can already be reachable. Visit its new edges, but
        // known descendants have already contributed their reachable closure.
        let mut pending: Vec<_> = self.entries[root.0]
            .imports
            .iter()
            .map(|edge| edge.target)
            .collect();
        while let Some(file) = pending.pop() {
            if !self.reachable.insert(file) {
                continue;
            }
            pending.extend(self.entries[file.0].imports.iter().map(|edge| edge.target));
        }
    }

    fn symbol_identity(&self, file: FileId, local: SymbolId) -> Option<GlobalSymbolId> {
        let entry = self.entries.get(file.0)?;
        entry.snapshot()?.syntax.symbol_name(local)?;
        Some(GlobalSymbolId {
            file,
            epoch: entry.semantic_epoch?,
            local,
        })
    }

    fn named_identity(&self, file: FileId, name: &str) -> Option<GlobalSymbolId> {
        let snapshot = self.entries.get(file.0)?.snapshot()?;
        let name = snapshot.syntax.name_id(&snapshot.text, name)?;
        self.symbol_identity(file, snapshot.syntax.symbol_by_name(name)?.id)
    }
}

pub(super) fn same_snapshot(
    left: Option<&Arc<DocumentSnapshot>>,
    right: Option<&Arc<DocumentSnapshot>>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

impl WorkspaceDb {
    /// Resolve a selected call against current immutable snapshots, without a call index.
    #[must_use]
    pub fn resolve_call(&self, uri: &Url, callee_token: TokenId) -> Option<WorkspaceSymbol> {
        let source = self.file_id_by_uri(uri)?;
        let snapshot = self.entries[source.0].snapshot()?;
        let call = snapshot.syntax.call_for_token(callee_token)?;
        self.symbol_by_id(self.snapshot_call_target(source, call)?)
    }

    fn snapshot_call_target(
        &self,
        source: FileId,
        call: &crate::analysis::CallSite,
    ) -> Option<GlobalSymbolId> {
        let entry = self.entries.get(source.0)?;
        if !entry.semantic_active {
            return None;
        }
        let snapshot = entry.snapshot()?;
        let id = if let Some(local) = call.callee {
            self.symbol_identity(source, local)?
        } else {
            let file = if let Some(qualifier) = call.qualifier {
                if call
                    .qualifier_token
                    .is_some_and(|token| snapshot.syntax.symbol_for_token(token).is_some())
                {
                    return None;
                }
                let alias = snapshot.syntax.name_text(&snapshot.text, qualifier);
                match template_module(
                    snapshot,
                    alias,
                    self.semantic.prelude_files.contains(&source),
                )? {
                    TemplateModule::Import(path) => *entry.import_by_range.get(&path)?,
                    TemplateModule::CompilerBase => self.semantic.compiler_base?,
                }
            } else if self.semantic.prelude_files.contains(&source) {
                self.semantic.compiler_base?
            } else {
                return None;
            };
            self.named_identity(file, snapshot.syntax.name_text(&snapshot.text, call.name))?
        };
        self.entries[id.file.0]
            .snapshot()?
            .syntax
            .symbol_by_id(id.local)
            .filter(|symbol| symbol.kind == SymbolKind::Function)?;
        Some(id)
    }

    /// Enumerate reachable immutable snapshots for callers; no reverse call index is retained.
    #[must_use]
    pub fn incoming_calls(&self, target: GlobalSymbolId) -> Vec<WorkspaceCallGroup> {
        if self.symbol_identity(target.file, target.local) != Some(target) {
            return Vec::new();
        }
        let mut groups = HashMap::<GlobalSymbolId, WorkspaceCallGroup>::new();
        let compiler_source = self
            .semantic
            .compiler_base
            .filter(|file| !self.reachable.contains(*file));
        for source in self.reachable.iter().copied().chain(compiler_source) {
            let Some(snapshot) = self.entries[source.0].snapshot() else {
                continue;
            };
            for call in snapshot.syntax.calls() {
                let Some(caller) = call
                    .caller
                    .and_then(|local| self.symbol_identity(source, local))
                else {
                    continue;
                };
                if self.snapshot_call_target(source, call) != Some(target) {
                    continue;
                }
                let Some(symbol) = self.symbol_by_id(caller) else {
                    continue;
                };
                groups
                    .entry(caller)
                    .or_insert_with(|| WorkspaceCallGroup {
                        source: symbol.document.clone(),
                        symbol,
                        ranges: Vec::new(),
                    })
                    .ranges
                    .push(call.callee_range);
            }
        }
        sorted_groups(groups)
    }

    /// Enumerate the selected caller's local snapshot and resolve its current dependencies.
    #[must_use]
    pub fn outgoing_calls(&self, caller: GlobalSymbolId) -> Vec<WorkspaceCallGroup> {
        let Some(source) = self.symbol_by_id(caller).map(|symbol| symbol.document) else {
            return Vec::new();
        };
        let mut groups = HashMap::<GlobalSymbolId, WorkspaceCallGroup>::new();
        for call in source
            .syntax
            .calls()
            .iter()
            .filter(|call| call.caller == Some(caller.local))
        {
            let Some(id) = self.snapshot_call_target(caller.file, call) else {
                continue;
            };
            let Some(symbol) = self.symbol_by_id(id) else {
                continue;
            };
            groups
                .entry(id)
                .or_insert_with(|| WorkspaceCallGroup {
                    symbol,
                    source: source.clone(),
                    ranges: Vec::new(),
                })
                .ranges
                .push(call.callee_range);
        }
        sorted_groups(groups)
    }
}

fn sorted_groups(groups: HashMap<GlobalSymbolId, WorkspaceCallGroup>) -> Vec<WorkspaceCallGroup> {
    let mut result: Vec<_> = groups.into_values().collect();
    for group in &mut result {
        group
            .ranges
            .sort_unstable_by_key(|range| (range.start, range.end));
    }
    result.sort_unstable_by(|left, right| {
        left.symbol
            .document
            .uri
            .as_str()
            .cmp(right.symbol.document.uri.as_str())
            .then_with(|| left.symbol.id.local.0.cmp(&right.symbol.id.local.0))
    });
    result
}
