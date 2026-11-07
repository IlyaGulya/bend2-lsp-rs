//! Low-overhead ownership accounting for document snapshots and semantic views.
//!
//! The registry holds only weak handles. Tracking never extends the lifetime of
//! a snapshot or view, deduplicates by allocation identity, and keeps counting a
//! superseded revision for exactly as long as another component still retains
//! it. Expired metadata is removed by a bounded rotating sweep on registration
//! and by an exact sweep when the diagnostic summary is read.
//!
//! Byte values are estimates read directly from existing storage: logical text
//! length and the capacities of owned buffers. Allocator headers, hash-table
//! control bytes and shared snapshot buffers behind a [`SemanticView`] are not
//! included, so the values must not be presented as allocator or RSS totals.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
};

use serde::Serialize;

use crate::{analysis::DocumentSnapshot, workspace::SemanticView};

/// Bounded entries examined by each registration's expired-metadata sweep.
const PRUNE_BATCH: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotUsage {
    pub(crate) live_snapshots: usize,
    pub(crate) source_bytes: u64,
    pub(crate) source_capacity_bytes: u64,
    pub(crate) line_index_bytes: u64,
    pub(crate) syntax_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticViewUsage {
    pub(crate) live_views: usize,
    pub(crate) owned_backing_bytes: u64,
}

/// Aggregates currently tracked by one [`StorageRegistry`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageSummary {
    pub(crate) snapshots: SnapshotUsage,
    pub(crate) semantic_views: SemanticViewUsage,
}

/// Storage owned by one workspace database, measured without touching source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceUsage {
    pub(crate) open_documents: usize,
    pub(crate) indexed_documents: usize,
    pub(crate) owned_snapshots: usize,
    pub(crate) semantic_cache_entries: usize,
    pub(crate) semantic_cache_bytes: u64,
    pub(crate) workspace_index_bytes: u64,
    pub(crate) semantic_evictions: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SnapshotBytes {
    source: u64,
    source_capacity: u64,
    line_index: u64,
    syntax: u64,
}

impl SnapshotBytes {
    fn of(snapshot: &DocumentSnapshot) -> Self {
        Self {
            source: saturating_u64(snapshot.text.len()),
            source_capacity: saturating_u64(snapshot.text.capacity()),
            line_index: saturating_u64(snapshot.line_index.footprint_bytes()),
            syntax: saturating_u64(snapshot.syntax.footprint_bytes()),
        }
    }
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct TrackedSnapshot {
    handle: Weak<DocumentSnapshot>,
    bytes: SnapshotBytes,
    generation: u64,
}

struct TrackedView {
    handle: Weak<SemanticView>,
    owned_bytes: u64,
    generation: u64,
}

#[derive(Default)]
struct RegistryState {
    snapshots: HashMap<usize, TrackedSnapshot>,
    views: HashMap<usize, TrackedView>,
    snapshot_order: VecDeque<(usize, u64)>,
    view_order: VecDeque<(usize, u64)>,
    next_generation: u64,
}

impl RegistryState {
    fn generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        generation
    }

    fn prune_snapshots(&mut self) {
        for _ in 0..PRUNE_BATCH {
            let Some((key, generation)) = self.snapshot_order.pop_front() else {
                break;
            };
            let tracked = self
                .snapshots
                .get(&key)
                .map(|entry| (entry.generation == generation, entry.handle.strong_count() == 0));
            match tracked {
                Some((true, true)) => {
                    self.snapshots.remove(&key);
                }
                Some((true, false)) => self.snapshot_order.push_back((key, generation)),
                Some((false, _)) | None => {}
            }
        }
    }

    fn prune_views(&mut self) {
        for _ in 0..PRUNE_BATCH {
            let Some((key, generation)) = self.view_order.pop_front() else {
                break;
            };
            let tracked = self
                .views
                .get(&key)
                .map(|entry| (entry.generation == generation, entry.handle.strong_count() == 0));
            match tracked {
                Some((true, true)) => {
                    self.views.remove(&key);
                }
                Some((true, false)) => self.view_order.push_back((key, generation)),
                Some((false, _)) | None => {}
            }
        }
    }
}

/// Shared weak registry instantiated once per server.
///
/// Methods take `&self` so the owning server can share one registry without a
/// mutable borrow. Registration does a fixed amount of metadata arithmetic and
/// one short critical section; it never scans source text or retains a strong
/// reference.
#[derive(Default)]
pub(crate) struct StorageRegistry {
    state: Mutex<RegistryState>,
}

impl StorageRegistry {
    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Track one actual snapshot allocation. Repeated calls for the same
    /// `Arc` are no-ops, and distinct allocations are never merged even when
    /// their text is identical.
    pub(crate) fn track_snapshot(&self, snapshot: &Arc<DocumentSnapshot>) {
        let key = Arc::as_ptr(snapshot) as usize;
        let mut state = self.lock();
        let already_tracked = state
            .snapshots
            .get(&key)
            .and_then(|entry| entry.handle.upgrade())
            .is_some_and(|tracked| Arc::ptr_eq(&tracked, snapshot));
        if already_tracked {
            return;
        }
        let bytes = SnapshotBytes::of(snapshot);
        let generation = state.generation();
        state.snapshots.insert(
            key,
            TrackedSnapshot {
                handle: Arc::downgrade(snapshot),
                bytes,
                generation,
            },
        );
        state.snapshot_order.push_back((key, generation));
        state.prune_snapshots();
    }

    /// Track one actual semantic view allocation. Its owned backing estimate
    /// excludes the document snapshot buffers it shares with the workspace.
    pub(crate) fn track_view(&self, view: &Arc<SemanticView>) {
        let key = Arc::as_ptr(view) as usize;
        let mut state = self.lock();
        let already_tracked = state
            .views
            .get(&key)
            .and_then(|entry| entry.handle.upgrade())
            .is_some_and(|tracked| Arc::ptr_eq(&tracked, view));
        if already_tracked {
            return;
        }
        let owned_bytes = view.owned_backing_bytes();
        let generation = state.generation();
        state.views.insert(
            key,
            TrackedView {
                handle: Arc::downgrade(view),
                owned_bytes,
                generation,
            },
        );
        state.view_order.push_back((key, generation));
        state.prune_views();
    }

    /// Exact current aggregates. Expired entries are removed here as well, so
    /// a report never counts a snapshot or view whose last strong owner is gone.
    #[must_use]
    pub(crate) fn snapshot(&self) -> StorageSummary {
        let mut state = self.lock();
        state
            .snapshots
            .retain(|_, entry| entry.handle.strong_count() > 0);
        state
            .views
            .retain(|_, entry| entry.handle.strong_count() > 0);
        state.snapshot_order = state
            .snapshots
            .iter()
            .map(|(&key, entry)| (key, entry.generation))
            .collect();
        state.view_order = state
            .views
            .iter()
            .map(|(&key, entry)| (key, entry.generation))
            .collect();
        let mut summary = StorageSummary::default();
        for entry in state.snapshots.values() {
            summary.snapshots.live_snapshots += 1;
            summary.snapshots.source_bytes = summary
                .snapshots
                .source_bytes
                .saturating_add(entry.bytes.source);
            summary.snapshots.source_capacity_bytes = summary
                .snapshots
                .source_capacity_bytes
                .saturating_add(entry.bytes.source_capacity);
            summary.snapshots.line_index_bytes = summary
                .snapshots
                .line_index_bytes
                .saturating_add(entry.bytes.line_index);
            summary.snapshots.syntax_bytes = summary
                .snapshots
                .syntax_bytes
                .saturating_add(entry.bytes.syntax);
        }
        for entry in state.views.values() {
            summary.semantic_views.live_views += 1;
            summary.semantic_views.owned_backing_bytes = summary
                .semantic_views
                .owned_backing_bytes
                .saturating_add(entry.owned_bytes);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        analysis::Revision,
        workspace::{Document, WorkspaceDb},
    };
    use std::{io, path::PathBuf, sync::Arc};
    use url::Url;

    fn snapshot(revision: i32, text: &str) -> Arc<DocumentSnapshot> {
        Arc::new(DocumentSnapshot::new(Revision(revision), text.into()))
    }

    fn open_workspace() -> io::Result<(tempfile::TempDir, Url, PathBuf)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("main.bend");
        let uri = Url::from_file_path(&path).map_err(|()| io::Error::other("document URI"))?;
        Ok((directory, uri, path))
    }

    #[test]
    fn registry_deduplicates_identity_and_never_extends_lifetime() -> io::Result<()> {
        let registry = StorageRegistry::default();
        let source = "def value() -> U32:\n  0\n";
        let document = snapshot(1, source);
        let strong_before = Arc::strong_count(&document);

        registry.track_snapshot(&document);
        registry.track_snapshot(&document);

        assert_eq!(Arc::strong_count(&document), strong_before);
        let summary = registry.snapshot();
        assert_eq!(summary.snapshots.live_snapshots, 1);
        assert_eq!(summary.snapshots.source_bytes, saturating_u64(source.len()));
        assert!(summary.snapshots.line_index_bytes > 0);
        assert!(summary.snapshots.syntax_bytes > 0);

        let retained_by_other_component = Arc::clone(&document);
        drop(document);
        assert_eq!(registry.snapshot().snapshots.live_snapshots, 1);

        drop(retained_by_other_component);
        let released = registry.snapshot();
        assert_eq!(released.snapshots.live_snapshots, 0);
        assert_eq!(released.snapshots.source_bytes, 0);
        assert_eq!(released.snapshots.line_index_bytes, 0);
        assert_eq!(released.snapshots.syntax_bytes, 0);
        Ok(())
    }

    #[test]
    fn poisoned_registry_never_presents_partial_ownership_as_zero_storage() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let registry = StorageRegistry::default();
        let document = snapshot(1, "def value() -> U32:\n  0\n");
        registry.track_snapshot(&document);
        assert_eq!(registry.snapshot().snapshots.live_snapshots, 1);
        let failed = catch_unwind(AssertUnwindSafe(|| {
            let mut state = registry.lock();
            state.snapshots.clear();
            panic!("ownership update failed before commit");
        }));
        assert!(failed.is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(|| registry.snapshot())).is_err(),
            "poisoned accounting must fail instead of exporting partial zero totals"
        );
    }

    #[test]
    fn registry_counts_equal_text_as_distinct_allocations() -> io::Result<()> {
        let registry = StorageRegistry::default();
        let source = "def value() -> U32:\n  0\n";
        let first = snapshot(1, source);
        let second = snapshot(1, source);
        assert!(!Arc::ptr_eq(&first, &second));

        registry.track_snapshot(&first);
        registry.track_snapshot(&second);

        let summary = registry.snapshot();
        assert_eq!(summary.snapshots.live_snapshots, 2);
        assert_eq!(summary.snapshots.source_bytes, saturating_u64(source.len() * 2));
        Ok(())
    }

    #[test]
    fn expired_registration_metadata_stays_bounded() {
        let registry = StorageRegistry::default();
        for revision in 0..(PRUNE_BATCH * 4) {
            let document = snapshot(
                i32::try_from(revision).unwrap_or(i32::MAX),
                "def value() -> U32:\n  0\n",
            );
            registry.track_snapshot(&document);
        }
        let state = registry.lock();
        assert!(
            state.snapshots.len() <= PRUNE_BATCH + 1,
            "expired metadata must be pruned in bounded batches, retained {}",
            state.snapshots.len()
        );
        drop(state);
        assert_eq!(registry.snapshot().snapshots.live_snapshots, 0);
    }

    #[test]
    fn workspace_usage_tracks_open_indexed_shared_and_cached_storage() -> io::Result<()> {
        let (_directory, uri, path) = open_workspace()?;
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(
                uri.clone(),
                "bend".into(),
                Revision(1),
                "def main() -> U32:\n  0\n".into(),
            ),
            Some(path),
        );
        database.load_reachable(&[root]);

        let usage = database.diagnostic_usage();
        assert_eq!(usage.open_documents, 1);
        assert_eq!(usage.indexed_documents, 1);
        assert_eq!(usage.owned_snapshots, 1);
        assert_eq!(usage.semantic_cache_entries, 0);
        assert_eq!(usage.semantic_cache_bytes, 0);
        assert_eq!(usage.semantic_evictions, 0);
        assert!(usage.workspace_index_bytes > 0);

        let view = Arc::new(
            database
                .capture_semantic_view(root, None)
                .ok_or_else(|| io::Error::other("semantic capture"))?
                .build(),
        );
        assert!(database.publish_semantic_view(Arc::clone(&view), None));

        let cached = database.diagnostic_usage();
        assert_eq!(cached.semantic_cache_entries, 1);
        assert_eq!(cached.semantic_cache_bytes, view.owned_backing_bytes());
        assert_eq!(
            cached.owned_snapshots, 1,
            "a view shares the entry snapshot and must not add a second identity"
        );

        let registry = StorageRegistry::default();
        registry.track_snapshot(&view.documents()[0].snapshot);
        registry.track_view(&view);
        registry.track_view(&view);
        let summary = registry.snapshot();
        assert_eq!(summary.snapshots.live_snapshots, 1);
        assert_eq!(summary.semantic_views.live_views, 1);
        assert_eq!(
            summary.semantic_views.owned_backing_bytes,
            view.owned_backing_bytes()
        );

        database
            .close_document(&uri)
            .ok_or_else(|| io::Error::other("closing document"))?;
        database.finish_load_reachable();
        let evicted = database.diagnostic_usage();
        assert_eq!(evicted.open_documents, 0);
        assert_eq!(evicted.semantic_cache_entries, 0);
        assert_eq!(evicted.semantic_cache_bytes, 0);
        assert_eq!(evicted.semantic_evictions, 1);

        let old_view = Arc::clone(&view);
        drop(view);
        assert_eq!(
            registry.snapshot().semantic_views.live_views,
            1,
            "the retained superseded view stays counted while another owner holds it"
        );
        drop(old_view);
        assert_eq!(registry.snapshot().semantic_views.live_views, 0);
        Ok(())
    }
}
