use super::{FileId, WorkspaceDb};
use crate::workspace::{PhysicalAliases, PhysicalFile, physical_source_uri};
use std::path::{Path, PathBuf};

impl WorkspaceDb {
    pub(in crate::workspace) fn update_physical_path(
        &mut self,
        file: FileId,
        path: Option<PathBuf>,
    ) {
        if self.entries[file.0].physical_path == path {
            return;
        }
        let physical_uri = {
            let entry = &self.entries[file.0];
            let previous = entry.physical_path.as_deref().or(entry.path.as_deref());
            let next = path.as_deref().or(entry.path.as_deref());
            (previous != next).then(|| physical_source_uri(&entry.uri, next))
        };
        self.invalidate_semantic_file(file);
        if let Some(previous) = self.entries[file.0].physical_path.take()
            && let Some(physical) = self.physical_files.get_mut(&previous)
        {
            let last_alias = physical
                .aliases
                .as_mut()
                .is_some_and(|aliases| aliases.remove(file));
            // The path keeps its generation after the last alias retires.
            if last_alias {
                physical.aliases = None;
            }
        }
        self.entries[file.0].physical_path.clone_from(&path);
        if let Some(uri) = physical_uri {
            self.entries[file.0].physical_uri = uri;
        }
        if let Some(path) = path {
            let physical = self.physical_files.entry(path).or_default();
            if let Some(aliases) = physical.aliases.as_mut() {
                aliases.others.push(file);
            } else {
                physical.aliases = Some(PhysicalAliases {
                    first: file,
                    others: Vec::new(),
                });
            }
            self.invalidate_physical_file(file);
        }
    }

    pub(super) fn invalidate_physical_file(&mut self, file: FileId) {
        let Some(path) = self.entries[file.0].physical_path.as_deref() else {
            return;
        };
        if let Some(physical) = self.physical_files.get_mut(path) {
            physical.generation = physical.generation.wrapping_add(1);
        } else {
            self.physical_files.insert(
                path.to_path_buf(),
                PhysicalFile {
                    aliases: None,
                    generation: 1,
                },
            );
        }
        if let Some(roots) = self.semantic_physical_dependents.remove(path) {
            for root in roots {
                self.remove_semantic_view(root);
            }
        }
    }

    pub(in crate::workspace) fn physical_generation(&self, path: &Path) -> u64 {
        self.physical_files
            .get(path)
            .map_or(0, |physical| physical.generation)
    }

    pub(in crate::workspace) fn physical_aliases(&self, path: &Path) -> Option<&PhysicalAliases> {
        self.physical_files.get(path)?.aliases.as_ref()
    }

    pub(super) fn physical_overlay_conflict(&self, path: &Path) -> bool {
        let mut snapshots = self
            .physical_aliases(path)
            .into_iter()
            .flat_map(PhysicalAliases::iter)
            .filter_map(|file| self.entries[file.0].open_snapshot.as_ref());
        let Some(first) = snapshots.next() else {
            return false;
        };
        snapshots.any(|snapshot| {
            !std::sync::Arc::ptr_eq(first, snapshot)
                && (first.text.len() != snapshot.text.len() || first.text != snapshot.text)
        })
    }

    pub(super) fn semantic_source_target(&self, file: FileId) -> (FileId, bool) {
        let Some(path) = &self.entries[file.0].physical_path else {
            return (file, false);
        };
        if self.physical_overlay_conflict(path) {
            return (file, true);
        }
        (
            self.physical_aliases(path)
                .into_iter()
                .flat_map(PhysicalAliases::iter)
                .copied()
                .filter(|file| self.entries[file.0].open_snapshot.is_some())
                .min_by(|left, right| {
                    let left = &self.entries[left.0];
                    let right = &self.entries[right.0];
                    (left.path.as_deref() != Some(path.as_path()))
                        .cmp(&(right.path.as_deref() != Some(path.as_path())))
                        .then_with(|| left.uri.as_str().cmp(right.uri.as_str()))
                })
                .unwrap_or(file),
            false,
        )
    }
}
