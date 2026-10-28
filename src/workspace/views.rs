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

/// Editor metadata for one spelling. The representative's buffer is stored
/// beside this metadata on the file entry, never inside it, so the analysis
/// snapshot path does not walk this structure.
pub(super) struct OpenView {
    pub(super) uri: Arc<Url>,
    pub(super) id: ViewId,
    pub(super) language_id: String,
}

/// A non-representative spelling. Unlike the representative, whose buffer is
/// owned directly by the entry, an alias carries its own buffer.
pub(super) struct AliasView {
    pub(super) view: OpenView,
    pub(super) snapshot: Arc<DocumentSnapshot>,
}

impl AliasView {
    fn view_ref(&self) -> ViewRef<'_> {
        ViewRef {
            uri: &self.view.uri,
            id: self.view.id,
            language_id: &self.view.language_id,
            snapshot: &self.snapshot,
        }
    }
}

/// Borrowed identity of one open spelling, representative or alias.
#[derive(Clone, Copy)]
pub(super) struct ViewRef<'a> {
    pub(super) uri: &'a Url,
    pub(super) id: ViewId,
    pub(super) language_id: &'a str,
    pub(super) snapshot: &'a Arc<DocumentSnapshot>,
}

impl ViewRef<'_> {
    pub(super) fn document(self) -> Document {
        document_of(self.uri, self.language_id, self.snapshot)
    }
}

fn document_of(uri: &Url, language_id: &str, snapshot: &Arc<DocumentSnapshot>) -> Document {
    Document::with_snapshot(uri.clone(), language_id.to_owned(), snapshot.clone())
}

impl FileEntry {
    pub(super) fn snapshot(&self) -> Option<&Arc<DocumentSnapshot>> {
        self.open_snapshot.as_ref().or(self.disk_snapshot.as_ref())
    }

    pub(super) fn is_open(&self) -> bool {
        self.open_snapshot.is_some()
    }

    pub(super) fn view_count(&self) -> usize {
        usize::from(self.open_snapshot.is_some()) + self.open_aliases.as_ref().map_or(0, Vec::len)
    }

    /// Language of the representative spelling, if a buffer is open.
    pub(super) fn representative_language(&self) -> Option<&str> {
        self.open_view
            .as_ref()
            .map(|view| view.language_id.as_str())
    }

    pub(super) fn view(&self, uri: &Url) -> Option<ViewRef<'_>> {
        // A closed entry has no spelling metadata, so the common closed case
        // never touches the inline view slot.
        let snapshot = self.open_snapshot.as_ref()?;
        if let Some(view) = self.open_view.as_ref()
            && view.uri.as_ref() == uri
        {
            return Some(ViewRef {
                uri: &view.uri,
                id: view.id,
                language_id: &view.language_id,
                snapshot,
            });
        }
        self.open_aliases
            .as_ref()?
            .iter()
            .find(|alias| alias.view.uri.as_ref() == uri)
            .map(AliasView::view_ref)
    }

    pub(super) fn views(&self) -> impl Iterator<Item = ViewRef<'_>> {
        let representative = self.open_snapshot.as_ref().and_then(|snapshot| {
            let view = self.open_view.as_ref()?;
            Some(ViewRef {
                uri: &view.uri,
                id: view.id,
                language_id: &view.language_id,
                snapshot,
            })
        });
        representative
            .into_iter()
            .chain(self.open_aliases.iter().flatten().map(AliasView::view_ref))
    }

    // Every open spelling of this file, appended to the caller's output. The
    // physical representative comes first; additional spellings keep their
    // commit order. Gathering directly into an existing vector keeps the
    // owned documents out of iterator adapter state.
    pub(super) fn append_documents(&self, documents: &mut Vec<Document>) {
        if let Some(snapshot) = &self.open_snapshot
            && let Some(view) = &self.open_view
        {
            documents.push(document_of(&view.uri, &view.language_id, snapshot));
            for alias in self.open_aliases.iter().flatten() {
                documents.push(document_of(
                    &alias.view.uri,
                    &alias.view.language_id,
                    &alias.snapshot,
                ));
            }
            return;
        }
        // No spelling is open exactly when the saved text answers instead.
        if let Some(snapshot) = &self.disk_snapshot {
            documents.push(document_of(&self.uri, FALLBACK_LANGUAGE_ID, snapshot));
        }
    }

    // The representative view is the most recently committed spelling; the
    // import graph, compiler, and dependent refresh use its text so one file
    // still has exactly one deterministic analysis text.
    pub(super) fn document(&self) -> Option<Document> {
        if let Some(snapshot) = &self.open_snapshot
            && let Some(view) = &self.open_view
        {
            return Some(document_of(&view.uri, &view.language_id, snapshot));
        }
        Some(document_of(
            &self.uri,
            FALLBACK_LANGUAGE_ID,
            self.disk_snapshot.as_ref()?,
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

    fn take_view(&mut self, uri: &Url) -> Option<AliasView> {
        if self
            .open_view
            .as_ref()
            .is_some_and(|view| view.uri.as_ref() == uri)
        {
            let view = self.open_view.take();
            let snapshot = self.open_snapshot.take();
            return view
                .zip(snapshot)
                .map(|(view, snapshot)| AliasView { view, snapshot });
        }
        let aliases = self.open_aliases.as_mut()?;
        let position = aliases
            .iter()
            .position(|alias| alias.view.uri.as_ref() == uri)?;
        Some(aliases.remove(position))
    }

    // Insert or replace one spelling and make it the representative without
    // ever dropping another buffer.
    pub(super) fn insert_view(&mut self, view: OpenView, snapshot: Arc<DocumentSnapshot>) {
        let _replaced = self.take_view(&view.uri);
        if let (Some(previous), Some(previous_snapshot)) =
            (self.open_view.take(), self.open_snapshot.take())
        {
            self.open_aliases
                .get_or_insert_with(Vec::new)
                .push(AliasView {
                    view: previous,
                    snapshot: previous_snapshot,
                });
        }
        self.open_view = Some(view);
        self.open_snapshot = Some(snapshot);
    }

    pub(super) fn remove_view(&mut self, uri: &Url) -> Option<AliasView> {
        let removed = self.take_view(uri)?;
        if self.open_snapshot.is_none() {
            let promoted = self.open_aliases.as_mut().and_then(Vec::pop);
            if let Some(promoted) = promoted {
                self.open_view = Some(promoted.view);
                self.open_snapshot = Some(promoted.snapshot);
            }
        }
        if self.open_aliases.as_ref().is_some_and(Vec::is_empty) {
            self.open_aliases = None;
        }
        Some(removed)
    }
}
