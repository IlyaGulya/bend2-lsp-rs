use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

use crate::analysis::{
    DocumentSnapshot, NameId, ReferenceKind, SymbolId, SymbolKind, TextRange, TokenId,
};
use url::Url;

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
    pub calls: usize,
}

#[derive(Clone, Copy)]
struct Occurrence {
    range: TextRange,
    kind: ReferenceKind,
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

struct PreparedTarget {
    target: TemplateTarget,
    occurrences: Vec<Occurrence>,
    calls: Vec<PreparedCall>,
}

/// One caller's span in the contribution's flat array of snapshot call ordinals.
/// Callee ranges stay in immutable syntax instead of per-caller allocations.
struct PreparedCall {
    caller: SymbolId,
    calls: Range<usize>,
}

/// Cold per-file semantic delta, prepared without a workspace lock.
///
/// Local references remain in immutable syntax spans, preserving duplicate
/// declarations and lexical bindings. Imported members remain symbolic until
/// commit. Compact target indices are shared by occurrences and selected calls.
/// Commit binds distinct groups and moves their arrays; it never scans tokens,
/// source, raw calls, or importer files.
/// External buckets use stable file/name identities, so changing an imported
/// snapshot does not require reconstructing any importing file's contribution.
pub struct PreparedSemanticSnapshot {
    snapshot: Arc<DocumentSnapshot>,
    targets: Vec<PreparedTarget>,
    call_targets: Vec<usize>,
    call_indices: Vec<usize>,
    local_occurrence_count: usize,
    local_call_count: usize,
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
    let mut call_targets = Vec::new();
    let local_occurrence_count = prepare_occurrences(
        &snapshot,
        &mut targets,
        &mut target_indices,
        &mut call_targets,
    );
    let PreparedCalls {
        local_call_count,
        call_indices,
    } = prepare_calls(
        &snapshot,
        imports_prelude,
        &mut targets,
        &mut target_indices,
        &mut call_targets,
    );
    PreparedSemanticSnapshot {
        snapshot,
        targets,
        call_targets,
        call_indices,
        local_occurrence_count,
        local_call_count,
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
    *indices.entry(target).or_insert_with(|| {
        let index = targets.len();
        targets.push(PreparedTarget {
            target,
            occurrences: Vec::new(),
            calls: Vec::new(),
        });
        index
    })
}

fn prepare_occurrences(
    snapshot: &DocumentSnapshot,
    targets: &mut Vec<PreparedTarget>,
    indices: &mut HashMap<TemplateTarget, usize>,
    call_targets: &mut Vec<usize>,
) -> usize {
    let mut local_occurrence_count = 0;
    for index in 0..snapshot.syntax.tokens().len() {
        let Some(reference) = snapshot.syntax.reference_for_token(TokenId(index)) else {
            continue;
        };
        // Own-file references already occupy compact per-symbol syntax spans.
        // Only imported occurrences need an additional workspace contribution.
        if reference.resolved.is_some() {
            local_occurrence_count += 1;
            continue;
        }
        let target = reference.qualifier.and_then(|qualifier| {
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
        });
        if let Some(target) = target {
            let index = prepared_target(targets, indices, target);
            targets[index].occurrences.push(Occurrence {
                range: reference.range,
                kind: reference.kind,
            });
            if reference.kind == ReferenceKind::Call
                && let Some(call_index) = snapshot.syntax.call_index_for_token(reference.token)
            {
                let call = &snapshot.syntax.calls()[call_index];
                if call.name == reference.name
                    && call.qualifier == reference.qualifier
                    && call.qualifier_token == reference.qualifier_token
                {
                    set_call_target(snapshot, call_targets, call_index, index);
                }
            }
        }
    }
    local_occurrence_count
}

fn set_call_target(
    snapshot: &DocumentSnapshot,
    targets: &mut Vec<usize>,
    call: usize,
    target: usize,
) {
    if targets.is_empty() {
        // The column uses the snapshot's existing call ordinals; local and
        // unresolved calls have no external target. All-local files allocate none.
        *targets = vec![usize::MAX; snapshot.syntax.calls().len()];
    }
    targets[call] = target;
}

fn call_template_target(
    snapshot: &DocumentSnapshot,
    call: &crate::analysis::CallSite,
    imports_prelude: bool,
) -> Option<TemplateTarget> {
    if let Some(qualifier) = call.qualifier {
        if call
            .qualifier_token
            .is_some_and(|token| snapshot.syntax.symbol_for_token(token).is_some())
        {
            return None;
        }
        let alias = snapshot.syntax.name_text(&snapshot.text, qualifier);
        template_module(snapshot, alias, imports_prelude).map(|module| TemplateTarget {
            module,
            name: call.name,
        })
    } else {
        imports_prelude.then_some(TemplateTarget {
            module: TemplateModule::CompilerBase,
            name: call.name,
        })
    }
}

struct PreparedCalls {
    local_call_count: usize,
    call_indices: Vec<usize>,
}

fn prepare_calls(
    snapshot: &DocumentSnapshot,
    imports_prelude: bool,
    targets: &mut Vec<PreparedTarget>,
    indices: &mut HashMap<TemplateTarget, usize>,
    call_targets: &mut Vec<usize>,
) -> PreparedCalls {
    let mut local_call_count = 0;
    let mut selected = Vec::new();
    for (call_index, call) in snapshot.syntax.calls().iter().enumerate() {
        if let Some(local) = call.callee {
            if call.caller.is_some()
                && snapshot
                    .syntax
                    .symbol_by_id(local)
                    .is_some_and(|symbol| symbol.kind == SymbolKind::Function)
            {
                local_call_count += 1;
            }
            continue;
        }
        let index = if let Some(index) = call_targets
            .get(call_index)
            .copied()
            .filter(|index| *index != usize::MAX)
        {
            index
        } else {
            let Some(target) = call_template_target(snapshot, call, imports_prelude) else {
                continue;
            };
            let index = prepared_target(targets, indices, target);
            set_call_target(snapshot, call_targets, call_index, index);
            index
        };
        if let Some(caller) = call.caller {
            selected.push((index, caller, call_index));
        }
    }
    // Group ordinals, not copied ranges. One flat array replaces every caller's
    // range allocation, including when targets alternate within a caller.
    selected.sort_unstable_by_key(|&(target, caller, _)| (target, caller.0));
    let mut call_indices = Vec::with_capacity(selected.len());
    for (target, caller, call) in selected {
        let start = call_indices.len();
        call_indices.push(call);
        let calls = &mut targets[target].calls;
        if let Some(previous) = calls.last_mut()
            && previous.caller == caller
            && previous.calls.end == start
        {
            previous.calls.end += 1;
        } else {
            calls.push(PreparedCall {
                caller,
                calls: start..start + 1,
            });
        }
    }
    PreparedCalls {
        local_call_count,
        call_indices,
    }
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
    names: Vec<Arc<str>>,
}

impl MemberNames {
    fn intern(&mut self, name: &str) -> MemberId {
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = MemberId(self.names.len());
        let name: Arc<str> = Arc::from(name);
        self.names.push(name.clone());
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

struct Contribution {
    targets: Vec<PreparedTarget>,
    call_indices: Vec<usize>,
    occurrence_count: usize,
    call_count: usize,
    call_targets: Vec<usize>,
    target_keys: Vec<Option<TargetKey>>,
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
        let Some(old) = self.contributions.get_mut(source.0).and_then(Option::take) else {
            return;
        };
        self.stats.occurrences -= old.occurrence_count;
        self.stats.calls -= old.call_count;
        // Bound keys are sufficient to remove this source directly; importing
        // files and target snapshots never need to be scanned or rebuilt.
        for target in old.target_keys.into_iter().flatten() {
            if let Some(sources) = self.external.get_mut(&target) {
                sources.remove(&source);
                if sources.is_empty() {
                    self.external.remove(&target);
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

    /// Resolve only the selected indexed call, including current dependency IDs.
    #[must_use]
    pub fn resolve_call(&self, uri: &Url, callee_token: TokenId) -> Option<WorkspaceSymbol> {
        let source = self.file_id_by_uri(uri)?;
        let contribution = self.semantic.contribution(source)?;
        let snapshot = self.entries[source.0].snapshot()?;
        let call_index = snapshot.syntax.call_index_for_token(callee_token)?;
        let call = &snapshot.syntax.calls()[call_index];
        if let Some(local) = call.callee {
            let id = self.symbol_identity(source, local)?;
            return self
                .is_function(id)
                .then(|| self.symbol_by_id(id))
                .flatten();
        }
        let target = *contribution.call_targets.get(call_index)?;
        let id = self.resolve_target(contribution.target_keys.get(target).copied().flatten()?)?;
        self.is_function(id)
            .then(|| self.symbol_by_id(id))
            .flatten()
    }

    #[must_use]
    pub fn references(
        &self,
        target: GlobalSymbolId,
        include_declaration: bool,
    ) -> Vec<WorkspaceOccurrence> {
        if self.symbol_identity(target.file, target.local) != Some(target) {
            return Vec::new();
        }
        let mut result = Vec::new();
        if self.reachable.contains(target.file)
            && self.semantic.contribution(target.file).is_some()
            && let Some(document) = self.entries[target.file.0].document()
        {
            result.extend(
                document
                    .syntax
                    .references(target.local)
                    .filter(|reference| {
                        include_declaration || reference.kind != ReferenceKind::Declaration
                    })
                    .map(|reference| WorkspaceOccurrence {
                        document: document.clone(),
                        range: reference.range,
                        kind: reference.kind,
                    }),
            );
        }
        for key in self.query_keys(target) {
            let Some(sources) = self.semantic.external.get(&key) else {
                continue;
            };
            for (&source, indices) in sources {
                if !self.reachable.contains(source) {
                    continue;
                }
                let Some(contribution) = self.semantic.contribution(source) else {
                    continue;
                };
                let Some(document) = self.entries[source.0].document() else {
                    continue;
                };
                result.extend(
                    indices
                        .iter()
                        .flat_map(|index| contribution.targets[index].occurrences.iter())
                        .filter(|occurrence| {
                            include_declaration || occurrence.kind != ReferenceKind::Declaration
                        })
                        .map(|occurrence| WorkspaceOccurrence {
                            document: document.clone(),
                            range: occurrence.range,
                            kind: occurrence.kind,
                        }),
                );
            }
        }
        result.sort_unstable_by(|left, right| {
            left.document
                .uri
                .as_str()
                .cmp(right.document.uri.as_str())
                .then_with(|| left.range.start.cmp(&right.range.start))
                .then_with(|| left.range.end.cmp(&right.range.end))
        });
        result.dedup_by(|left, right| {
            left.document.uri == right.document.uri && left.range == right.range
        });
        result
    }

    #[must_use]
    pub fn rename_occurrences(&self, target: GlobalSymbolId) -> Vec<WorkspaceOccurrence> {
        self.references(target, true)
    }

    #[must_use]
    pub fn incoming_calls(&self, target: GlobalSymbolId) -> Vec<WorkspaceCallGroup> {
        if self.symbol_identity(target.file, target.local) != Some(target)
            || !self.is_function(target)
        {
            return Vec::new();
        }
        let mut groups = HashMap::<GlobalSymbolId, WorkspaceCallGroup>::new();
        if (self.reachable.contains(target.file)
            || self.semantic.compiler_base == Some(target.file))
            && self.semantic.contribution(target.file).is_some()
            && let Some(snapshot) = self.entries[target.file.0].snapshot()
        {
            for call in snapshot.syntax.calls_to(target.local) {
                let Some(caller) = call
                    .caller
                    .and_then(|local| self.symbol_identity(target.file, local))
                else {
                    continue;
                };
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
        for key in self.query_keys(target) {
            let Some(sources) = self.semantic.external.get(&key) else {
                continue;
            };
            for (&source, indices) in sources {
                if !self.reachable.contains(source) && self.semantic.compiler_base != Some(source) {
                    continue;
                }
                let Some(contribution) = self.semantic.contribution(source) else {
                    continue;
                };
                let Some(snapshot) = self.entries[source.0].snapshot() else {
                    continue;
                };
                for index in indices.iter() {
                    for span in &contribution.targets[index].calls {
                        let Some(caller) = self.symbol_identity(source, span.caller) else {
                            continue;
                        };
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
                            .extend(
                                contribution.call_indices[span.calls.clone()]
                                    .iter()
                                    .map(|&call| snapshot.syntax.calls()[call].callee_range),
                            );
                    }
                }
            }
        }
        sorted_groups(groups)
    }

    #[must_use]
    pub fn outgoing_calls(&self, caller: GlobalSymbolId) -> Vec<WorkspaceCallGroup> {
        let Some(source) = self.symbol_by_id(caller).map(|symbol| symbol.document) else {
            return Vec::new();
        };
        let mut groups = HashMap::<GlobalSymbolId, WorkspaceCallGroup>::new();
        let contribution = self.semantic.contribution(caller.file);
        if let Some(contribution) = contribution {
            for call in source.syntax.calls_from(caller.local) {
                let target = if let Some(local) = call.callee {
                    self.symbol_identity(caller.file, local)
                } else {
                    source
                        .syntax
                        .call_index_for_token(call.callee_token)
                        .and_then(|index| contribution.call_targets.get(index))
                        .and_then(|&target| contribution.target_keys.get(target))
                        .and_then(|key| key.and_then(|key| self.resolve_target(key)))
                };
                let Some(target) = target.filter(|id| self.is_function(*id)) else {
                    continue;
                };
                let Some(symbol) = self.symbol_by_id(target) else {
                    continue;
                };
                groups
                    .entry(target)
                    .or_insert_with(|| WorkspaceCallGroup {
                        symbol,
                        source: source.clone(),
                        ranges: Vec::new(),
                    })
                    .ranges
                    .push(call.callee_range);
            }
        }
        sorted_groups(groups)
    }

    pub(super) fn install_semantics(
        &mut self,
        changed: FileId,
        prepared: Option<PreparedSemanticSnapshot>,
        imports_changed: bool,
    ) {
        let entry = &mut self.entries[changed.0];
        let snapshot = entry.snapshot().cloned();
        if !same_snapshot(entry.semantic_snapshot.as_ref(), snapshot.as_ref()) {
            entry.semantic_epoch = entry.semantic_epoch.and_then(|epoch| epoch.checked_add(1));
            entry.semantic_snapshot = snapshot;
        }
        self.semantic.remove(changed);
        if let Some(prepared) = prepared
            && (self.entries[changed.0].language_id == "bend"
                || self.entries[changed.0].language_id == "bend2")
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
        // Preserve logical occurrence statistics without copying local spans.
        let local_occurrence_count = if self.entries[source.0].semantic_epoch.is_some() {
            prepared.local_occurrence_count
        } else {
            0
        };
        let mut occurrence_count = local_occurrence_count;
        if prepared.imports_prelude {
            self.semantic.prelude_files.insert(source);
        }
        let mut target_keys = Vec::with_capacity(prepared.targets.len());
        // Different import spellings can bind to the same key. Each sparse
        // source bucket keeps their canonical group ordinals without copying.
        let mut call_count = if self.entries[source.0].semantic_epoch.is_some() {
            prepared.local_call_count
        } else {
            0
        };
        for (index, target) in prepared.targets.iter().enumerate() {
            let key = self.bind_target(source, &prepared.snapshot, target.target);
            target_keys.push(key);
            let Some(key) = key else {
                continue;
            };
            occurrence_count += target.occurrences.len();
            let mut has_calls = false;
            for span in &target.calls {
                if self.symbol_identity(source, span.caller).is_some() {
                    call_count += span.calls.len();
                    has_calls = true;
                }
            }
            if !target.occurrences.is_empty() || has_calls {
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
        self.semantic.stats.occurrences += occurrence_count;
        self.semantic.stats.calls += call_count;
        if self.semantic.contributions.len() <= source.0 {
            self.semantic
                .contributions
                .resize_with(source.0 + 1, || None);
        }
        self.semantic.contributions[source.0] = Some(Contribution {
            targets: prepared.targets,
            call_indices: prepared.call_indices,
            occurrence_count,
            call_count,
            call_targets: prepared.call_targets,
            target_keys,
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

    fn resolve_target(&self, target: TargetKey) -> Option<GlobalSymbolId> {
        let file = match target.module {
            ModuleKey::File(file) => file,
            ModuleKey::CompilerBase => self.semantic.compiler_base?,
        };
        self.named_identity(file, &self.semantic.names.names[target.member.0])
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

    fn is_function(&self, id: GlobalSymbolId) -> bool {
        self.entries[id.file.0]
            .snapshot()
            .and_then(|snapshot| snapshot.syntax.symbol_by_id(id.local))
            .is_some_and(|symbol| symbol.kind == SymbolKind::Function)
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
