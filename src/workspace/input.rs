use super::{Document, FileId, PhysicalAliases, WorkspaceDb, normalize_path};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
};
use url::Url;

/// One coherent sample of editor inputs: the immutable open document
/// snapshots and the revision generation each spelling was published under.
///
/// Both fields are produced from a single workspace state read, so a fence
/// sampled beside these documents always describes the same inputs.
pub struct EditorInputs {
    pub documents: Vec<Document>,
    pub generations: HashMap<Url, u64>,
}

impl WorkspaceDb {
    /// Every file identity whose lexical path or known physical input is this
    /// spelling. Path events fan out through this before dependents are
    /// computed, so one change reaches every alias, not only the identity the
    /// lexical path map currently owns.
    ///
    /// Uses prepared lexical and physical identities only. A metadata sweep
    /// also finds retired lexical owners and virtual spellings, then joins
    /// their known physical alias groups. No filesystem operation runs while
    /// the caller holds a workspace read.
    pub(crate) fn input_files(&self, path: &Path) -> Vec<FileId> {
        let normalized = normalize_path(path);
        let mut files = Vec::new();
        if let Some(&file) = self.by_path.get(&normalized) {
            files.push(file);
        }
        if let Some(aliases) = self.physical_files.get(&normalized) {
            files.extend(aliases.iter().copied());
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.path.as_deref() == Some(normalized.as_path())
                || entry.physical_path.as_deref() == Some(normalized.as_path())
            {
                files.push(FileId(index));
                if let Some(physical) = entry.physical_path.as_deref()
                    && physical != normalized.as_path()
                    && let Some(aliases) = self.physical_files.get(physical)
                {
                    files.extend(aliases.iter().copied());
                }
            }
        }
        files.sort_unstable();
        files.dedup();
        files
    }

    /// Every currently open logical URI spelling of a known input, ordered
    /// by URI. Unsaved and virtual buffers participate through their prepared
    /// paths; a closed cached source is not an editor spelling.
    pub(crate) fn open_input_spellings(&self, path: &Path) -> Vec<Url> {
        let mut spellings: Vec<Url> = self
            .input_files(path)
            .into_iter()
            .filter(|file| self.is_file_open(*file))
            .map(|file| self.entries[file.0].uri.as_ref().clone())
            .collect();
        spellings.sort_unstable();
        spellings.dedup();
        spellings
    }

    pub(crate) fn open_document_by_id(&self, file: FileId) -> Option<Document> {
        let entry = self.entries.get(file.0)?;
        entry.open_snapshot.as_ref()?;
        entry.document()
    }

    /// Every currently open editor spelling, borrowed. Membership is read
    /// from the same open snapshots the workspace services, so a caller can
    /// detect an alias opened or closed since it prepared work.
    pub(crate) fn open_editor_spellings(&self) -> impl Iterator<Item = &Url> {
        self.entries
            .iter()
            .filter_map(|entry| entry.open_snapshot.as_ref().map(|_| entry.uri.as_ref()))
    }

    /// Compiler-input ambiguity reachable from one exact open spelling: the
    /// requested root buffer and its loaded local dependency graph. Returns
    /// the first conflicted identity in deterministic order and every open
    /// spelling of that input, ordered by URI, so the caller can report the
    /// ambiguity without selecting a different buffer.
    ///
    /// The walk follows loaded import edges and physical alias groups only;
    /// both are indexed, so no source text is scanned and no discovery work is
    /// triggered. The requested buffer stays the traversal root and is never
    /// replaced by an alias.
    pub(crate) fn source_input_conflict(&self, uri: &Url) -> Option<(FileId, Vec<Url>)> {
        let root = *self.by_uri.get(uri)?;
        if self.entries.get(root.0)?.uri.as_ref() != uri {
            return None;
        }
        let mut pending = VecDeque::from([root]);
        let mut visited = HashSet::new();
        while let Some(file) = pending.pop_front() {
            if !visited.insert(file) {
                continue;
            }
            let Some(entry) = self.entries.get(file.0) else {
                continue;
            };
            let physical = entry.physical_path.as_deref();
            if let Some(path) = physical
                && let Some(spellings) = self.physical_conflict_spellings(path)
            {
                return Some((file, spellings));
            }
            if let Some(aliases) = physical.and_then(|path| self.physical_files.get(path)) {
                for alias in aliases.iter().copied() {
                    if !visited.contains(&alias) {
                        pending.push_back(alias);
                    }
                }
            }
            for edge in &entry.imports {
                if !visited.contains(&edge.target) {
                    pending.push_back(edge.target);
                }
            }
        }
        None
    }

    /// Open spellings of one canonical physical input whose snapshots
    /// disagree, reusing main's overlay-conflict fact.
    fn physical_conflict_spellings(&self, path: &Path) -> Option<Vec<Url>> {
        if !self.physical_overlay_conflict(path) {
            return None;
        }
        let mut spellings: Vec<Url> = self
            .physical_files
            .get(path)
            .into_iter()
            .flat_map(PhysicalAliases::iter)
            .copied()
            .filter(|file| self.entries[file.0].open_snapshot.is_some())
            .map(|file| self.entries[file.0].uri.as_ref().clone())
            .collect();
        spellings.sort_unstable();
        spellings.dedup();
        Some(spellings)
    }
}
