use std::collections::BTreeMap;

use super::{FileId, SemanticCompletion, SemanticFileContext, SemanticTarget, SemanticView};
use crate::analysis::DeclarationKind;

impl SemanticView {
    /// Project canonical Book names into this file's written namespace. Locals
    /// remain the frontend's lexical completion contribution.
    #[must_use]
    pub fn completion_candidates(&self, file: FileId, prefix: &str) -> Vec<SemanticCompletion> {
        let Some(context) = self.contexts.get(&file) else {
            return Vec::new();
        };
        let mut candidates = BTreeMap::new();
        if let Some((alias, suffix)) = prefix.split_once('.')
            && let Some(namespace) = context.aliases.get(alias)
        {
            self.completion_range(context, namespace, suffix, Some(alias), &mut candidates);
            for (name, target) in &mut candidates {
                if self.names.get(name.as_str()).is_some_and(|global| {
                    self.target_available(context, *global) && global != target
                }) {
                    *target = SemanticTarget::Ambiguous;
                }
            }
        } else {
            self.completion_range(context, &context.namespace, prefix, None, &mut candidates);
            if !context.namespace.is_empty() {
                let mut globals = BTreeMap::new();
                self.completion_range(context, "", prefix, None, &mut globals);
                for (name, target) in globals {
                    candidates.entry(name).or_insert(target);
                }
            }
        }
        candidates
            .into_iter()
            .map(|(name, target)| SemanticCompletion { name, target })
            .collect()
    }

    fn target_available(&self, context: &SemanticFileContext, target: SemanticTarget) -> bool {
        match target {
            SemanticTarget::Global(id) => {
                id.view == self.serial() && id.index < context.available_end
            }
            _ => false,
        }
    }

    fn completion_range(
        &self,
        context: &SemanticFileContext,
        namespace: &str,
        prefix: &str,
        alias: Option<&str>,
        candidates: &mut BTreeMap<String, SemanticTarget>,
    ) {
        let namespace_end = usize::from(!namespace.is_empty()) + namespace.len();
        let (qualifier, query) = prefix
            .rsplit_once('.')
            .map_or(("", prefix), |(qualifier, query)| {
                (&prefix[..=qualifier.len()], query)
            });
        let mut lookup_prefix = String::with_capacity(namespace_end + qualifier.len());
        if !namespace.is_empty() {
            lookup_prefix.push_str(namespace);
            lookup_prefix.push(':');
        }
        lookup_prefix.push_str(qualifier);
        let Some(indices) = self.namespace_indices.get(namespace) else {
            return;
        };
        let first = indices
            .partition_point(|index| self.entities[*index].name.as_str() < lookup_prefix.as_str());
        for index in &indices[first..] {
            let entity = &self.entities[*index];
            if !entity.name.starts_with(&lookup_prefix) {
                break;
            }
            if entity.id.index >= context.available_end {
                continue;
            }
            let Some(written) = entity.name.get(namespace_end..) else {
                continue;
            };
            // A prefix lookup in the global namespace must not expose internal
            // module-qualified keys such as `dep:target`.
            if written.contains(':') {
                continue;
            }
            let Some(suffix) = written.strip_prefix(qualifier) else {
                continue;
            };
            if crate::analysis::completion_match(suffix, query).is_none() {
                continue;
            }
            let label =
                alias.map_or_else(|| written.to_owned(), |alias| format!("{alias}.{written}"));
            let target = SemanticTarget::Global(entity.id);
            candidates
                .entry(label)
                .and_modify(|existing| {
                    if *existing != target {
                        let old_kind = match *existing {
                            SemanticTarget::Global(id) => self.entity(id).map(|entity| entity.kind),
                            _ => None,
                        };
                        *existing = match (old_kind, entity.kind) {
                            (Some(DeclarationKind::Constructor), kind)
                                if kind != DeclarationKind::Constructor =>
                            {
                                target
                            }
                            (Some(kind), DeclarationKind::Constructor)
                                if kind != DeclarationKind::Constructor =>
                            {
                                *existing
                            }
                            _ => SemanticTarget::Ambiguous,
                        };
                    }
                })
                .or_insert(target);
        }
    }
}
