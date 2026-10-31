use super::{
    FileId, SemanticBindingId, SemanticEntityId, SemanticFileContext, SemanticTarget, SemanticView,
};
use crate::analysis::{DeclarationKind, IndexedBinding, ReferenceRole, SyntaxIndex, TextRange};

impl SemanticView {
    /// Simulate replacing one file-local import alias using immutable name and
    /// scope facts. The caller compares the resulting ranges across entry views.
    #[must_use]
    pub fn alias_rename_ranges(
        &self,
        file: FileId,
        alias_range: TextRange,
        new_name: &str,
    ) -> Option<Vec<TextRange>> {
        // book_load accepts one ASCII alias word, independently of the term
        // parser's keyword set (including contextual `as` and `unsafe`).
        let bytes = new_name.as_bytes();
        if !bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return None;
        }
        if self.capture.conflicted {
            return None;
        }
        let document = self.document(file)?;
        let context = self.contexts.get(&file)?;
        let import = document
            .syntax
            .imports()
            .iter()
            .find(|import| import.valid_syntax && import.alias == Some(alias_range))?;
        let alias = import.alias_text(&document.text)?;
        let namespace = context.alias(alias);
        if document.syntax.imports().iter().any(|other| {
            other.valid_syntax
                && other.alias != Some(alias_range)
                && (other.alias_text(&document.text) == Some(alias)
                    || other.alias_text(&document.text) == Some(new_name))
        }) {
            return None;
        }
        let mut ranges = vec![alias_range];
        for occurrence in self.occurrences(file) {
            let written = document
                .text
                .get(occurrence.full_range.start..occurrence.full_range.end)?;
            let Some((prefix, suffix)) = written.split_once('.') else {
                continue;
            };
            if prefix != alias && prefix != new_name {
                continue;
            }
            // Bindings are complete Bend atoms: `D` does not shadow `D.target`,
            // while an actual binder for `D.target` does.
            if matches!(occurrence.target, SemanticTarget::Local(_)) {
                continue;
            }
            let complete_declaration =
                self.has_complete_declaration_spelling(occurrence.target, written)?;
            // A newly introduced alias also reserves its dotted declaration
            // prefix: parse_fresh rejects `E.anything`, not just exported names.
            if complete_declaration
                && occurrence.role == ReferenceRole::Binding
                && prefix == new_name
                && alias != new_name
            {
                return None;
            }
            let rewritten = prefix == alias && !complete_declaration;
            if occurrence.uncertain || occurrence.target == SemanticTarget::Ambiguous {
                return None;
            }
            let replacement = rewritten.then(|| format!("{new_name}.{suffix}"));
            let proposed_name = replacement.as_deref().unwrap_or(written);
            if rewritten
                && document
                    .syntax
                    .name_id(&document.text, proposed_name)
                    .is_some_and(|name| {
                        document
                            .syntax
                            .binding_named_at(name, occurrence.full_range.start)
                            .is_some()
                    })
            {
                return None;
            }
            // A fresh declaration's own complete spelling is not an alias use.
            // Uses and law fills must retain their original semantic entity.
            if occurrence.role != ReferenceRole::Binding || !complete_declaration {
                let proposed = self.resolve_renamed_alias(
                    context,
                    file,
                    occurrence.full_range.start,
                    proposed_name,
                    occurrence.role,
                    (alias, new_name, namespace),
                );
                if proposed != occurrence.target {
                    return None;
                }
            }
            if rewritten {
                ranges.push(TextRange::new(
                    occurrence.full_range.start,
                    occurrence.full_range.start + alias.len(),
                ));
            }
        }
        ranges.sort_unstable_by_key(|range| (range.start, range.end));
        ranges.dedup();
        Some(ranges)
    }

    fn has_complete_declaration_spelling(
        &self,
        target: SemanticTarget,
        written: &str,
    ) -> Option<bool> {
        let SemanticTarget::Global(id) = target else {
            return Some(false);
        };
        let entity = self.entity(id)?;
        let source = self.document(entity.declaration.file)?;
        Some(
            source
                .text
                .get(entity.declaration.range.start..entity.declaration.range.end)
                == Some(written),
        )
    }

    fn resolve_renamed_alias(
        &self,
        context: &SemanticFileContext,
        file: FileId,
        offset: usize,
        written: &str,
        role: ReferenceRole,
        aliases: (&str, &str, Option<&str>),
    ) -> SemanticTarget {
        let (old_alias, new_alias, namespace) = aliases;
        let qualify = |namespace: &str, name: &str| {
            if namespace.is_empty() {
                name.to_owned()
            } else {
                format!("{namespace}:{name}")
            }
        };
        let mut qualified = qualify(&context.namespace, written);
        if let Some((prefix, suffix)) = written.split_once('.') {
            let alias_namespace = if prefix == new_alias {
                namespace
            } else if prefix == old_alias {
                None
            } else {
                context.alias(prefix)
            };
            if let Some(namespace) = alias_namespace {
                qualified = qualify(namespace, suffix);
                if qualified != written
                    && self.alias_name_available(context, file, offset, &qualified)
                    && self.alias_name_available(context, file, offset, written)
                {
                    return SemanticTarget::Ambiguous;
                }
            }
        }
        if !self.alias_name_available(context, file, offset, &qualified)
            && self.alias_name_available(context, file, offset, written)
        {
            written.clone_into(&mut qualified);
        }
        if role == ReferenceRole::Constructor {
            // The constructor table holds exactly the constructor entities.
            // Alias refactoring already refused conflicting overlays above, so
            // no `Ambiguous` registration can reach this file.
            return match self.constructors.get(qualified.as_str()) {
                Some(SemanticTarget::Global(id)) => SemanticTarget::Global(*id),
                _ => SemanticTarget::Unresolved,
            };
        }
        self.top_level(&qualified)
            .filter(|target| match target {
                SemanticTarget::Global(id) => self
                    .entity(*id)
                    .is_some_and(|entity| entity.kind != DeclarationKind::Constructor),
                _ => true,
            })
            .unwrap_or(SemanticTarget::Unresolved)
    }

    fn alias_name_available(
        &self,
        context: &SemanticFileContext,
        file: FileId,
        offset: usize,
        name: &str,
    ) -> bool {
        let Some(SemanticTarget::Global(id)) = self.top_level(name) else {
            return false;
        };
        let Some(entity) = self.entity(id) else {
            return false;
        };
        if entity.id.index >= context.available_end {
            return false;
        }
        if entity.declaration.file != file {
            return true;
        }
        let Some(document) = self.document(file) else {
            return false;
        };
        let declaration = document
            .syntax
            .token_at_or_before(entity.declaration.range.start)
            .and_then(|token| document.syntax.symbol_for_token(token))
            .and_then(|symbol| document.syntax.declaration_site(symbol));
        declaration.is_some_and(|declaration| {
            let registered = if declaration.kind == DeclarationKind::Law {
                declaration.full_range.end
            } else {
                declaration
                    .body_range
                    .map_or(declaration.signature_range.end, |body| body.start)
            };
            registered <= offset
        })
    }

    /// Reject spellings the pinned frontend cannot declare before any
    /// name-availability fast path can accept them.
    fn new_atom_is_valid(&self, target: SemanticTarget, new_name: &str) -> bool {
        // `parse_lexeme`/`parse_name`: ASCII words joined by single dots.
        if !is_complete_atom(new_name) {
            return false;
        }
        match target {
            // A local rename replaces its whole written atom, so the new name
            // itself must not be a keyword or the `_` wildcard.
            SemanticTarget::Local(_) => !is_standalone_name(new_name),
            SemanticTarget::Global(id) => {
                // A global rename replaces only a written terminal: a dotted
                // atom keeps its leading words, and `parse_name` rejects a
                // keyword (or `parse_open` drops `_`) only for a standalone
                // atom. `Foo.def` and `Foo._` remain complete Bend names.
                if is_standalone_name(new_name) && !self.written_atoms_stay_dotted(target) {
                    return false;
                }
                !self.global_name_starts_with_alias(id, new_name)
            }
            SemanticTarget::Unresolved | SemanticTarget::Ambiguous => true,
        }
    }

    /// Every written occurrence keeps a dotted prefix, so a renamed terminal
    /// can only become a name segment, never a standalone keyword or wildcard.
    fn written_atoms_stay_dotted(&self, target: SemanticTarget) -> bool {
        let references = self.references(target);
        if references.is_empty() {
            return false;
        }
        for occurrence in references {
            let Some(document) = self.document(occurrence.site.file) else {
                return false;
            };
            let Some(written) = document
                .text
                .get(occurrence.full_range.start..occurrence.full_range.end)
            else {
                return false;
            };
            if !written.contains('.') {
                return false;
            }
        }
        true
    }

    /// A declaration's first word may not be an import's alias: `parse_fresh`
    /// rejects it, and `parse_reso` would route every use to the imported file.
    fn global_name_starts_with_alias(&self, id: SemanticEntityId, new_name: &str) -> bool {
        let Some(entity) = self.entity(id) else {
            return false;
        };
        let Some(context) = self.contexts.get(&entity.declaration.file) else {
            return false;
        };
        // The canonical name prefixes the namespace with ':'; the written
        // declaration keeps only the bare dotted name.
        let bare = entity
            .name
            .rsplit_once(':')
            .map_or(entity.name.as_str(), |(_, bare)| bare);
        let first = if bare.contains('.') {
            // A dotted declaration keeps its leading words.
            first_word(bare)
        } else if new_name.contains('.') {
            first_word(new_name)
        } else {
            return false;
        };
        context.alias(first).is_some()
    }

    /// Reject spellings the pinned frontend cannot declare, then check
    /// collisions and lexical capture using the already lowered facts.
    /// Cross-entry conflicts must additionally be checked with `rename_conflicts`.
    #[must_use]
    pub fn rename_is_safe(&self, target: SemanticTarget, new_name: &str) -> bool {
        if !self.new_atom_is_valid(target, new_name) {
            return false;
        }
        if self.capture.conflicted {
            return false;
        }
        if self.references(target).iter().any(|occurrence| {
            self.occurrences_from(occurrence.site.file, occurrence.full_range.start)
                .take_while(|entry| entry.full_range.start == occurrence.full_range.start)
                .any(|entry| entry.site.range == occurrence.site.range && entry.target != target)
        }) {
            return false;
        }
        match target {
            SemanticTarget::Local(id) => self.local_rename_is_safe(id, new_name),
            SemanticTarget::Global(id) => {
                let Some(entity) = self.entity(id) else {
                    return false;
                };
                if entity.base {
                    return false;
                }
                let boundary = entity.name.rfind([':', '.']).map_or(0, |index| index + 1);
                let mut proposed = String::with_capacity(boundary + new_name.len());
                proposed.push_str(&entity.name[..boundary]);
                proposed.push_str(new_name);
                if self
                    .top_level(&proposed)
                    .is_some_and(|found| found != target)
                {
                    return false;
                }
                let written_name = proposed
                    .rsplit_once(':')
                    .map_or(proposed.as_str(), |(_, name)| name);
                if self
                    .top_level(written_name)
                    .is_some_and(|found| found != target)
                {
                    return false;
                }
                self.references(target).iter().all(|occurrence| {
                    if occurrence.uncertain {
                        return false;
                    }
                    let Some(document) = self.document(occurrence.site.file) else {
                        return false;
                    };
                    let Some(written) = document
                        .text
                        .get(occurrence.full_range.start..occurrence.full_range.end)
                    else {
                        return false;
                    };
                    let boundary = written.rfind('.').map_or(0, |index| index + 1);
                    let mut replacement = String::with_capacity(boundary + new_name.len());
                    replacement.push_str(&written[..boundary]);
                    replacement.push_str(new_name);
                    let Some(name) = document.syntax.name_id(&document.text, &replacement) else {
                        return true;
                    };
                    document
                        .syntax
                        .indexed_references_named(name)
                        .filter(|reference| reference.role == ReferenceRole::Binding)
                        .filter_map(|reference| reference.local_binding)
                        .filter_map(|binding| document.syntax.bindings().get(binding.0))
                        .all(|binding| !binding.scope.contains(occurrence.full_range.start))
                })
            }
            SemanticTarget::Unresolved | SemanticTarget::Ambiguous => false,
        }
    }

    fn local_rename_is_safe(&self, id: SemanticBindingId, new_name: &str) -> bool {
        if id.view != self.serial() {
            return false;
        }
        let Some(document) = self.document(id.file) else {
            return false;
        };
        let Some(binding) = document.syntax.binding_by_id(id.symbol) else {
            return false;
        };
        if document
            .syntax
            .scopes()
            .get(binding.scope_id.0)
            .is_none_or(|scope| scope.uncertain)
        {
            return false;
        }
        if document.syntax.name_text(&document.text, binding.name) == new_name {
            return true;
        }
        let Some(name) = document.syntax.name_id(&document.text, new_name) else {
            return true;
        };
        let target = SemanticTarget::Local(id);
        // A selected use must continue to choose this binder after its spelling changes.
        for occurrence in self.references(target) {
            if occurrence.uncertain {
                return false;
            }
            if occurrence.role == ReferenceRole::Binding {
                continue;
            }
            for reference in document
                .syntax
                .indexed_references_named(name)
                .filter(|reference| reference.role == ReferenceRole::Binding)
            {
                let Some(other) = reference
                    .local_binding
                    .and_then(|other| document.syntax.bindings().get(other.0))
                else {
                    continue;
                };
                if other.scope.contains(occurrence.full_range.start) && !shadows(binding, other) {
                    return false;
                }
            }
        }
        // Existing bare uses of the new spelling must not be captured by this binder.
        document
            .syntax
            .indexed_references_named(name)
            .all(|reference| {
                if reference.role == ReferenceRole::Binding
                    || !binding.scope.contains(reference.full_range.start)
                {
                    return true;
                }
                if document
                    .syntax
                    .name_text(&document.text, reference.full_name)
                    != new_name
                {
                    return true;
                }
                reference
                    .local_binding
                    .and_then(|other| document.syntax.bindings().get(other.0))
                    .is_some_and(|other| shadows(other, binding))
            })
    }
}

/// A complete spelling that is a keyword or the `_` wildcard atom.
fn is_standalone_name(name: &str) -> bool {
    SyntaxIndex::is_keyword_text(name) || name == "_"
}

/// The leading dot-separated word of a complete name (`Foo` in `Foo.target`).
fn first_word(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// The pinned compiler's `parse_lexeme` spelling of one complete name:
/// ASCII words joined by single dots (`Foo.target`, `x`).
fn is_complete_atom(name: &str) -> bool {
    name.split('.').all(|word| {
        let mut bytes = word.bytes();
        let Some(head) = bytes.next() else {
            return false;
        };
        if !head.is_ascii_alphabetic() && head != b'_' {
            return false;
        }
        bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn shadows(inner: &IndexedBinding, outer: &IndexedBinding) -> bool {
    inner.scope.start >= outer.scope.start
        && inner.scope.end <= outer.scope.end
        && (inner.scope != outer.scope || inner.name_range.start > outer.name_range.start)
}
