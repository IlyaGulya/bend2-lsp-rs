use super::{Document, FALLBACK_LANGUAGE_ID, FileEntry};
use crate::analysis::DocumentSnapshot;
use std::sync::Arc;
use url::Url;
/// Identity of one open editor view, i.e. one URI spelling of a file.
///
/// `FileId` remains the normalized lexical path identity shared by every
/// spelling. A view adds the independent buffer text and LSP version stream of
/// that spelling; revisions and close epochs are tracked per view so a buffer
/// at version 4 and another spelling of the same file at version 11 never
/// order against each other.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ViewId(pub(super) u64);

pub(super) struct OpenView {
    pub(super) uri: Url,
    pub(super) id: ViewId,
    pub(super) language_id: String,
    pub(super) snapshot: Arc<DocumentSnapshot>,
}
impl FileEntry {
    pub(super) fn snapshot(&self) -> Option<&Arc<DocumentSnapshot>> {
        match &self.open_view {
            Some(view) => Some(&view.snapshot),
            None => self.disk_snapshot.as_ref(),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open_view.is_some()
    }

    pub(super) fn view_count(&self) -> usize {
        usize::from(self.open_view.is_some()) + self.open_aliases.as_ref().map_or(0, Vec::len)
    }

    pub(super) fn view(&self, uri: &Url) -> Option<&OpenView> {
        self.open_view
            .as_ref()
            .filter(|view| view.uri == *uri)
            .or_else(|| {
                self.open_aliases
                    .as_ref()?
                    .iter()
                    .find(|view| view.uri == *uri)
            })
    }

    pub(super) fn views(&self) -> impl Iterator<Item = &OpenView> {
        self.open_view
            .iter()
            .chain(self.open_aliases.iter().flat_map(|aliases| aliases.iter()))
    }

    pub(super) fn view_document(view: &OpenView) -> Document {
        Document::with_snapshot(
            view.uri.clone(),
            view.language_id.clone(),
            view.snapshot.clone(),
        )
    }

    // Every open spelling of this file. The physical representative comes
    // first; additional spellings keep their commit order. Yields lazily so a
    // dependents sweep never materializes one throwaway Vec per file.
    pub(super) fn documents(&self) -> impl Iterator<Item = Document> + '_ {
        // No spelling is open exactly when the saved text answers instead.
        let saved = if self.open_view.is_some() {
            None
        } else {
            self.document()
        };
        self.views().map(Self::view_document).chain(saved)
    }

    // The representative view is the most recently committed spelling; the
    // import graph, compiler, and dependent refresh use its text so one file
    // still has exactly one deterministic analysis text.
    pub(super) fn document(&self) -> Option<Document> {
        if let Some(view) = &self.open_view {
            return Some(Self::view_document(view));
        }
        Some(Document::with_snapshot(
            self.uri.clone(),
            FALLBACK_LANGUAGE_ID.into(),
            self.disk_snapshot.as_ref()?.clone(),
        ))
    }

    // Multiple spellings that agree share one analysis text; disagreeing
    // spellings are ambiguous and must never be resolved by picking one.
    pub(super) fn conflicting_views(entry: &FileEntry) -> Option<Vec<Url>> {
        let mut views = entry.views();
        let first = views.next()?;
        let mut uris = vec![first.uri.clone()];
        let mut conflict = false;
        for view in views {
            uris.push(view.uri.clone());
            conflict = conflict || view.snapshot.text != first.snapshot.text;
        }
        conflict.then(|| {
            uris.sort_unstable();
            uris
        })
    }

    pub(super) fn take_view(&mut self, uri: &Url) -> Option<OpenView> {
        if self.open_view.as_ref().is_some_and(|view| view.uri == *uri) {
            return self.open_view.take();
        }
        let aliases = self.open_aliases.as_mut()?;
        let position = aliases.iter().position(|view| view.uri == *uri)?;
        Some(aliases.remove(position))
    }

    // Insert or replace one spelling and make it the representative without
    // ever dropping another buffer.
    pub(super) fn insert_view(&mut self, view: OpenView) {
        let _replaced = self.take_view(&view.uri);
        if let Some(previous) = self.open_view.take() {
            self.open_aliases
                .get_or_insert_with(Vec::new)
                .push(previous);
        }
        self.open_view = Some(view);
    }

    pub(super) fn remove_view(&mut self, uri: &Url) -> Option<OpenView> {
        let removed = self.take_view(uri)?;
        if self.open_view.is_none()
            && let Some(aliases) = &mut self.open_aliases
        {
            self.open_view = aliases.pop();
        }
        if self.open_aliases.as_ref().is_some_and(Vec::is_empty) {
            self.open_aliases = None;
        }
        Some(removed)
    }
}
