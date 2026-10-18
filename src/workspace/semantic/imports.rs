use super::{FileId, WorkspaceDb};
use std::path::{Path, PathBuf};
use url::Url;

impl WorkspaceDb {
    pub(in crate::workspace) fn intern_import(
        &mut self,
        path: PathBuf,
        physical_path: PathBuf,
    ) -> Option<FileId> {
        let uri = Url::from_file_path(&path).ok()?;
        let open_owner = self
            .by_uri
            .get(&uri)
            .copied()
            .filter(|file| {
                let entry = &self.entries[file.0];
                entry.uri == uri && entry.open_snapshot.is_some()
            })
            .or_else(|| {
                self.by_path.get(&path).copied().filter(|file| {
                    let entry = &self.entries[file.0];
                    entry.path.as_ref() == Some(&path) && entry.open_snapshot.is_some()
                })
            });
        self.detach_import_display(&path, &physical_path);
        let existing = self.physical_files.get(&physical_path).and_then(|files| {
            files.iter().copied().min_by_key(|file| {
                let entry = &self.entries[file.0];
                (entry.uri != uri, entry.open_snapshot.is_some(), *file)
            })
        });
        let target = if let Some(existing) = existing {
            existing
        } else {
            if let Some(pending) = self.by_uri.get(&uri).copied().filter(|file| {
                let entry = &self.entries[file.0];
                entry.uri == uri && entry.snapshot().is_none() && entry.disk_generation == 0
            }) {
                self.update_physical_path(pending, Some(physical_path.clone()));
                self.by_path.entry(path).or_insert(pending);
                self.by_path.entry(physical_path).or_insert(pending);
                return Some(pending);
            }
            let target = self.allocate_file(uri.clone(), Some(path.clone()));
            self.update_physical_path(target, Some(physical_path.clone()));
            target
        };
        // These are lookup aliases only. Never change an existing source's URI
        // or overlay merely because another import spells its path differently.
        let lookup_target = open_owner.unwrap_or(target);
        self.by_uri.insert(uri, lookup_target);
        self.by_path.insert(path, lookup_target);
        self.by_path.entry(physical_path).or_insert(target);
        Some(target)
    }

    fn detach_import_display(&mut self, path: &Path, physical_path: &Path) {
        // A retargeted symlink is no longer a valid display URI for its old source.
        if let Some(previous) = self.by_path.get(path).copied()
            && self.entries[previous.0].open_snapshot.is_none()
            && self.entries[previous.0].snapshot().is_some()
            && self.entries[previous.0].path.as_deref() == Some(path)
            && let Some(physical) = self.entries[previous.0]
                .physical_path
                .as_ref()
                .filter(|physical| physical.as_path() != physical_path)
                .cloned()
            && let Ok(canonical_uri) = Url::from_file_path(&physical)
        {
            self.invalidate_semantic_file(previous);
            let old_uri =
                std::mem::replace(&mut self.entries[previous.0].uri, canonical_uri.clone());
            self.entries[previous.0].path = Some(physical.clone());
            if self.by_uri.get(&old_uri) == Some(&previous) {
                self.by_uri.remove(&old_uri);
            }
            if self.by_path.get(path) == Some(&previous) {
                self.by_path.remove(path);
            }
            self.by_uri.entry(canonical_uri).or_insert(previous);
            self.by_path.entry(physical).or_insert(previous);
            self.refresh_import_watch_paths(previous);
        }
    }
}
