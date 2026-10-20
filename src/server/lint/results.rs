use super::{LintResult, saved::SavedProject};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};
use url::Url;

#[derive(Default)]
pub(super) struct LintResults {
    entries: HashMap<Url, Arc<LintResult>>,
    documents: HashMap<Url, Arc<LintResult>>,
}

impl LintResults {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn get(&self, uri: &Url) -> Option<Arc<LintResult>> {
        self.documents.get(uri).cloned()
    }

    pub(super) fn saved_for(&self, path: &Path) -> Option<Arc<SavedProject>> {
        self.entries
            .values()
            .filter(|result| path.starts_with(&result.saved.root))
            .max_by_key(|result| result.saved.root.as_os_str().len())
            .map(|result| result.saved.clone())
    }

    pub(super) fn clear(&mut self) -> HashSet<Url> {
        let affected = self.documents.keys().cloned().collect();
        self.entries.clear();
        self.documents.clear();
        affected
    }

    pub(super) fn insert(&mut self, uri: Url, result: Arc<LintResult>) -> HashSet<Url> {
        let mut affected: HashSet<_> = result.findings.by_uri.keys().cloned().collect();
        affected.insert(uri.clone());
        if let Some(previous) = self.entries.insert(uri, result) {
            affected.extend(previous.findings.by_uri.keys().cloned());
        }
        self.rebuild();
        affected
    }

    pub(super) fn invalidate(
        &mut self,
        mut affected: impl FnMut(&Url, &LintResult) -> bool,
    ) -> HashSet<Url> {
        let mut changed = HashSet::new();
        let mut removed = false;
        self.entries.retain(|uri, result| {
            if affected(uri, result) {
                removed = true;
                changed.insert(uri.clone());
                changed.extend(result.findings.by_uri.keys().cloned());
                false
            } else {
                true
            }
        });
        if removed {
            self.rebuild();
        }
        changed
    }

    fn rebuild(&mut self) {
        self.documents.clear();
        for (entry, result) in &self.entries {
            for uri in std::iter::once(entry).chain(result.findings.by_uri.keys()) {
                let slot = self
                    .documents
                    .entry(uri.clone())
                    .or_insert_with(|| result.clone());
                if slot.epoch < result.epoch {
                    *slot = result.clone();
                }
            }
        }
    }
}
