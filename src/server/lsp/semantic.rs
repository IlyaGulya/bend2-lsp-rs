use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_json::{Value, json};
use tokio::sync::RwLockReadGuard;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, CompletionTextEdit};
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, DocumentChanges,
        Location, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range,
        TextDocumentEdit, TextEdit, TypeHierarchyItem, WorkspaceEdit,
    },
};

use crate::{
    analysis::{self, ReferenceRole},
    workspace::{
        Document, FileId, SemanticBase, SemanticEntity, SemanticEntityId, SemanticOccurrence,
        SemanticSite, SemanticTarget, SemanticView, SemanticViewInput,
    },
};

use super::Backend;
use crate::server::{features, orchestration::run_staging};
use url::Url;

pub(in crate::server) struct Selection {
    pub view: Arc<SemanticView>,
    pub occurrence: SemanticOccurrence,
}

#[derive(Clone, Copy)]
enum SemanticPreparation<'a> {
    Open,
    Entries(&'a [FileId]),
}

/// Bound roots per staging task and publication guard while amortizing dispatch.
/// Shutdown can stop between individual builds within the batch.
const SEMANTIC_PREPARATION_BATCH: usize = 32;

impl Backend {
    fn semantic_base(&self) -> Option<SemanticBase> {
        let module = self.compiler.base_module.read().clone()?;
        Some(SemanticBase {
            generation: module.snapshot.content_hash(),
            provenance: module.provenance,
            document: Document::with_snapshot(module.uri, "bend".into(), module.snapshot),
            source_lease: Some(module.source_directory),
        })
    }

    /// Cold preparation only: callers finish this before acknowledging an update.
    pub(in crate::server) async fn prepare_semantic_views(&self) {
        self.prepare_semantic_entries(SemanticPreparation::Open)
            .await;
    }

    pub(in crate::server) async fn prepare_semantic_views_for_read(&self, roots: &[FileId]) {
        self.prepare_semantic_entries(SemanticPreparation::Entries(roots))
            .await;
    }

    fn missing_semantic_entries(&self, scope: SemanticPreparation<'_>) -> Vec<FileId> {
        let database = self.workspace.read();
        match scope {
            SemanticPreparation::Entries(roots) => roots
                .iter()
                .copied()
                .filter(|root| !database.semantic_views_ready_for(Some(*root)))
                .collect(),
            SemanticPreparation::Open => database.missing_open_semantic_roots(),
        }
    }

    async fn prepare_semantic_entries(&self, scope: SemanticPreparation<'_>) {
        if self.missing_semantic_entries(scope).is_empty() {
            return;
        }
        let _preparation = self.workspace.semantic_preparation.lock().await;
        loop {
            let roots = self.missing_semantic_entries(scope);
            if roots.is_empty() {
                return;
            }
            let mut published = false;
            for chunk in roots.chunks(SEMANTIC_PREPARATION_BATCH) {
                if self.workspace.staging.is_closed() {
                    return;
                }
                let base = self.semantic_base();
                let load_base = base.is_none();
                let mut inputs = Vec::with_capacity(chunk.len());
                for root in chunk {
                    if self.workspace.staging.is_closed() {
                        return;
                    }
                    // One short capture guard per root: chunking changes only
                    // how many staging and publication round trips are paid.
                    let input = self
                        .workspace
                        .read()
                        .capture_semantic_view(*root, base.clone());
                    if let Some(input) = input {
                        inputs.push(input);
                    }
                }
                if inputs.is_empty() {
                    continue;
                }
                if load_base && inputs.iter().any(SemanticViewInput::requires_base) {
                    self.load_prelude_module().await;
                    if let Some(base) = self.semantic_base() {
                        inputs = chunk
                            .iter()
                            .take_while(|_| !self.workspace.staging.is_closed())
                            .filter_map(|root| {
                                self.workspace
                                    .read()
                                    .capture_semantic_view(*root, Some(base.clone()))
                            })
                            .collect();
                    }
                }
                let staging = self.workspace.staging.clone();
                let Some(views) = run_staging(staging.clone(), move || {
                    inputs
                        .into_iter()
                        .take_while(|_| !staging.is_closed())
                        .map(|input| Arc::new(input.build()))
                        .collect::<Vec<_>>()
                })
                .await
                else {
                    return;
                };
                let _update = self.workspace.updates.write().await;
                let current_base = self.semantic_base();
                for view in views {
                    published |= self
                        .workspace
                        .publish_semantic_view(view, current_base.as_ref());
                }
            }
            // A superseding revision or closed epoch invalidates the captured
            // inputs. Its owner prepares the replacement after committing.
            if !published {
                return;
            }
        }
    }

    fn missing_semantic_reference_roots(&self, file: FileId) -> Vec<FileId> {
        let database = self.workspace.read();
        // Dirty roots are exactly the semantic roots without a current view, so
        // an empty dirty set proves nothing is missing without walking the
        // reverse-import closure of a settled workspace.
        if database.semantic_views_ready_for(None) {
            return Vec::new();
        }
        database
            .semantic_entry_roots_for_file(file)
            .into_iter()
            .filter(|root| !database.semantic_views_ready_for(Some(*root)))
            .collect()
    }

    pub(in crate::server) async fn semantic_reference_read(
        &self,
        uri: &Url,
        position: Position,
    ) -> RwLockReadGuard<'_, ()> {
        self.semantic_reference_read_inner(uri, Some(position))
            .await
    }

    pub(in crate::server) async fn semantic_reference_document_read(
        &self,
        uri: &Url,
    ) -> RwLockReadGuard<'_, ()> {
        self.semantic_reference_read_inner(uri, None).await
    }

    async fn semantic_reference_read_inner(
        &self,
        uri: &Url,
        position: Option<Position>,
    ) -> RwLockReadGuard<'_, ()> {
        loop {
            let read = self.workspace_ready_read().await;
            let Some(root) = self.workspace.read().file_id_by_uri(uri) else {
                return read;
            };
            if !self.workspace.read().semantic_views_ready_for(Some(root)) {
                drop(read);
                self.prepare_semantic_views_for_read(std::slice::from_ref(&root))
                    .await;
                continue;
            }
            let target = position.and_then(|position| {
                let document = self.document(uri)?;
                let offset = super::super::adapters::offset_at(&document, position);
                let selection = self.semantic_selection(&document, offset)?;
                Self::semantic_reference_file(&selection.view, selection.occurrence.target)
            });
            let roots = self.missing_semantic_reference_roots(target.unwrap_or(root));
            if roots.is_empty() {
                return read;
            }
            drop(read);
            self.prepare_semantic_views_for_read(&roots).await;
        }
    }

    fn semantic_reference_file(view: &SemanticView, target: SemanticTarget) -> Option<FileId> {
        if let SemanticTarget::Global(id) = target
            && view.entity(id)?.base
        {
            return Some(FileId::BASE);
        }
        view.target_site(target).map(|site| site.file)
    }

    pub(in crate::server) async fn semantic_incoming_read(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<RwLockReadGuard<'_, ()>> {
        loop {
            let read = self.workspace_ready_read().await;
            let (view, entity) = self.semantic_hierarchy_target(item)?;
            let file = Self::semantic_reference_file(&view, SemanticTarget::Global(entity))
                .ok_or_else(Error::content_modified)?;
            let roots = self.missing_semantic_reference_roots(file);
            if roots.is_empty() {
                return Ok(read);
            }
            drop(read);
            self.prepare_semantic_views_for_read(&roots).await;
        }
    }

    pub(in crate::server) fn semantic_view(&self, uri: &Url) -> Option<Arc<SemanticView>> {
        let database = self.workspace.read();
        database.semantic_view(database.file_id_by_uri(uri)?)
    }

    pub(in crate::server) fn semantic_selection(
        &self,
        document: &Document,
        offset: usize,
    ) -> Option<Selection> {
        if features::cursor_in_comment_or_string(document, offset) {
            return None;
        }
        let view = self.semantic_view(&document.uri)?;
        let file = view.file_id(&document.uri)?;
        let occurrence = *view.resolve_at(file, offset)?;
        if offset < occurrence.site.range.start
            && occurrence.role != ReferenceRole::Binding
            && let Some(token) = document
                .syntax
                .token_at_or_before(occurrence.full_range.start)
                .and_then(|id| document.syntax.token(id))
            && offset <= token.range.end
            && document.syntax.imports().iter().any(|import| {
                import.valid_syntax
                    && import.alias_text(&document.text)
                        == document.text.get(token.range.start..token.range.end)
            })
        {
            return None;
        }
        Some(Selection { view, occurrence })
    }

    pub(in crate::server) fn semantic_location(
        view: &SemanticView,
        site: SemanticSite,
    ) -> Option<Location> {
        let document = view.document(site.file)?;
        Some(Location {
            uri: document.uri.clone(),
            range: super::super::adapters::range(document, site.range),
        })
    }

    pub(in crate::server) fn semantic_symbol(
        view: &SemanticView,
        site: SemanticSite,
    ) -> Option<(Document, analysis::SymbolId)> {
        let document = view.document(site.file)?.clone();
        let token = document.syntax.token_at_or_before(site.range.start)?;
        let symbol = document.syntax.symbol_for_token(token)?;
        document.syntax.symbol_by_id(symbol)?;
        Some((document, symbol))
    }

    pub(in crate::server) fn semantic_hierarchy_item(
        view: &SemanticView,
        entity: SemanticEntityId,
    ) -> Option<CallHierarchyItem> {
        if !matches!(
            view.entity(entity)?.kind,
            analysis::DeclarationKind::Def | analysis::DeclarationKind::Law
        ) {
            return None;
        }
        Self::semantic_entity_item(view, entity)
    }

    fn semantic_entity_item(
        view: &SemanticView,
        entity: SemanticEntityId,
    ) -> Option<CallHierarchyItem> {
        let entry = view.entity(entity)?;
        // Call ranges are relative to the item's URI, not the law's signature file.
        let (document, symbol) = Self::semantic_symbol(view, hierarchy_site(entry))?;
        let symbol = super::super::adapters::document_symbol_by_id(&document, symbol)?;
        let mut item = features::call_hierarchy_item(document.uri, symbol);
        let declaration = view.document(entry.declaration.file)?;
        let name = declaration
            .text
            .get(entry.declaration.range.start..entry.declaration.range.end)?;
        name.clone_into(&mut item.name);
        item.data = Some(json!({
            "schema": 1,
            "rootUri": view.root_uri(),
            "view": view.serial(),
            "entity": entity.index,
        }));
        Some(item)
    }

    pub(in crate::server) fn semantic_hierarchy_target(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<(Arc<SemanticView>, SemanticEntityId)> {
        self.semantic_handle_target(item.data.as_ref(), &item.uri, item.selection_range)
    }

    fn semantic_handle_target(
        &self,
        data: Option<&Value>,
        uri: &Url,
        selection_range: Range,
    ) -> Result<(Arc<SemanticView>, SemanticEntityId)> {
        let data =
            data.ok_or_else(|| Error::invalid_params("missing semantic hierarchy handle"))?;
        if data["schema"] != 1 {
            return Err(Error::invalid_params(
                "unsupported semantic hierarchy handle",
            ));
        }
        let root = data["rootUri"]
            .as_str()
            .and_then(|uri| Url::parse(uri).ok())
            .ok_or_else(|| Error::invalid_params("missing hierarchy root"))?;
        let view = self
            .semantic_view(&root)
            .ok_or_else(Error::content_modified)?;
        if data["view"].as_u64() != Some(view.serial()) {
            return Err(Error::content_modified());
        }
        let index = data["entity"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| Error::invalid_params("missing hierarchy entity"))?;
        let entity = SemanticEntityId {
            view: view.serial(),
            index,
        };
        let entry = view.entity(entity).ok_or_else(Error::content_modified)?;
        let location = Self::semantic_location(&view, hierarchy_site(entry))
            .ok_or_else(Error::content_modified)?;
        if location.uri != *uri || location.range != selection_range {
            return Err(Error::content_modified());
        }
        Ok((view, entity))
    }

    pub(in crate::server) fn semantic_reference_views(
        &self,
        selection: &Selection,
    ) -> Vec<Arc<SemanticView>> {
        let Some(site) = selection.view.target_site(selection.occurrence.target) else {
            return Vec::new();
        };
        let Some(document) = selection.view.document(site.file) else {
            return Vec::new();
        };
        let mut views = {
            let database = self.workspace.read();
            database.views_for_file(database.file_id_by_uri(&document.uri).unwrap_or(site.file))
        };
        if !views
            .iter()
            .any(|view| view.serial() == selection.view.serial())
        {
            views.push(selection.view.clone());
        }
        views
    }

    pub(in crate::server) fn corresponding_target(
        selection: &Selection,
        view: &SemanticView,
    ) -> Option<SemanticTarget> {
        let site = selection.view.target_site(selection.occurrence.target)?;
        let uri = selection.view.physical_uri(site.file)?;
        let file = view.physical_file_id(uri)?;
        let target = view.resolve_at(file, site.range.start)?.target;
        let actual = view.target_site(target)?;
        (actual.range == site.range).then_some(target)
    }

    pub(in crate::server) fn semantic_references(
        &self,
        selection: &Selection,
        include_declaration: bool,
    ) -> Vec<Location> {
        let mut locations = Vec::new();
        for view in self.semantic_reference_views(selection) {
            let Some(target) = Self::corresponding_target(selection, &view) else {
                continue;
            };
            locations.extend(
                view.references(target)
                    .iter()
                    .filter(|reference| {
                        include_declaration || reference.role != ReferenceRole::Binding
                    })
                    .filter_map(|reference| Self::semantic_location(&view, reference.site)),
            );
        }
        super::super::reference_locations::sort_and_deduplicate(&mut locations);
        locations
    }

    pub(in crate::server) fn semantic_rename_is_safe(
        &self,
        selection: &Selection,
        new_name: &str,
    ) -> bool {
        for view in self.semantic_reference_views(selection) {
            let Some(target) = Self::corresponding_target(selection, &view) else {
                return false;
            };
            if !view.rename_is_safe(target, new_name) {
                return false;
            }
            let edited_files: HashSet<_> = view
                .references(target)
                .iter()
                .map(|reference| reference.site.file)
                .collect();
            let database = self.workspace.read();
            let mut checked = HashSet::new();
            for file in edited_files {
                for other in database.views_for_file(file) {
                    if checked.insert(other.serial()) && view.rename_conflicts(target, &other) {
                        return false;
                    }
                }
            }
        }
        true
    }

    pub(in crate::server) fn semantic_workspace_edit(
        &self,
        changes: HashMap<Url, Vec<TextEdit>>,
    ) -> WorkspaceEdit {
        let versioned = self.registration.read().versioned_edits;
        if !versioned {
            return WorkspaceEdit {
                changes: Some(changes),
                ..WorkspaceEdit::default()
            };
        }
        let database = self.workspace.read();
        let mut edits: Vec<_> = changes
            .into_iter()
            .map(|(uri, edits)| {
                let version = database
                    .open_document(&uri)
                    .map(|document| document.revision.0);
                TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                }
            })
            .collect();
        edits.sort_by(|left, right| {
            left.text_document
                .uri
                .as_str()
                .cmp(right.text_document.uri.as_str())
        });
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(edits)),
            ..WorkspaceEdit::default()
        }
    }

    pub(in crate::server) fn semantic_completion_items(
        &self,
        document: &Document,
        offset: usize,
    ) -> (Vec<CompletionItem>, bool) {
        let Some(view) = self.semantic_view(&document.uri) else {
            return (Vec::new(), false);
        };
        let Some(file) = view.file_id(&document.uri) else {
            return (Vec::new(), false);
        };
        let syntax = &document.syntax;
        let span = completion_span(document, &view, file, offset);
        let prefix = &document.text[span.start..offset];
        let qualifier_end = prefix.rfind('.').map_or(0, |dot| dot + 1);
        let pattern = syntax.case_pattern_type(offset);
        let expected_type = Self::semantic_pattern_type(
            &view,
            file,
            pattern.and_then(|pattern| pattern.explicit_type),
        );
        let edit_range = super::super::adapters::range(document, span);
        let mut items = Vec::new();
        let visibility_offset = if span.start < offset {
            span.start
        } else {
            offset
        };
        if pattern.is_none() {
            Self::append_local_completions(
                document,
                visibility_offset,
                prefix,
                edit_range,
                &mut items,
            );
        }
        for candidate in view.completion_candidates(file, prefix) {
            let SemanticTarget::Global(id) = candidate.target else {
                continue;
            };
            let Some(entity) = view.entity(id) else {
                continue;
            };
            if entity.kind != analysis::DeclarationKind::Constructor
                && (pattern.is_some()
                    || syntax
                        .name_id(&document.text, &candidate.name)
                        .is_some_and(|name| {
                            syntax.binding_named_at(name, visibility_offset).is_some()
                        }))
            {
                continue;
            }
            let Some((signature, symbol)) = Self::semantic_symbol(&view, entity.signature_owner)
            else {
                continue;
            };
            let Some(metadata) = analysis::declaration_metadata(&signature, symbol) else {
                continue;
            };
            if let Some(expected) = expected_type {
                let parent = signature
                    .syntax
                    .constructor_by_symbol(symbol)
                    .and_then(|constructor| {
                        view.declaration(entity.declaration.file, constructor.parent)
                    })
                    .map(|declaration| declaration.target);
                if parent != Some(SemanticTarget::Global(expected)) {
                    continue;
                }
            }
            let label = candidate
                .name
                .get(qualifier_end..)
                .unwrap_or(&candidate.name)
                .to_owned();
            let detail = signature.text
                [metadata.signature_range.start..metadata.signature_range.end]
                .to_owned();
            let mut item = CompletionItem::new_simple(label, detail);
            item.kind = Some(match entity.kind {
                analysis::DeclarationKind::Def | analysis::DeclarationKind::Law => {
                    CompletionItemKind::FUNCTION
                }
                analysis::DeclarationKind::Type => CompletionItemKind::STRUCT,
                analysis::DeclarationKind::Constructor => CompletionItemKind::CONSTRUCTOR,
            });
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range: edit_range,
                new_text: candidate.name,
            }));
            items.push(item);
        }
        if qualifier_end == 0 && pattern.is_none() {
            Self::append_unqualified_completions(document, prefix, &mut items);
        }
        (items, expected_type.is_some())
    }

    fn semantic_pattern_type(
        view: &SemanticView,
        file: FileId,
        annotation: Option<analysis::TextRange>,
    ) -> Option<SemanticEntityId> {
        let occurrence = view.resolve_at(file, annotation?.start)?;
        let SemanticTarget::Global(id) = occurrence.target else {
            return None;
        };
        (view.entity(id)?.kind == analysis::DeclarationKind::Type).then_some(id)
    }

    fn append_local_completions(
        document: &Document,
        offset: usize,
        prefix: &str,
        edit_range: Range,
        items: &mut Vec<CompletionItem>,
    ) {
        for binding in document.syntax.bindings_at(offset) {
            let name = document.syntax.name_text(&document.text, binding.name);
            if analysis::completion_match(name, prefix).is_none() {
                continue;
            }
            let mut item = CompletionItem::new_simple(name.to_owned(), "Local binding".into());
            item.kind = Some(CompletionItemKind::VARIABLE);
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range: edit_range,
                new_text: name.to_owned(),
            }));
            items.push(item);
        }
    }

    fn append_unqualified_completions(
        document: &Document,
        prefix: &str,
        items: &mut Vec<CompletionItem>,
    ) {
        for keyword in analysis::keyword_completions(prefix) {
            if !items.iter().any(|item| item.label == keyword.label) {
                let mut item = CompletionItem::new_simple(keyword.label, keyword.detail);
                item.kind = Some(CompletionItemKind::KEYWORD);
                items.push(item);
            }
        }
        for import in document
            .syntax
            .imports()
            .iter()
            .filter(|import| import.valid_syntax && import.path_text(&document.text) != "Base")
        {
            let Some(alias) = import
                .alias_text(&document.text)
                .filter(|alias| analysis::completion_match(alias, prefix).is_some())
            else {
                continue;
            };
            if items.iter().any(|item| item.label == alias) {
                continue;
            }
            let mut item = CompletionItem::new_simple(
                alias.to_owned(),
                format!("Imported module {}", import.path_text(&document.text)),
            );
            item.kind = Some(CompletionItemKind::MODULE);
            items.push(item);
        }
    }

    pub(in crate::server) fn semantic_inlay_hints(
        &self,
        document: &Document,
        range: analysis::TextRange,
    ) -> Vec<(usize, String)> {
        let Some(view) = self.semantic_view(&document.uri) else {
            return Vec::new();
        };
        let Some(file) = view.file_id(&document.uri) else {
            return Vec::new();
        };
        view.parameter_hints(file, range)
    }

    pub(in crate::server) fn semantic_incoming(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<Vec<CallHierarchyIncomingCall>> {
        let (selected, entity) = self.semantic_hierarchy_target(item)?;
        let declaration = selected
            .entity(entity)
            .ok_or_else(Error::content_modified)?
            .declaration;
        let occurrence = *selected
            .resolve_at(declaration.file, declaration.range.start)
            .ok_or_else(Error::content_modified)?;
        let selection = Selection {
            view: selected,
            occurrence,
        };
        let views = self.semantic_reference_views(&selection);
        let mut candidates: Vec<IncomingCallCandidate<'_>> = Vec::new();
        for view in &views {
            let Some(target) = Self::corresponding_target(&selection, view) else {
                continue;
            };
            let mut grouped = HashMap::<SemanticEntityId, Vec<Range>>::new();
            for call in view.calls_to(target) {
                let Some(caller) = call.caller() else {
                    continue;
                };
                if let Some(location) = Self::semantic_location(view, call.site()) {
                    grouped.entry(caller).or_default().push(location.range);
                }
            }
            for (caller, mut ranges) in grouped {
                let Some(from) = Self::semantic_hierarchy_item(view, caller) else {
                    continue;
                };
                ranges.sort_by_key(range_order);
                ranges.dedup();
                candidates.push((
                    view.as_ref(),
                    caller,
                    CallHierarchyIncomingCall {
                        from,
                        from_ranges: ranges,
                    },
                ));
            }
        }
        candidates.sort_by(|left, right| {
            incoming_caller_order(&left.2).cmp(&incoming_caller_order(&right.2))
        });
        let prepared = selection.view.serial();
        let mut stack = Vec::new();
        let mut visited = HashSet::new();
        // Compact each physical-caller run in place. The retained prefix holds
        // one representative per complete hierarchy meaning, without allocating
        // a separate run or cloning an entry view for every caller.
        let mut retained = 0;
        let mut group_start = 0;
        for current in 0..candidates.len() {
            if retained == 0
                || incoming_caller_order(&candidates[group_start].2)
                    != incoming_caller_order(&candidates[current].2)
            {
                group_start = retained;
            }
            let equivalent = (group_start..retained).find(|&index| {
                let representative = &candidates[index];
                let candidate = &candidates[current];
                same_incoming_caller(&representative.2, &candidate.2)
                    && representative.2.from_ranges == candidate.2.from_ranges
                    && same_hierarchy_expansion(
                        representative.0,
                        representative.1,
                        candidate.0,
                        candidate.1,
                        &mut stack,
                        &mut visited,
                    )
            });
            if let Some(representative) = equivalent {
                if candidates[representative].0.serial() != prepared
                    && candidates[current].0.serial() == prepared
                {
                    candidates.swap(representative, current);
                }
            } else {
                if retained != current {
                    candidates.swap(retained, current);
                }
                retained += 1;
            }
        }
        candidates.truncate(retained);
        Ok(candidates.into_iter().map(|(_, _, call)| call).collect())
    }

    pub(in crate::server) fn semantic_outgoing(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<Vec<CallHierarchyOutgoingCall>> {
        let (view, caller) = self.semantic_hierarchy_target(item)?;
        let mut grouped = HashMap::<SemanticEntityId, Vec<Range>>::new();
        for call in view.calls_from(caller) {
            let SemanticTarget::Global(target) = call.target() else {
                continue;
            };
            if let Some(location) = Self::semantic_location(&view, call.site()) {
                grouped.entry(target).or_default().push(location.range);
            }
        }
        let mut result = Vec::with_capacity(grouped.len());
        for (target, mut ranges) in grouped {
            let Some(to) = Self::semantic_hierarchy_item(&view, target) else {
                continue;
            };
            ranges.sort_by_key(|range| {
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            });
            ranges.dedup();
            result.push(CallHierarchyOutgoingCall {
                to,
                from_ranges: ranges,
            });
        }
        result.sort_by(|left, right| {
            left.to
                .uri
                .as_str()
                .cmp(right.to.uri.as_str())
                .then_with(|| {
                    (
                        left.to.selection_range.start.line,
                        left.to.selection_range.start.character,
                    )
                        .cmp(&(
                            right.to.selection_range.start.line,
                            right.to.selection_range.start.character,
                        ))
                })
        });
        Ok(result)
    }

    pub(in crate::server) fn semantic_type_item(
        view: &SemanticView,
        entity: SemanticEntityId,
    ) -> Option<TypeHierarchyItem> {
        let item = Self::semantic_entity_item(view, entity)?;
        Some(TypeHierarchyItem {
            name: item.name,
            kind: item.kind,
            tags: None,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        })
    }

    pub(in crate::server) fn semantic_type_target(
        &self,
        item: &TypeHierarchyItem,
    ) -> Result<(Arc<SemanticView>, SemanticEntityId)> {
        self.semantic_handle_target(item.data.as_ref(), &item.uri, item.selection_range)
    }

    pub(in crate::server) fn semantic_binding_type(
        &self,
        document: &Document,
        symbol: analysis::SymbolId,
    ) -> Option<Location> {
        let annotation = document.syntax.binding_type_range(&document.text, symbol)?;
        let view = self.semantic_view(&document.uri)?;
        let file = view.file_id(&document.uri)?;
        view.occurrences_from(file, annotation.start)
            .take_while(|occurrence| occurrence.full_range.start < annotation.end)
            .find_map(|occurrence| {
                let SemanticTarget::Global(id) = occurrence.target else {
                    return None;
                };
                let entity = view.entity(id)?;
                (entity.kind == analysis::DeclarationKind::Type)
                    .then(|| Self::semantic_location(&view, entity.declaration))
                    .flatten()
            })
    }

    pub(in crate::server) async fn semantic_document(&self, params: Value) -> Result<Value> {
        let uri = params["textDocument"]["uri"]
            .as_str()
            .and_then(|uri| Url::parse(uri).ok())
            .ok_or_else(|| Error::invalid_params("textDocument.uri is required"))?;
        let root = match params.get("rootUri") {
            Some(Value::String(root)) => {
                Url::parse(root).map_err(|_| Error::invalid_params("invalid rootUri"))?
            }
            None => uri.clone(),
            _ => return Err(Error::invalid_params("rootUri must be a URI string")),
        };
        self.workspace.wait_for_discovery().await;
        let _read = self.document_read(&root).await;
        let view = self.semantic_view(&root).ok_or_else(|| {
            Error::invalid_params("rootUri must identify a live, prepared entry document")
        })?;
        let file = view
            .file_id(&uri)
            .ok_or_else(|| Error::invalid_params("document is not in the selected entry graph"))?;
        let document = view.document(file).ok_or_else(Error::content_modified)?;
        let mut metadata_complete = true;
        let declarations = document.syntax.declaration_sites().iter()
            .map(|metadata| -> Result<Value> {
                let declaration = view.declaration(file, metadata.symbol).ok_or_else(Error::internal_error)?;
                metadata_complete &= !metadata.uncertain;
                let symbol = document.syntax.symbol_by_id(declaration.symbol)
                    .ok_or_else(Error::internal_error)?;
                let entity = match declaration.target {
                    SemanticTarget::Global(id) => view.entity(id),
                    _ => None,
                };
                Ok(json!({
                    "id": declaration.symbol.0,
                    "name": document.syntax.name_text(&document.text, symbol.name),
                    "kind": declaration_kind(declaration.kind),
                    "range": super::super::adapters::range(document, metadata.full_range),
                    "nameRange": super::super::adapters::range(document, metadata.name_range),
                    "signatureRange": super::super::adapters::range(document, metadata.signature_range),
                    "bodyRange": metadata.body_range.map(|range| super::super::adapters::range(document, range)),
                    "documentationRange": metadata.doc_range.map(|range| super::super::adapters::range(document, range)),
                    "target": target_json(&view, declaration.target),
                    "signatureOwner": entity.and_then(|entity| Self::semantic_location(&view, entity.signature_owner)),
                    "bodyOwner": entity.and_then(|entity| entity.body_owner)
                        .and_then(|site| Self::semantic_location(&view, site)),
                    "uncertain": metadata.uncertain,
                }))
            }).collect::<Result<Vec<_>>>()?;
        let occurrences: Vec<_> = view
            .occurrences(file)
            .map(|occurrence| {
                json!({
                    "range": super::super::adapters::range(document, occurrence.site.range),
                    "fullRange": super::super::adapters::range(document, occurrence.full_range),
                    "role": reference_role(occurrence.role),
                    "inType": occurrence.in_type,
                    "target": target_json(&view, occurrence.target),
                    "enclosing": occurrence.enclosing.and_then(|id| view.entity(id))
                        .and_then(|entity| Self::semantic_location(&view, entity.declaration)),
                    "synthetic": occurrence.synthetic,
                    "uncertain": occurrence.uncertain,
                    "status": match occurrence.target {
                        SemanticTarget::Unresolved => "unresolved",
                        SemanticTarget::Ambiguous => "ambiguous",
                        SemanticTarget::Global(_) | SemanticTarget::Local(_) => "resolved",
                    },
                })
            })
            .collect();
        Ok(json!({
            "schemaVersion": 1, "rootUri": view.root_uri(), "viewId": view.serial(),
            "document": { "uri": uri, "version": document.revision.0,
                "contentHash": format!("fnv1a64:{:016x}", document.content_hash()) },
            "declarations": declarations, "occurrences": occurrences,
            "complete": metadata_complete && view.issues().is_empty() && view.occurrences(file).all(|occurrence|
                !occurrence.uncertain && !matches!(occurrence.target, SemanticTarget::Unresolved | SemanticTarget::Ambiguous)),
        }))
    }
}

fn completion_span(
    document: &Document,
    view: &SemanticView,
    file: FileId,
    offset: usize,
) -> analysis::TextRange {
    let syntax = &document.syntax;
    let mut token = syntax
        .token_at_or_before(offset)
        .and_then(|id| syntax.token(id));
    if token.is_some_and(|token| {
        token.range.start == offset
            && !matches!(
                token.kind,
                analysis::TokenKind::Identifier | analysis::TokenKind::Number
            )
    }) && offset > 0
    {
        let previous = syntax
            .token_at_or_before(offset - 1)
            .and_then(|id| syntax.token(id));
        if previous.is_some_and(|token| {
            token.range.end == offset
                && (matches!(
                    token.kind,
                    analysis::TokenKind::Identifier | analysis::TokenKind::Number
                ) || document.text.get(token.range.start..token.range.end) == Some("."))
        }) {
            token = previous;
        }
    }
    let Some(token) =
        token.filter(|token| token.range.start <= offset && offset <= token.range.end)
    else {
        return analysis::TextRange::new(offset, offset);
    };
    if matches!(
        token.kind,
        analysis::TokenKind::Identifier | analysis::TokenKind::Number
    ) {
        return view
            .resolve_at(file, token.range.start)
            .map_or(token.range, |occurrence| occurrence.full_range);
    }
    if document.text.get(token.range.start..token.range.end) == Some(".") {
        let start = view
            .resolve_at(file, token.range.start.saturating_sub(1))
            .map_or(token.range.start, |occurrence| occurrence.full_range.start);
        return analysis::TextRange::new(start, offset);
    }
    analysis::TextRange::new(offset, offset)
}

const fn hierarchy_site(entity: &SemanticEntity) -> SemanticSite {
    match entity.body_owner {
        Some(body) => body,
        None => entity.declaration,
    }
}

/// One incoming call candidate borrowing the entry view that published it.
type IncomingCallCandidate<'a> = (
    &'a SemanticView,
    SemanticEntityId,
    CallHierarchyIncomingCall,
);

fn range_order(range: &Range) -> (u32, u32, u32, u32) {
    (
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
    )
}

type IncomingCallerOrder<'a> = (&'a str, (u32, u32, u32, u32), (u32, u32, u32, u32), &'a str);

fn incoming_caller_order(call: &CallHierarchyIncomingCall) -> IncomingCallerOrder<'_> {
    let from = &call.from;
    (
        from.uri.as_str(),
        range_order(&from.selection_range),
        range_order(&from.range),
        from.name.as_str(),
    )
}

/// The caller presentation a client sees, independent of the opaque handle.
fn same_incoming_caller(
    left: &CallHierarchyIncomingCall,
    right: &CallHierarchyIncomingCall,
) -> bool {
    let left = &left.from;
    let right = &right.from;
    left.uri == right.uri
        && left.range == right.range
        && left.selection_range == right.selection_range
        && left.name == right.name
        && left.kind == right.kind
        && left.detail == right.detail
}

/// Compare the complete hierarchy continuation of two caller entities.
///
/// Callers are the same incoming call only when their physical declaration
/// metadata agrees and every indexed call matches at every depth, so an entry
/// that fills a claim differently is never merged away. The traversal only
/// walks `calls_from` in its existing source order; entity pairs are visited
/// once, which also terminates recursive call graphs.
fn same_hierarchy_expansion(
    left: &SemanticView,
    left_entity: SemanticEntityId,
    right: &SemanticView,
    right_entity: SemanticEntityId,
    stack: &mut Vec<(SemanticEntityId, SemanticEntityId)>,
    visited: &mut HashSet<(SemanticEntityId, SemanticEntityId)>,
) -> bool {
    stack.clear();
    visited.clear();
    stack.push((left_entity, right_entity));
    while let Some((left_id, right_id)) = stack.pop() {
        if !visited.insert((left_id, right_id)) {
            continue;
        }
        let (Some(left_entry), Some(right_entry)) = (left.entity(left_id), right.entity(right_id))
        else {
            return false;
        };
        if !same_hierarchy_entity(left, left_entry, right, right_entry) {
            return false;
        }
        let mut left_calls = left.calls_from(left_id);
        let mut right_calls = right.calls_from(right_id);
        loop {
            match (left_calls.next(), right_calls.next()) {
                (None, None) => break,
                (Some(left_call), Some(right_call)) => {
                    if left_call.site() != right_call.site() {
                        return false;
                    }
                    match (
                        visible_hierarchy_target(left, left_call.target()),
                        visible_hierarchy_target(right, right_call.target()),
                    ) {
                        (Some(left_target), Some(right_target)) => {
                            stack.push((left_target, right_target));
                        }
                        (None, None) => {}
                        _ => return false,
                    }
                }
                _ => return false,
            }
        }
    }
    true
}

/// The physical entity facts a hierarchy item exposes for one declaration.
fn same_hierarchy_entity(
    left: &SemanticView,
    left_entity: &SemanticEntity,
    right: &SemanticView,
    right_entity: &SemanticEntity,
) -> bool {
    left_entity.declaration == right_entity.declaration
        && left_entity.signature_owner == right_entity.signature_owner
        && left_entity.body_owner == right_entity.body_owner
        && left_entity.kind == right_entity.kind
        && hierarchy_declaration_text(left, left_entity)
            == hierarchy_declaration_text(right, right_entity)
}

/// The hierarchy item's name is the declaration text, not the entry-qualified
/// canonical name, which changes with each importing entry's namespace.
fn hierarchy_declaration_text<'a>(
    view: &'a SemanticView,
    entity: &SemanticEntity,
) -> Option<&'a str> {
    let document = view.document(entity.declaration.file)?;
    document
        .text
        .get(entity.declaration.range.start..entity.declaration.range.end)
}

/// `semantic_outgoing` only exposes global calls whose target has an item.
fn visible_hierarchy_target(
    view: &SemanticView,
    target: SemanticTarget,
) -> Option<SemanticEntityId> {
    let SemanticTarget::Global(id) = target else {
        return None;
    };
    let entity = view.entity(id)?;
    matches!(
        entity.kind,
        analysis::DeclarationKind::Def | analysis::DeclarationKind::Law
    )
    .then_some(id)
}

fn target_json(view: &SemanticView, target: SemanticTarget) -> Value {
    view.target_site(target)
        .and_then(|site| Backend::semantic_location(view, site))
        .map_or(Value::Null, |location| json!(location))
}

const fn declaration_kind(kind: analysis::DeclarationKind) -> &'static str {
    match kind {
        analysis::DeclarationKind::Def => "def",
        analysis::DeclarationKind::Law => "law",
        analysis::DeclarationKind::Type => "type",
        analysis::DeclarationKind::Constructor => "constructor",
    }
}

const fn reference_role(role: ReferenceRole) -> &'static str {
    match role {
        ReferenceRole::Value => "value",
        ReferenceRole::Type => "type",
        ReferenceRole::Constructor => "constructor",
        ReferenceRole::Call => "call",
        ReferenceRole::Offload => "offload",
        ReferenceRole::Binding => "binding",
    }
}
