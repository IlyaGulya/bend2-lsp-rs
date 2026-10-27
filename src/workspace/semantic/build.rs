use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Component, Path},
    sync::Arc,
};

use super::{
    CapturedFile, Document, FileId, SemanticBindingId, SemanticCall, SemanticCapture,
    SemanticDeclaration, SemanticEntity, SemanticEntityId, SemanticFileContext, SemanticIssue,
    SemanticOccurrence, SemanticSite, SemanticTarget, SemanticView, SemanticViewInput,
};
use crate::analysis::{
    DeclarationKind, DeclarationSite, DocumentSnapshot, ReferenceRole, SymbolId, TextRange,
};

/// Captured files and their lookup index; both are dropped once build completes.
struct BuildFiles {
    files: Vec<CapturedFile>,
    indices: HashMap<FileId, usize>,
}

impl BuildFiles {
    fn file(&self, id: FileId) -> Option<&CapturedFile> {
        self.files.get(*self.indices.get(&id)?)
    }
}

struct FileContext {
    file: FileId,
    generation: u64,
    namespace: String,
    aliases: HashMap<String, String>,
    base: bool,
    conflicted: bool,
}

impl FileContext {
    fn new(
        files: &BuildFiles,
        capture: &SemanticCapture,
        file: FileId,
        namespace: String,
        aliases: HashMap<String, String>,
        base: bool,
    ) -> Self {
        let captured = files.file(file);
        let generation = captured.map_or_else(
            || capture.base.as_ref().map_or(0, |base| base.generation),
            |entry| {
                entry
                    .path
                    .as_deref()
                    .and_then(|path| capture.physical_generations.get(path))
                    .copied()
                    .unwrap_or(entry.generation)
            },
        );
        let conflicted = captured
            .and_then(|entry| entry.path.as_deref())
            .is_some_and(|path| capture.conflicting_paths.contains(path));
        Self {
            file,
            generation,
            namespace,
            aliases,
            base,
            conflicted,
        }
    }
}

struct FileLoad {
    context: FileContext,
    children: Vec<(FileId, String, bool)>,
}

#[derive(Clone)]
enum PendingTarget {
    Direct(SemanticTarget),
    Name(String),
    Declaration(FileId, SymbolId),
}

struct PendingOccurrence {
    occurrence: SemanticOccurrence,
    target: PendingTarget,
    owner: Option<SymbolId>,
}

struct Builder {
    view: SemanticView,
    files: BuildFiles,
    names: HashMap<String, SemanticTarget>,
    constructors: HashMap<String, SemanticTarget>,
    top_levels: HashMap<String, SemanticTarget>,
    // Signature metadata stays aligned with entities, including filled laws.
    template_parameter_counts: Vec<usize>,
    pending: Vec<PendingOccurrence>,
    call_count: usize,
}

pub(super) fn build(input: SemanticViewInput) -> SemanticView {
    let SemanticViewInput {
        serial,
        root,
        files: captured,
        file_indices,
        library,
        base,
        base_file,
        physical_generations,
        conflicting_paths,
    } = input;
    let mut view = SemanticView {
        capture: SemanticCapture {
            serial,
            root,
            files: Vec::new().into_boxed_slice(),
            library,
            base,
            base_file,
            physical_generations,
            conflicting_paths,
        },
        documents: Vec::new(),
        document_indices: HashMap::new(),
        entities: Vec::new(),
        by_uri: HashMap::new(),
        targets_by_site: HashMap::new(),
        declaration_indices: HashMap::new(),
        physical_uris: HashMap::new(),
        by_physical_uri: HashMap::new(),
        names: HashMap::new(),
        namespace_indices: HashMap::new(),
        contexts: HashMap::new(),
        declarations: Vec::new(),
        occurrences: Vec::new().into_boxed_slice(),
        occurrence_indices: HashMap::new(),
        reference_ranges: HashMap::new(),
        calls: Vec::new(),
        incoming: HashMap::new(),
        outgoing: HashMap::new(),
        bindings: HashMap::new(),
        issues: Vec::new(),
        parameter_hints: HashMap::new(),
    };
    view.issues.extend(
        view.capture
            .conflicting_paths
            .iter()
            .cloned()
            .map(|path| SemanticIssue::ConflictingOverlays { path }),
    );
    let mut builder = Builder {
        view,
        files: BuildFiles {
            files: captured,
            indices: file_indices,
        },
        names: HashMap::new(),
        constructors: HashMap::new(),
        top_levels: HashMap::new(),
        template_parameter_counts: Vec::new(),
        pending: Vec::new(),
        call_count: 0,
    };
    builder.install_documents();
    let contexts = builder.load_order();
    builder.reserve_indexes(&contexts);
    for context in contexts {
        builder.file(&context);
        builder.view.contexts.insert(
            context.file,
            SemanticFileContext {
                namespace: context.namespace,
                aliases: context.aliases,
                available_end: builder.view.entities.len(),
            },
        );
    }
    builder.finish()
}

impl Builder {
    fn install_documents(&mut self) {
        // The view owns one Document per captured file; the capture keeps only
        // the shared URI and snapshot identity afterwards.
        for file in &mut self.files.files {
            let Some(document) = &mut file.document else {
                continue;
            };
            let id = file.id;
            let physical_uri = Arc::clone(&file.physical_uri);
            self.view
                .document_indices
                .insert(id, self.view.documents.len());
            self.view.by_uri.insert(Arc::clone(&document.uri), id);
            self.view
                .by_physical_uri
                .insert(Arc::clone(&physical_uri), id);
            self.view.physical_uris.insert(id, physical_uri);
            self.view.documents.push(Document::with_snapshot(
                document.uri.as_ref().clone(),
                std::mem::take(&mut document.language_id),
                Arc::clone(&document.snapshot),
            ));
        }
        if let (Some(base), Some(file)) = (&self.view.capture.base, self.view.capture.base_file) {
            self.view
                .document_indices
                .insert(file, self.view.documents.len());
            let uri = Arc::new(base.document.uri.clone());
            self.view.by_uri.insert(Arc::clone(&uri), file);
            self.view.by_physical_uri.insert(Arc::clone(&uri), file);
            self.view.physical_uris.insert(file, uri);
            self.view.documents.push(base.document.clone());
        }
    }

    fn reserve_indexes(&mut self, contexts: &[FileContext]) {
        let mut declarations = 0;
        let mut bindings = 0;
        let mut references = 0;
        for context in contexts {
            if let Some(document) = self.view.document(context.file) {
                declarations += document.syntax.declaration_sites().len();
                bindings += document.syntax.bindings().len();
                let count = document.syntax.all_references().len();
                references += count;
                if count != 0 {
                    self.view
                        .occurrence_indices
                        .insert(context.file, Vec::with_capacity(count));
                }
            }
        }
        self.view.entities.reserve(declarations);
        self.view.declarations.reserve(declarations);
        self.view.declaration_indices.reserve(declarations);
        self.view.bindings.reserve(bindings);
        self.view.targets_by_site.reserve(declarations + bindings);
        self.names.reserve(declarations);
        self.top_levels.reserve(declarations);
        self.template_parameter_counts.reserve(declarations);
        self.pending.reserve(references);
    }

    fn load_order(&mut self) -> Vec<FileContext> {
        enum Event {
            Enter(FileId, String, bool),
            Finish(FileContext),
        }
        let root = self.view.capture.root;
        let top = self
            .files
            .file(root)
            .and_then(|file| file.path.as_deref())
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let mut pending = vec![Event::Enter(root, String::new(), false)];
        let mut active = HashSet::new();
        let mut loaded = HashSet::new();
        let mut ordered = Vec::new();
        while let Some(event) = pending.pop() {
            match event {
                Event::Finish(context) => {
                    active.remove(&context.file);
                    loaded.insert(context.file);
                    ordered.push(context);
                }
                Event::Enter(file, namespace, base) => {
                    if loaded.contains(&file) {
                        continue;
                    }
                    if !active.insert(file) {
                        self.view.issues.push(SemanticIssue::ImportCycle { file });
                        continue;
                    }
                    let Some(load) =
                        self.prepare_file_context(file, namespace, base, top.as_deref())
                    else {
                        active.remove(&file);
                        continue;
                    };
                    pending.push(Event::Finish(load.context));
                    pending.extend(
                        load.children
                            .into_iter()
                            .rev()
                            .map(|(file, namespace, base)| Event::Enter(file, namespace, base)),
                    );
                }
            }
        }
        ordered
    }

    fn prepare_file_context(
        &mut self,
        file: FileId,
        namespace: String,
        base: bool,
        top: Option<&Path>,
    ) -> Option<FileLoad> {
        let document = Arc::clone(&self.view.document(file)?.snapshot);
        let mut aliases = HashMap::new();
        let mut children = Vec::new();
        for import in document.syntax.imports() {
            if !import.valid_syntax {
                self.view.issues.push(SemanticIssue::MissingImport {
                    file,
                    range: import.path,
                });
                continue;
            }
            let path = import.path_text(&document.text);
            if path == "Base" && import.alias.is_none() {
                if let Some(base_file) = self.view.capture.base_file {
                    children.push((base_file, String::new(), true));
                } else {
                    self.view.issues.push(SemanticIssue::MissingImport {
                        file,
                        range: import.path,
                    });
                }
                continue;
            }
            let edge = self.files.file(file).and_then(|entry| {
                entry
                    .semantic_imports
                    .iter()
                    .find(|edge| edge.path == import.path)
            });
            let Some(edge) = edge else {
                self.view.issues.push(SemanticIssue::MissingImport {
                    file,
                    range: import.path,
                });
                continue;
            };
            if self
                .files
                .file(edge.target)
                .and_then(|entry| entry.document.as_ref())
                .is_none()
            {
                self.view.issues.push(SemanticIssue::MissingImport {
                    file,
                    range: import.path,
                });
                continue;
            }
            let target_path = self
                .files
                .file(edge.target)
                .and_then(|entry| entry.path.as_deref());
            let namespace_root = target_path.and_then(|path| {
                self.view
                    .capture
                    .library
                    .as_deref()
                    .filter(|library| path.starts_with(library))
                    .or(top)
            });
            let Some(namespace) = target_path
                .zip(namespace_root)
                .map(|(path, top)| self::namespace(path, top))
            else {
                self.view.issues.push(SemanticIssue::MissingImport {
                    file,
                    range: import.path,
                });
                continue;
            };
            if let Some(alias) = import.alias_text(&document.text) {
                if aliases.contains_key(alias) {
                    self.view.issues.push(SemanticIssue::DuplicateAlias {
                        file,
                        range: import.alias.unwrap_or(import.path),
                    });
                    continue;
                }
                aliases.insert(alias.to_owned(), namespace.clone());
            }
            children.push((edge.target, namespace, false));
        }
        Some(FileLoad {
            context: FileContext::new(
                &self.files,
                &self.view.capture,
                file,
                namespace,
                aliases,
                base,
            ),
            children,
        })
    }

    fn site(context: &FileContext, range: TextRange) -> SemanticSite {
        SemanticSite {
            file: context.file,
            generation: context.generation,
            range,
        }
    }

    fn resolve(&mut self, context: &FileContext, name: &str, site: SemanticSite) -> PendingTarget {
        let alias = name.split_once('.').and_then(|(prefix, suffix)| {
            context
                .aliases
                .get(prefix)
                .map(|namespace| (namespace, suffix))
        });
        let mut qualified = if let Some((namespace, suffix)) = alias {
            qualify(namespace, suffix)
        } else {
            qualify(&context.namespace, name)
        };
        if alias.is_some()
            && qualified != name
            && self.names.contains_key(&qualified)
            && self.names.contains_key(name)
        {
            self.view.issues.push(SemanticIssue::AmbiguousName { site });
            return PendingTarget::Direct(SemanticTarget::Ambiguous);
        }
        if qualified != name
            && !self.names.contains_key(&qualified)
            && self.names.contains_key(name)
        {
            name.clone_into(&mut qualified);
        }
        PendingTarget::Name(qualified)
    }

    fn register(
        &mut self,
        context: &FileContext,
        document: &DocumentSnapshot,
        declaration: &DeclarationSite,
    ) {
        let name = document.syntax.name_text(&document.text, declaration.name);
        let site = Self::site(context, declaration.name_range);
        let resolved = self.resolve(context, name, site);
        let existing = match &resolved {
            PendingTarget::Name(qualified) => self.top_levels.get(qualified).copied(),
            _ => None,
        };
        let fill = match existing {
            Some(SemanticTarget::Global(id)) if declaration.kind == DeclarationKind::Def => {
                let entity = &self.view.entities[id.index];
                let required = self.template_parameter_counts[id.index];
                // book_load marks Base laws native only after parsing that whole file.
                (entity.kind == DeclarationKind::Law
                    && entity.body_owner.is_none()
                    && (!entity.base || (context.base && entity.declaration.file == context.file))
                    && declaration.linkable
                    && declaration.law_fill_compatible
                    && document
                        .syntax
                        .symbol_by_id(declaration.symbol)
                        .is_some_and(|symbol| document.syntax.parameters(symbol).len() >= required))
                .then_some(id)
            }
            _ => None,
        };
        let target = if context.conflicted {
            let canonical = qualify(&context.namespace, name);
            self.names
                .insert(canonical.clone(), SemanticTarget::Ambiguous);
            let table = if declaration.kind == DeclarationKind::Constructor {
                &mut self.constructors
            } else {
                &mut self.top_levels
            };
            table.insert(canonical, SemanticTarget::Ambiguous);
            SemanticTarget::Ambiguous
        } else if let Some(id) = fill {
            let entity = &mut self.view.entities[id.index];
            entity.body_owner = Some(site);
            let mut sites = entity.sites.to_vec();
            sites.push(site);
            entity.sites = sites.into_boxed_slice();
            SemanticTarget::Global(id)
        } else {
            self.register_fresh(context, declaration, name, site)
        };
        self.view.targets_by_site.insert(site, target);
        self.view.declaration_indices.insert(
            (context.file, declaration.symbol),
            self.view.declarations.len(),
        );
        self.view.declarations.push(SemanticDeclaration {
            site,
            symbol: declaration.symbol,
            kind: declaration.kind,
            uncertain: declaration.uncertain,
            target,
        });
    }

    fn register_fresh(
        &mut self,
        context: &FileContext,
        declaration: &DeclarationSite,
        name: &str,
        site: SemanticSite,
    ) -> SemanticTarget {
        let canonical = qualify(&context.namespace, name);
        let table = if declaration.kind == DeclarationKind::Constructor {
            &self.constructors
        } else {
            &self.top_levels
        };
        let alias_conflict = name
            .split_once('.')
            .is_some_and(|(prefix, _)| context.aliases.contains_key(prefix));
        if !declaration.linkable
            || alias_conflict
            || table.contains_key(&canonical)
            || (canonical != name && table.contains_key(name))
        {
            self.view
                .issues
                .push(SemanticIssue::InvalidDeclaration { site });
            return SemanticTarget::Unresolved;
        }
        let id = SemanticEntityId {
            view: self.view.serial(),
            index: self.view.entities.len(),
        };
        self.view.entities.push(SemanticEntity {
            id,
            name: canonical.clone(),
            declaration: site,
            signature_owner: site,
            body_owner: (declaration.kind == DeclarationKind::Def).then_some(site),
            sites: vec![site].into_boxed_slice(),
            kind: declaration.kind,
            base: context.base,
        });
        self.template_parameter_counts
            .push(declaration.template_parameter_count);
        let target = SemanticTarget::Global(id);
        if declaration.kind == DeclarationKind::Constructor {
            self.constructors.insert(canonical.clone(), target);
            self.names.entry(canonical).or_insert(target);
        } else {
            self.top_levels.insert(canonical.clone(), target);
            self.names.insert(canonical, target);
        }
        target
    }

    fn file(&mut self, context: &FileContext) {
        let Some(document) = self.view.document(context.file) else {
            return;
        };
        let document = Arc::clone(&document.snapshot);
        for binding in document.syntax.bindings() {
            let id = SemanticBindingId {
                view: self.view.serial(),
                file: context.file,
                symbol: binding.id,
            };
            let site = Self::site(context, binding.name_range);
            self.view.bindings.insert(id, site);
            self.view
                .targets_by_site
                .insert(site, SemanticTarget::Local(id));
        }
        let declarations = document.syntax.declaration_sites();
        let mut registration: Vec<_> = declarations.iter().collect();
        registration.sort_by_key(|declaration| match declaration.kind {
            DeclarationKind::Law => declaration.full_range.end,
            _ => declaration
                .body_range
                .map_or(declaration.signature_range.end, |range| range.start),
        });
        let mut next = 0;
        for reference in document.syntax.all_references() {
            let lexical = &reference.reference;
            while let Some(declaration) = registration.get(next) {
                let at = match declaration.kind {
                    DeclarationKind::Law => declaration.full_range.end,
                    _ => declaration
                        .body_range
                        .map_or(declaration.signature_range.end, |range| range.start),
                };
                if at > reference.full_range.start {
                    break;
                }
                self.register(context, &document, declaration);
                next += 1;
            }
            let site = Self::site(context, lexical.range);
            let local = reference
                .local_binding
                .and_then(|id| document.syntax.bindings().get(id.0))
                .map(|binding| binding.id);
            let target = if reference.uncertain {
                PendingTarget::Direct(SemanticTarget::Unresolved)
            } else if reference.role == ReferenceRole::Binding {
                if let Some(symbol) = local {
                    PendingTarget::Direct(SemanticTarget::Local(SemanticBindingId {
                        view: self.view.serial(),
                        file: context.file,
                        symbol,
                    }))
                } else if let Some(declaration) = lexical
                    .resolved
                    .and_then(|symbol| document.syntax.declaration_site(symbol))
                    .filter(|declaration| declaration.name_range == reference.full_range)
                {
                    PendingTarget::Declaration(context.file, declaration.symbol)
                } else {
                    PendingTarget::Direct(SemanticTarget::Unresolved)
                }
            } else if let Some(symbol) = local {
                PendingTarget::Direct(SemanticTarget::Local(SemanticBindingId {
                    view: self.view.serial(),
                    file: context.file,
                    symbol,
                }))
            } else {
                let name = document
                    .syntax
                    .name_text(&document.text, reference.full_name);
                self.resolve(context, name, site)
            };
            self.call_count += usize::from(
                !reference.in_type
                    && matches!(reference.role, ReferenceRole::Call | ReferenceRole::Offload),
            );
            self.pending.push(PendingOccurrence {
                occurrence: SemanticOccurrence {
                    site,
                    full_range: reference.full_range,
                    role: reference.role,
                    in_type: reference.in_type,
                    uncertain: reference.uncertain,
                    synthetic: reference.synthetic,
                    target: SemanticTarget::Unresolved,
                    enclosing: None,
                },
                target,
                owner: lexical.enclosing,
            });
        }
        for declaration in &registration[next..] {
            self.register(context, &document, declaration);
        }
    }

    fn finish(mut self) -> SemanticView {
        self.view.calls.reserve(self.call_count);
        let mut occurrence_indices = std::mem::take(&mut self.view.occurrence_indices);
        let mut occurrences = Vec::with_capacity(self.pending.len());
        let mut reference_ranges: HashMap<SemanticTarget, Range<usize>> = HashMap::new();
        let mut incoming: HashMap<SemanticTarget, Vec<usize>> = HashMap::new();
        let mut outgoing: HashMap<SemanticEntityId, Vec<usize>> = HashMap::new();
        for pending in self.pending {
            let mut occurrence = pending.occurrence;
            occurrence.target = match pending.target {
                PendingTarget::Direct(target) => target,
                PendingTarget::Name(name) => {
                    let table = if occurrence.role == ReferenceRole::Constructor {
                        &self.constructors
                    } else {
                        &self.top_levels
                    };
                    table
                        .get(&name)
                        .copied()
                        .unwrap_or(SemanticTarget::Unresolved)
                }
                PendingTarget::Declaration(file, symbol) => self
                    .view
                    .declaration(file, symbol)
                    .map_or(SemanticTarget::Unresolved, |declaration| declaration.target),
            };
            occurrence.enclosing = pending
                .owner
                .and_then(|owner| self.view.declaration(occurrence.site.file, owner))
                .and_then(|declaration| match declaration.target {
                    SemanticTarget::Global(id) => Some(id),
                    _ => None,
                });
            if matches!(
                occurrence.target,
                SemanticTarget::Global(_) | SemanticTarget::Local(_)
            ) {
                reference_ranges.entry(occurrence.target).or_default().end += 1;
            }
            if !occurrence.in_type
                && matches!(
                    occurrence.role,
                    ReferenceRole::Call | ReferenceRole::Offload
                )
            {
                let index = self.view.calls.len();
                self.view.calls.push(SemanticCall {
                    site: occurrence.site,
                    full_range: occurrence.full_range,
                    caller: occurrence.enclosing,
                    target: occurrence.target,
                    role: occurrence.role,
                });
                incoming.entry(occurrence.target).or_default().push(index);
                if let Some(caller) = occurrence.enclosing {
                    outgoing.entry(caller).or_default().push(index);
                }
            }
            occurrence_indices
                .entry(occurrence.site.file)
                .or_default()
                .push(occurrences.len());
            occurrences.push(occurrence);
        }
        let source_indices = group_occurrences(&mut occurrences, &mut reference_ranges);
        for indices in occurrence_indices.values_mut() {
            if !source_indices.is_empty() {
                for index in &mut *indices {
                    *index = source_indices[*index];
                }
            }
            indices.sort_by_key(|index| {
                let occurrence = &occurrences[*index];
                (occurrence.full_range.start, !occurrence.synthetic)
            });
        }
        self.view.occurrences = occurrences.into_boxed_slice();
        self.view.occurrence_indices = compact_index(occurrence_indices);
        self.view.reference_ranges = reference_ranges;
        self.view.incoming = compact_index(incoming);
        self.view.outgoing = compact_index(outgoing);
        self.view.names = self.names;
        self.view.namespace_indices = namespace_index(&self.view.entities);
        self.view.parameter_hints = parameter_hint_index(&self.view);
        self.view.capture.files = self
            .files
            .files
            .into_iter()
            .map(CapturedFile::into_receipt)
            .collect();
        self.view
    }
}

/// Keep reference slices contiguous without retaining a second wide-record copy.
/// The returned source-to-arena permutation is empty when grouping is unnecessary.
fn group_occurrences(
    occurrences: &mut [SemanticOccurrence],
    reference_ranges: &mut HashMap<SemanticTarget, Range<usize>>,
) -> Vec<usize> {
    let resolved_count: usize = reference_ranges.values().map(|range| range.end).sum();
    let group_count = reference_ranges.len() + usize::from(resolved_count < occurrences.len());
    if group_count <= 1 {
        return Vec::new();
    }
    let mut total = 0;
    for range in reference_ranges.values_mut() {
        let count = range.end;
        *range = total..total;
        total += count;
    }
    // Unresolved and ambiguous records share an unexposed final group.
    let mut unresolved = total..total;
    let mut order = vec![0; occurrences.len()];
    for (source, occurrence) in occurrences.iter().enumerate() {
        let range = match occurrence.target {
            SemanticTarget::Global(_) | SemanticTarget::Local(_) => {
                reference_ranges.entry(occurrence.target).or_default()
            }
            SemanticTarget::Unresolved | SemanticTarget::Ambiguous => &mut unresolved,
        };
        order[range.end] = source;
        range.end += 1;
    }
    let mut source_indices = vec![0; occurrences.len()];
    // Mirror the syntax index's stable inverse scatter: each record moves once.
    for start in 0..order.len() {
        let source = order[start];
        if source == usize::MAX {
            continue;
        }
        if source == start {
            source_indices[start] = start;
            order[start] = usize::MAX;
            continue;
        }
        let displaced = occurrences[start];
        let mut destination = start;
        loop {
            let origin = order[destination];
            order[destination] = usize::MAX;
            source_indices[origin] = destination;
            if origin == start {
                occurrences[destination] = displaced;
                break;
            }
            occurrences[destination] = occurrences[origin];
            destination = origin;
        }
    }
    source_indices
}

// Compact each payload without allocating replacement tables and rehashing keys.
fn compact_index<K, V>(mut index: HashMap<K, Vec<V>>) -> HashMap<K, Vec<V>> {
    for entries in index.values_mut() {
        entries.shrink_to_fit();
    }
    index
}

fn parameter_hint_index(view: &SemanticView) -> HashMap<FileId, Box<[(usize, String)]>> {
    let mut indexed = HashMap::new();
    for (&file, &document_index) in &view.document_indices {
        let document = &view.documents[document_index];
        let mut hints = Vec::new();
        for call in document.syntax.calls() {
            let Some(occurrence) = view.resolve_at(file, call.callee_range.start) else {
                continue;
            };
            let SemanticTarget::Global(id) = occurrence.target else {
                continue;
            };
            let Some(entity) = view.entity(id) else {
                continue;
            };
            let Some(signature) = view.document(entity.signature_owner.file) else {
                continue;
            };
            let Some(token) = signature
                .syntax
                .token_at_or_before(entity.signature_owner.range.start)
            else {
                continue;
            };
            let Some(symbol) = signature
                .syntax
                .symbol_for_token(token)
                .and_then(|symbol| signature.syntax.symbol_by_id(symbol))
            else {
                continue;
            };
            let parameters = signature.syntax.parameters(symbol);
            for (argument, parameter) in document.syntax.call_arguments(call).iter().zip(parameters)
            {
                let hint = crate::analysis::InlayHint {
                    position: argument.start,
                    parameter_name: parameter.name_range,
                };
                hints.push((argument.start, hint.label(&signature.text)));
            }
        }
        if !hints.is_empty() {
            hints.sort_unstable_by_key(|hint| hint.0);
            indexed.insert(file, hints.into_boxed_slice());
        }
    }
    indexed
}

fn namespace_index(entities: &[SemanticEntity]) -> HashMap<String, Box<[usize]>> {
    let mut namespaces: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, entity) in entities.iter().enumerate() {
        let namespace = entity
            .name
            .split_once(':')
            .map_or("", |(namespace, _)| namespace);
        namespaces
            .entry(namespace.to_owned())
            .or_default()
            .push(index);
    }
    namespaces
        .into_iter()
        .map(|(namespace, mut indices)| {
            indices.sort_unstable_by(|left, right| {
                entities[*left]
                    .name
                    .cmp(&entities[*right].name)
                    .then(left.cmp(right))
            });
            (namespace, indices.into_boxed_slice())
        })
        .collect()
}

fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_owned()
    } else {
        let mut qualified = String::with_capacity(namespace.len() + 1 + name.len());
        qualified.push_str(namespace);
        qualified.push(':');
        qualified.push_str(name);
        qualified
    }
}

fn namespace(path: &Path, root: &Path) -> String {
    let mut path = path.components().peekable();
    let mut root = root.components().peekable();
    while path.peek().is_some() && path.peek() == root.peek() {
        path.next();
        root.next();
    }
    let mut namespace = String::new();
    for component in root {
        if matches!(component, Component::Normal(_)) {
            if !namespace.is_empty() {
                namespace.push('/');
            }
            namespace.push_str("..");
        }
    }
    for component in path {
        if let Component::Normal(name) = component {
            if !namespace.is_empty() {
                namespace.push('/');
            }
            namespace.push_str(&name.to_string_lossy());
        }
    }
    if namespace.ends_with(".bend") {
        namespace.truncate(namespace.len() - 5);
    }
    namespace
}
