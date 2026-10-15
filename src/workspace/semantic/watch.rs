use super::{Document, FileId, SemanticViewInput, WorkspaceDb};
use crate::{analysis::DocumentSnapshot, workspace::PreparedImportTarget};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

impl WorkspaceDb {
    #[must_use]
    pub fn importer_documents_for_path(&self, path: &Path) -> Vec<Document> {
        let path = crate::workspace::normalize_path(path);
        let mut sources: std::collections::HashSet<_> = self
            .import_path_dependents
            .get(&path)
            .into_iter()
            .flat_map(|files| files.iter().copied())
            .collect();
        let mut targets = std::collections::HashSet::new();
        if let Some(file) = self.by_path.get(&path) {
            targets.insert(*file);
        }
        for source in &sources {
            let entry = &self.entries[source.0];
            let Some(snapshot) = entry.snapshot() else {
                continue;
            };
            for edge in &entry.imports {
                let Some(imported) = snapshot.text.get(edge.path.start..edge.path.end) else {
                    continue;
                };
                if [entry.path.as_deref(), entry.physical_path.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter_map(Path::parent)
                    .any(|directory| {
                        crate::workspace::normalize_path(&directory.join(imported)) == path
                    })
                {
                    targets.insert(edge.target);
                }
            }
        }
        for target in targets {
            let physical = self.entries[target.0]
                .physical_path
                .as_ref()
                .and_then(|path| self.physical_files.get(path));
            for file in std::iter::once(target)
                .chain(physical.into_iter().flat_map(|files| files.iter().copied()))
            {
                sources.extend(self.entries[file.0].reverse_imports.iter().copied());
            }
        }
        let mut documents: Vec<_> = sources
            .into_iter()
            .filter(|file| self.is_needed(*file))
            .filter_map(|file| self.entries[file.0].document())
            .collect();
        documents.sort_unstable_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
        documents
    }

    /// Synchronous cold import refresh, for callers without a staged lifecycle.
    pub fn refresh_imports(
        &mut self,
        file: FileId,
        expected: &Arc<DocumentSnapshot>,
    ) -> Option<bool> {
        let path = self.entries.get(file.0)?.path.clone()?;
        let physical = crate::workspace::canonical_source_path(&path);
        let imports = crate::workspace::resolve_import_targets(&path, expected);
        self.refresh_imports_prepared(file, expected, &physical, imports)
    }

    pub(crate) fn update_source_identity_prepared(
        &mut self,
        file: FileId,
        expected: &Arc<DocumentSnapshot>,
        physical_path: &Path,
    ) -> Option<bool> {
        if !self.is_needed(file) || !Arc::ptr_eq(self.entries.get(file.0)?.snapshot()?, expected) {
            return None;
        }
        let changed = self.entries[file.0].physical_path.as_deref() != Some(physical_path);
        // A closed imported source owns a physical file, not a retargetable
        // display alias. Its importers will intern the newly resolved target.
        if changed && self.entries[file.0].open_snapshot.is_none() {
            return None;
        }
        if changed {
            self.update_physical_path(file, Some(physical_path.to_path_buf()));
        }
        Some(changed)
    }

    /// Commit an off-lock re-resolution of existing cold import facts.
    pub(crate) fn refresh_imports_prepared(
        &mut self,
        file: FileId,
        expected: &Arc<DocumentSnapshot>,
        physical_path: &Path,
        imports: Vec<PreparedImportTarget>,
    ) -> Option<bool> {
        let identity_changed =
            self.update_source_identity_prepared(file, expected, physical_path)?;
        let changed = self.refresh_imports_with_targets(file, imports);
        if changed {
            // Re-resolution changes graph identity, not the snapshot owning
            // the editor path facts just published by this same preparation.
            let editor_paths = self.editor_import_paths.remove(&file);
            self.invalidate_semantic_file(file);
            if let Some(paths) = editor_paths {
                self.editor_import_paths.insert(file, paths);
            }
        }
        Some(changed || identity_changed)
    }

    pub(in crate::workspace) fn clear_import_watch_paths(&mut self, file: FileId) {
        self.semantic_base_importers.remove(&file);
        for path in std::mem::take(&mut self.entries[file.0].import_watch_paths) {
            if self
                .import_path_dependents
                .get_mut(&path)
                .is_some_and(|files| {
                    files.remove(&file);
                    files.is_empty()
                })
            {
                self.import_path_dependents.remove(&path);
            }
        }
    }

    pub(in crate::workspace) fn refresh_import_watch_paths(&mut self, file: FileId) {
        let entry = &self.entries[file.0];
        let mut paths = Vec::new();
        let imports_base = entry.snapshot().is_some_and(|snapshot| {
            snapshot.syntax.imports().iter().any(|import| {
                import.valid_syntax
                    && import.alias.is_none()
                    && import.path_text(&snapshot.text) == "Base"
            })
        });
        if let Some(snapshot) = entry.snapshot() {
            let physical_directory = entry.physical_path.as_deref().and_then(Path::parent);
            let logical_directory = entry.path.as_deref().and_then(Path::parent);
            for directory in physical_directory
                .into_iter()
                .chain(logical_directory.filter(|directory| Some(*directory) != physical_directory))
            {
                for import in snapshot.syntax.imports() {
                    let imported = import.path_text(&snapshot.text);
                    if !imported.is_empty()
                        && imported != "Base"
                        && !crate::workspace::is_hub_import_path(imported)
                    {
                        let path = crate::workspace::normalize_path(&directory.join(imported));
                        if path.extension().is_none() {
                            paths.push(path.with_extension("bend"));
                        }
                        paths.push(path);
                    }
                }
            }
        }
        if let Some(prepared) = self.editor_import_paths.get(&file) {
            paths.extend(prepared.keys().cloned());
        }
        paths.sort_unstable();
        paths.dedup();
        if paths.as_slice() != entry.import_watch_paths.as_ref() {
            self.clear_import_watch_paths(file);
            for path in &paths {
                self.import_path_dependents
                    .entry(path.clone())
                    .or_default()
                    .insert(file);
            }
            self.entries[file.0].import_watch_paths = paths.into_boxed_slice();
        }
        if imports_base {
            self.semantic_base_importers.insert(file);
        } else {
            self.semantic_base_importers.remove(&file);
        }
    }

    /// The caller resolves this configuration path outside workspace locks.
    pub fn set_semantic_library_path(&mut self, path: Option<PathBuf>) {
        if self.semantic_library_path == path {
            return;
        }
        let previous = std::mem::replace(&mut self.semantic_library_path, path);
        let affected: Vec<_> = self
            .semantic_views
            .iter()
            .filter_map(|(root, view)| {
                view.input
                    .library_affected(previous.as_deref(), self.semantic_library_path.as_deref())
                    .then_some(*root)
            })
            .collect();
        for root in affected {
            self.remove_semantic_view(root);
        }
    }
}

impl SemanticViewInput {
    pub(super) fn library_affected(&self, previous: Option<&Path>, next: Option<&Path>) -> bool {
        self.physical_generations.keys().any(|path| {
            previous.is_some_and(|root| path.starts_with(root))
                || next.is_some_and(|root| path.starts_with(root))
        }) || self.files.iter().any(|file| {
            file.path.as_deref().is_some_and(|path| {
                previous.is_some_and(|root| path.starts_with(root))
                    || next.is_some_and(|root| path.starts_with(root))
            }) || file.document.as_ref().is_some_and(|document| {
                document.syntax.imports().iter().any(|import| {
                    import.valid_syntax
                        && crate::workspace::is_hub_import_path(import.path_text(&document.text))
                })
            })
        })
    }
}
