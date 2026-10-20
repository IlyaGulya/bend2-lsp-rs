use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path},
    sync::Arc,
};

use super::{
    FileId, SemanticBindingId, SemanticCall, SemanticDeclaration, SemanticEntity, SemanticEntityId,
    SemanticFileContext, SemanticIssue, SemanticOccurrence, SemanticSite, SemanticTarget,
    SemanticView, SemanticViewInput,
};
use crate::analysis::{
    DeclarationKind, DeclarationSite, DocumentSnapshot, ReferenceRole, SymbolId, TextRange,
};

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
        input: &SemanticViewInput,
        file: FileId,
        namespace: String,
        aliases: HashMap<String, String>,
        base: bool,
    ) -> Self {
        let captured = input.file(file);
        let generation = captured.map_or_else(
            || input.base.as_ref().map_or(0, |base| base.generation),
            |entry| {
                entry
                    .path
                    .as_ref()
                    .and_then(|path| input.physical_generations.get(path))
                    .copied()
                    .unwrap_or(entry.generation)
            },
        );
        let conflicted = captured
            .and_then(|entry| entry.path.as_ref())
            .is_some_and(|path| input.conflicting_paths.contains(path));
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
    names: HashMap<String, SemanticTarget>,
    constructors: HashMap<String, SemanticTarget>,
    top_levels: HashMap<String, SemanticTarget>,
    // Signature metadata stays aligned with entities, including filled laws.
    template_parameter_counts: Vec<usize>,
    pending: Vec<PendingOccurrence>,
    call_count: usize,
}

pub(super) fn build(input: SemanticViewInput) -> SemanticView {
    let mut view = SemanticView {
        input,
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
        occurrences: HashMap::new(),
        references: HashMap::new(),
        calls: Vec::new(),
        incoming: HashMap::new(),
        outgoing: HashMap::new(),
        bindings: HashMap::new(),
        issues: Vec::new(),
        parameter_hints: HashMap::new(),
    };
    view.issues.extend(
        view.input
            .conflicting_paths
            .iter()
            .cloned()
            .map(|path| SemanticIssue::ConflictingOverlays { path }),
    );
    for file in &view.input.files {
        if let Some(document) = &file.document {
            view.document_indices.insert(file.id, view.documents.len());
            view.by_uri.insert(document.uri.clone(), file.id);
            let physical_uri = file
                .path
                .as_deref()
                .and_then(|path| url::Url::from_file_path(path).ok())
                .unwrap_or_else(|| document.uri.clone());
            view.by_physical_uri.insert(physical_uri.clone(), file.id);
            view.physical_uris.insert(file.id, physical_uri);
            view.documents.push(document.clone());
        }
    }
    if let (Some(base), Some(file)) = (&view.input.base, view.input.base_file) {
        view.document_indices.insert(file, view.documents.len());
        view.by_uri.insert(base.document.uri.clone(), file);
        view.by_physical_uri.insert(base.document.uri.clone(), file);
        view.physical_uris.insert(file, base.document.uri.clone());
        view.documents.push(base.document.clone());
    }
    let mut builder = Builder {
        view,
        names: HashMap::new(),
        constructors: HashMap::new(),
        top_levels: HashMap::new(),
        template_parameter_counts: Vec::new(),
        pending: Vec::new(),
        call_count: 0,
    };
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
                        .occurrences
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
        let root = self.view.input.root;
        let top = self
            .view
            .input
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
                if let Some(base_file) = self.view.input.base_file {
                    children.push((base_file, String::new(), true));
                } else {
                    self.view.issues.push(SemanticIssue::MissingImport {
                        file,
                        range: import.path,
                    });
                }
                continue;
            }
            let edge = self.view.input.file(file).and_then(|entry| {
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
                .view
                .input
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
                .view
                .input
                .file(edge.target)
                .and_then(|entry| entry.path.as_deref());
            let namespace_root = target_path.and_then(|path| {
                self.view
                    .input
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
            context: FileContext::new(&self.view.input, file, namespace, aliases, base),
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
        let mut occurrences = std::mem::take(&mut self.view.occurrences);
        let mut references: HashMap<SemanticTarget, Vec<SemanticOccurrence>> = HashMap::new();
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
                references
                    .entry(occurrence.target)
                    .or_default()
                    .push(occurrence);
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
            occurrences
                .entry(occurrence.site.file)
                .or_default()
                .push(occurrence);
        }
        for entries in occurrences.values_mut() {
            entries.sort_by_key(|entry| (entry.full_range.start, !entry.synthetic));
        }
        self.view.occurrences = compact_index(occurrences);
        self.view.references = compact_index(references);
        self.view.incoming = compact_index(incoming);
        self.view.outgoing = compact_index(outgoing);
        self.view.names = self.names;
        self.view.namespace_indices = namespace_index(&self.view.entities);
        self.view.parameter_hints = parameter_hint_index(&self.view);
        self.view
    }
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
