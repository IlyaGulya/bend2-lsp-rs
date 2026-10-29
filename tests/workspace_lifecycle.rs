mod support;

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use bend2_lsp::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{Document, FileId, WorkspaceDb},
};
use support::Must;
use tempfile::tempdir;
use url::Url;

const IMPORT: &str = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
const CLEAN: &str = "def main: U32\n  0\n";
const DISK: &str = "def value: U32\n  1\n";
const UPDATED: &str = "def value: U32\n  2\n";

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).must_be("file URI")
}

fn open(db: &mut WorkspaceDb, path: &Path, text: &str) -> FileId {
    let id = db.set_open_document(
        Document::new(uri(path), "bend".into(), Revision(1), text.into()),
        Some(path.into()),
    );
    db.load_reachable(&[id]);
    id
}

fn replace(db: &mut WorkspaceDb, path: &Path, text: &str) -> FileId {
    let (id, _) = db
        .update_open_snapshot(
            &uri(path),
            Arc::new(DocumentSnapshot::new(Revision(2), text.into())),
        )
        .must_be("update open document");
    db.load_reachable(&[id]);
    id
}

fn close(db: &mut WorkspaceDb, path: &Path) {
    let id = db.close_document(&uri(path)).must_be("close document");
    db.load_reachable(&[id]);
}

#[test]
fn removing_import_releases_dependency_but_preserves_identity_and_live_reader() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, DISK).must_be("write dependency");
    let mut db = WorkspaceDb::default();
    let root_id = open(&mut db, &root, IMPORT);
    let dep_id = db.file_id_by_path(&dep).must_be("dependency identity");
    let reader = db.cached_document(&uri(&dep)).must_be("dependency reader");
    let weak = Arc::downgrade(&reader.snapshot);

    assert_eq!(replace(&mut db, &root, CLEAN), root_id);
    assert!(db.cached_document(&uri(&dep)).is_none());
    assert_eq!(db.file_id_by_path(&dep), Some(dep_id));
    assert_eq!(db.file_id_by_uri(&uri(&dep)), Some(dep_id));
    assert_eq!(reader.text, DISK);
    assert!(weak.upgrade().is_some());
    drop(reader);
    assert!(weak.upgrade().is_none());
}

#[test]
fn closing_last_root_releases_root_and_orphan_dependency() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&root, IMPORT).must_be("write root");
    fs::write(&dep, DISK).must_be("write dependency");
    let mut db = WorkspaceDb::default();
    let root_id = open(&mut db, &root, IMPORT);
    let root_weak = Arc::downgrade(&db.open_document(&uri(&root)).must_be("open root").snapshot);
    let dep_weak = Arc::downgrade(
        &db.cached_document(&uri(&dep))
            .must_be("cached dependency")
            .snapshot,
    );

    close(&mut db, &root);
    assert!(root_weak.upgrade().is_none());
    assert!(dep_weak.upgrade().is_none());
    assert!(db.cached_document(&uri(&root)).is_none());
    assert!(db.cached_document(&uri(&dep)).is_none());
    assert!(db.indexed_documents().is_empty());
    assert_eq!(db.file_id_by_path(&root), Some(root_id));
}

#[test]
fn shared_dependency_is_retained_until_last_importing_root_closes() {
    let temp = tempdir().must_be("temporary workspace");
    let first = temp.path().join("first.bend");
    let second = temp.path().join("second.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, DISK).must_be("write dependency");
    let mut db = WorkspaceDb::default();
    open(&mut db, &first, IMPORT);
    open(&mut db, &second, IMPORT);
    let weak = Arc::downgrade(
        &db.cached_document(&uri(&dep))
            .must_be("cached shared dependency")
            .snapshot,
    );

    close(&mut db, &first);
    assert_eq!(
        db.cached_document(&uri(&dep)).must_be("still needed").text,
        DISK
    );
    assert!(weak.upgrade().is_some());
    close(&mut db, &second);
    assert!(weak.upgrade().is_none());
    assert!(db.cached_document(&uri(&dep)).is_none());
}

#[test]
fn reimport_reads_updated_disk_with_stable_dependency_identity() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, DISK).must_be("write dependency");
    let mut db = WorkspaceDb::default();
    open(&mut db, &root, IMPORT);
    let id = db.file_id_by_path(&dep).must_be("dependency identity");
    replace(&mut db, &root, CLEAN);
    fs::write(&dep, UPDATED).must_be("replace dependency on disk");

    replace(&mut db, &root, IMPORT);
    assert_eq!(db.file_id_by_path(&dep), Some(id));
    assert_eq!(
        db.cached_document(&uri(&dep))
            .must_be("fresh dependency")
            .text,
        UPDATED
    );
    let dependents = db.dependents(id);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].uri.as_ref(), &uri(&root));
}

#[test]
fn reimport_during_unfinished_load_discards_retired_disk_and_import_edges() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    let old_target = temp.path().join("old_target.bend");
    fs::write(
        &dep,
        "import ./old_target.bend as Old\ndef value: U32\n  Old.value\n",
    )
    .must_be("write original dependency");
    fs::write(&old_target, DISK).must_be("write old transitive dependency");
    let mut db = WorkspaceDb::default();
    let root_id = open(&mut db, &root, IMPORT);
    let dep_id = db.file_id_by_path(&dep).must_be("dependency identity");
    let reader = db
        .cached_document(&uri(&dep))
        .must_be("old dependency reader");
    let weak = Arc::downgrade(&reader.snapshot);
    let old_target_weak = Arc::downgrade(
        &db.cached_document(&uri(&old_target))
            .must_be("old transitive payload")
            .snapshot,
    );

    db.update_open_snapshot(
        &uri(&root),
        Arc::new(DocumentSnapshot::new(Revision(2), CLEAN.into())),
    )
    .must_be("remove import without finishing the load");
    fs::write(&dep, UPDATED).must_be("update retired dependency on disk");
    db.update_open_snapshot(
        &uri(&root),
        Arc::new(DocumentSnapshot::new(Revision(3), IMPORT.into())),
    )
    .must_be("reimport while earlier transaction is unfinished");
    db.load_reachable(&[root_id]);

    assert_eq!(db.file_id_by_path(&dep), Some(dep_id));
    assert_eq!(
        db.cached_document(&uri(&dep))
            .must_be("fresh reimport")
            .text,
        UPDATED
    );
    assert!(db.cached_document(&uri(&old_target)).is_none());
    assert!(old_target_weak.upgrade().is_none());
    assert!(reader.text.contains("Old.value"));
    drop(reader);
    assert!(weak.upgrade().is_none());
    let dependents = db.dependents(dep_id);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].uri.as_ref(), &uri(&root));
}

#[test]
fn unreachable_import_cycle_releases_both_payloads_and_reverse_links() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let a = temp.path().join("a.bend");
    let b = temp.path().join("b.bend");
    fs::write(&a, "import ./b.bend as B\ndef value: U32\n  B.value\n")
        .must_be("write cycle member A");
    fs::write(&b, "import ./a.bend as A\ndef value: U32\n  A.value\n")
        .must_be("write cycle member B");
    let mut db = WorkspaceDb::default();
    open(
        &mut db,
        &root,
        "import ./a.bend as A\ndef main: U32\n  A.value\n",
    );
    let a_id = db.file_id_by_path(&a).must_be("A identity");
    let b_id = db.file_id_by_path(&b).must_be("B identity");
    let a_weak = Arc::downgrade(&db.cached_document(&uri(&a)).must_be("A payload").snapshot);
    let b_weak = Arc::downgrade(&db.cached_document(&uri(&b)).must_be("B payload").snapshot);

    replace(&mut db, &root, CLEAN);
    assert!(a_weak.upgrade().is_none());
    assert!(b_weak.upgrade().is_none());
    assert!(db.cached_document(&uri(&a)).is_none());
    assert!(db.cached_document(&uri(&b)).is_none());
    assert!(db.dependents(a_id).is_empty());
    assert!(db.dependents(b_id).is_empty());

    open(&mut db, &a, DISK);
    assert!(db.cached_document(&uri(&b)).is_none());
    assert!(db.dependents(b_id).is_empty());
    assert_eq!(db.file_id_by_path(&a), Some(a_id));
}

#[test]
fn independently_open_dependency_survives_importer_close() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, DISK).must_be("write dependency");
    let mut db = WorkspaceDb::default();
    open(&mut db, &root, IMPORT);
    open(&mut db, &dep, UPDATED);
    let weak = Arc::downgrade(
        &db.open_document(&uri(&dep))
            .must_be("open dependency")
            .snapshot,
    );

    close(&mut db, &root);
    assert_eq!(
        db.open_document(&uri(&dep))
            .must_be("independent root")
            .text,
        UPDATED
    );
    assert!(weak.upgrade().is_some());
    close(&mut db, &dep);
    assert!(weak.upgrade().is_none());
    assert!(db.cached_document(&uri(&dep)).is_none());
}

#[test]
fn closing_unsaved_dependency_overlay_reloads_fresh_disk_for_open_importer() {
    let temp = tempdir().must_be("temporary workspace");
    let root = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    let overlay_only = temp.path().join("overlay_only.bend");
    fs::write(&dep, DISK).must_be("write initial dependency");
    fs::write(&overlay_only, DISK).must_be("write overlay-only dependency");
    let mut db = WorkspaceDb::default();
    let root_id = open(&mut db, &root, IMPORT);
    let dep_id = db.file_id_by_path(&dep).must_be("dependency identity");
    open(
        &mut db,
        &dep,
        "import ./overlay_only.bend as Overlay\ndef value: U32\n  Overlay.value\n",
    );
    let overlay_weak = Arc::downgrade(&db.open_document(&uri(&dep)).must_be("overlay").snapshot);
    let orphan_weak = Arc::downgrade(
        &db.cached_document(&uri(&overlay_only))
            .must_be("overlay dependency")
            .snapshot,
    );
    fs::write(&dep, UPDATED).must_be("replace disk while overlay is open");
    assert!(
        db.open_document(&uri(&dep))
            .must_be("unsaved overlay")
            .text
            .contains("Overlay.value")
    );

    close(&mut db, &dep);
    assert!(overlay_weak.upgrade().is_none());
    assert!(orphan_weak.upgrade().is_none());
    assert!(db.open_document(&uri(&dep)).is_none());
    assert_eq!(
        db.cached_document(&uri(&dep))
            .must_be("fresh disk dependency")
            .text,
        UPDATED
    );
    assert!(db.cached_document(&uri(&overlay_only)).is_none());
    assert_eq!(db.file_id_by_path(&dep), Some(dep_id));
    assert_eq!(db.file_id_by_path(&root), Some(root_id));
    let dependents = db.dependents(dep_id);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].uri.as_ref(), &uri(&root));
}

#[test]
fn dependents_preserve_terminal_frontiers_diamonds_and_cycles_across_updates() {
    let temp = tempdir().must_be("temporary dependency graph");
    let common = temp.path().join("common.bend");
    let left = temp.path().join("left.bend");
    let right = temp.path().join("right.bend");
    let outer = temp.path().join("outer.bend");
    fs::write(&common, DISK).must_be("write shared dependency");
    let mut db = WorkspaceDb::default();
    let source = "import ./common.bend as Common\ndef value: U32\n  Common.value\n";
    open(&mut db, &left, source);
    open(&mut db, &right, source);
    let common_id = db.file_id_by_path(&common).must_be("shared file identity");
    let dependent_uris = |db: &WorkspaceDb| {
        let mut uris = db
            .dependents(common_id)
            .into_iter()
            .map(|document| document.uri.as_ref().clone())
            .collect::<Vec<_>>();
        uris.sort_unstable();
        uris
    };
    let mut expected = vec![uri(&left), uri(&right)];
    expected.sort_unstable();
    assert_eq!(dependent_uris(&db), expected);

    open(
        &mut db,
        &outer,
        "import ./left.bend as Left\nimport ./right.bend as Right\ndef value: U32\n  Left.value\n",
    );
    expected.push(uri(&outer));
    expected.sort_unstable();
    assert_eq!(dependent_uris(&db), expected);

    open(
        &mut db,
        &common,
        "import ./common.bend as Self\nimport ./left.bend as Left\ndef value: U32\n  1\n",
    );
    assert_eq!(dependent_uris(&db), expected);
}

const STRESS_FILES: usize = 10_000;
const STRESS_SHARED_ROOT: usize = STRESS_FILES - 2;
const STRESS_OVERLAY: usize = STRESS_FILES - 1;
const STRESS_WATCHED: usize = STRESS_FILES - 3;

struct LifecycleFixture {
    paths: Vec<PathBuf>,
    uris: Vec<Url>,
    sources: Vec<String>,
}

impl LifecycleFixture {
    fn new(directory: &Path) -> Self {
        let mut paths = Vec::with_capacity(STRESS_FILES);
        let mut uris = Vec::with_capacity(STRESS_FILES);
        let mut sources = Vec::with_capacity(STRESS_FILES);
        for index in 0..STRESS_FILES {
            let path = directory.join(format!("file_{index}.bend"));
            // A binary tree bounds import fan-out and reverse traversal depth.
            // The second open root shares the final leaf with the main tree.
            let targets: Vec<_> = if index == STRESS_SHARED_ROOT {
                vec![STRESS_OVERLAY]
            } else {
                [index * 2 + 1, index * 2 + 2]
                    .into_iter()
                    .filter(|target| *target < STRESS_FILES)
                    .collect()
            };
            let mut source = String::new();
            for target in targets {
                writeln!(source, "import ./file_{target}.bend as Child{target}")
                    .must_be("format bounded import tree");
            }
            write!(source, "def stress_file_{index}: U32\n  0\n")
                .must_be("format fixture declaration");
            fs::write(&path, &source).must_be("write lifecycle fixture once");
            uris.push(uri(&path));
            paths.push(path);
            sources.push(source);
        }
        Self {
            paths,
            uris,
            sources,
        }
    }
}

fn payload_weak(db: &WorkspaceDb, uri: &Url) -> Weak<DocumentSnapshot> {
    Arc::downgrade(&db.cached_document(uri).must_be("indexed payload").snapshot)
}

fn assert_candidate(db: &WorkspaceDb, source: &Url, prefix: &str, expected: Option<&Url>) {
    let candidates = db.import_completion_candidates(source, prefix, false);
    if let Some(expected) = expected {
        assert_eq!(candidates.len(), 1, "unique candidate for {prefix}");
        assert_eq!(candidates[0].0.uri.as_ref(), expected);
    } else {
        assert!(candidates.is_empty(), "retired candidate for {prefix}");
    }
}

fn reindex_stress_overlays(
    db: &mut WorkspaceDb,
    fixture: &mut LifecycleFixture,
    cycle: usize,
) -> Vec<Weak<DocumentSnapshot>> {
    let mut snapshots = Vec::new();
    replace(
        db,
        &fixture.paths[0],
        &fixture.sources[0].replace("stress_file_0:", "stress_reindexed_root:"),
    );
    snapshots.push(payload_weak(db, &fixture.uris[0]));
    assert!(
        bend2_lsp::analysis::declaration_range(
            &db.cached_document(&fixture.uris[0])
                .must_be("reindexed root"),
            "stress_reindexed_root",
        )
        .is_some()
    );
    open(
        db,
        &fixture.paths[STRESS_SHARED_ROOT],
        &fixture.sources[STRESS_SHARED_ROOT],
    );
    snapshots.push(payload_weak(db, &fixture.uris[STRESS_SHARED_ROOT]));
    open(
        db,
        &fixture.paths[STRESS_OVERLAY],
        "def stress_overlay_old: U32\n  1\n",
    );
    let old_overlay = payload_weak(db, &fixture.uris[STRESS_OVERLAY]);
    snapshots.push(old_overlay.clone());
    let overlay = format!(
        "import ./file_{STRESS_WATCHED}.bend as Watched\ndef stress_overlay_new: U32\n  Watched.stress_watched_{cycle}\n"
    );
    replace(db, &fixture.paths[STRESS_OVERLAY], &overlay);
    assert!(old_overlay.upgrade().is_none());
    snapshots.push(payload_weak(db, &fixture.uris[STRESS_OVERLAY]));
    assert_candidate(db, &fixture.uris[0], "stress_overlay_old", None);
    assert_candidate(
        db,
        &fixture.uris[0],
        "stress_overlay_new",
        Some(&fixture.uris[STRESS_OVERLAY]),
    );

    let old_watched = payload_weak(db, &fixture.uris[STRESS_WATCHED]);
    fixture.sources[STRESS_WATCHED] = format!("def stress_watched_{cycle}: U32\n  {cycle}\n");
    fs::write(
        &fixture.paths[STRESS_WATCHED],
        &fixture.sources[STRESS_WATCHED],
    )
    .must_be("change watched disk source");
    let (watched, _) = db
        .sync_disk_path(
            &fixture.paths[STRESS_WATCHED],
            Some(fixture.sources[STRESS_WATCHED].clone()),
        )
        .must_be("apply supported disk invalidation");
    db.load_reachable(&[watched]);
    assert_candidate(
        db,
        &fixture.uris[0],
        &format!("stress_watched_{cycle}"),
        Some(&fixture.uris[STRESS_WATCHED]),
    );
    assert_candidate(db, &fixture.uris[0], "stress_file_9997", None);
    assert!(old_watched.upgrade().is_none());
    let changed = payload_weak(db, &fixture.uris[STRESS_WATCHED]);
    snapshots.push(changed.clone());
    fs::remove_file(&fixture.paths[STRESS_WATCHED]).must_be("delete watched disk source");
    db.sync_disk_path(&fixture.paths[STRESS_WATCHED], None)
        .must_be("invalidate deleted disk source");
    db.load_reachable(&[watched]);
    assert!(changed.upgrade().is_none());
    assert!(db.cached_document(&fixture.uris[STRESS_WATCHED]).is_none());
    assert_eq!(db.indexed_documents().len(), STRESS_FILES - 1);
    assert_candidate(db, &fixture.uris[0], "stress_watched_", None);
    fs::write(
        &fixture.paths[STRESS_WATCHED],
        &fixture.sources[STRESS_WATCHED],
    )
    .must_be("recreate watched source");
    db.sync_disk_path(
        &fixture.paths[STRESS_WATCHED],
        Some(fixture.sources[STRESS_WATCHED].clone()),
    )
    .must_be("invalidate recreated disk source");
    db.load_reachable(&[watched]);
    snapshots.push(payload_weak(db, &fixture.uris[STRESS_WATCHED]));
    assert_eq!(db.indexed_documents().len(), STRESS_FILES);
    snapshots
}

fn fresh_stress_payloads(
    db: &WorkspaceDb,
    fixture: &LifecycleFixture,
    cycle: usize,
) -> Vec<Weak<DocumentSnapshot>> {
    fixture
        .uris
        .iter()
        .enumerate()
        .map(|(index, uri)| {
            let document = db.cached_document(uri).must_be("freshly loaded payload");
            assert_eq!(
                document.text.as_str(),
                fixture.sources[index],
                "cycle {cycle}, file {index}",
            );
            Arc::downgrade(&document.snapshot)
        })
        .collect()
}

fn assert_shared_root_retained(
    db: &WorkspaceDb,
    fixture: &LifecycleFixture,
    identities: &[FileId],
) {
    assert_eq!(
        db.open_document(&fixture.uris[STRESS_SHARED_ROOT])
            .must_be("independent shared root survives")
            .text
            .as_str(),
        fixture.sources[STRESS_SHARED_ROOT],
    );
    let dependents = db.dependents(identities[STRESS_OVERLAY]);
    assert_eq!(dependents.len(), 1);
    assert_eq!(dependents[0].uri.as_ref(), &fixture.uris[STRESS_SHARED_ROOT]);
    drop(dependents);
    assert_candidate(
        db,
        &fixture.uris[STRESS_SHARED_ROOT],
        "stress_overlay_new",
        Some(&fixture.uris[STRESS_OVERLAY]),
    );
}

#[test]
fn repeated_10000_file_import_lifecycle_releases_snapshots_and_preserves_identity() {
    let temp = tempdir().must_be("temporary stress workspace");
    let mut fixture = LifecycleFixture::new(temp.path());
    // Discovered-folder admission/removal is crate-private. This test exercises
    // public import/open-root reachability in one DB; the LSP runner separately
    // exercises real discovered-folder removal and readdition.
    let mut db = WorkspaceDb::default();
    let mut identities = Vec::new();
    for cycle in 0..3 {
        open(&mut db, &fixture.paths[0], &fixture.sources[0]);
        assert_eq!(db.indexed_documents().len(), STRESS_FILES);
        let initial = fresh_stress_payloads(&db, &fixture, cycle);
        for (index, path) in fixture.paths.iter().enumerate() {
            let id = db.file_id_by_path(path).must_be("stable file identity");
            assert_eq!(db.file_id_by_uri(&fixture.uris[index]), Some(id));
            if cycle == 0 {
                identities.push(id);
            } else {
                assert_eq!(id, identities[index], "cycle {cycle}, file {index}");
            }
        }
        let mut snapshots = reindex_stress_overlays(&mut db, &mut fixture, cycle);
        assert!(initial[0].upgrade().is_none());
        assert!(initial[STRESS_WATCHED].upgrade().is_none());
        close(&mut db, &fixture.paths[0]);
        assert!(snapshots[0].upgrade().is_none());
        assert_eq!(db.indexed_documents().len(), 3);
        // Weak handles observe snapshots, not total allocation or allocator
        // liveness; stable FileId/name interning is intentional.
        for snapshot in [
            &snapshots[1],
            &snapshots[3],
            snapshots.last().must_be("live watched snapshot"),
        ] {
            assert!(snapshot.upgrade().is_some());
        }
        for (index, snapshot) in initial.iter().enumerate() {
            assert_eq!(
                snapshot.upgrade().is_some(),
                index == STRESS_SHARED_ROOT || index == STRESS_OVERLAY,
                "cycle {cycle}, initial snapshot {index}",
            );
        }
        assert_shared_root_retained(&db, &fixture, &identities);
        fixture.sources[STRESS_OVERLAY] = format!("def stress_fresh_{cycle}: U32\n  {cycle}\n");
        fs::write(
            &fixture.paths[STRESS_OVERLAY],
            &fixture.sources[STRESS_OVERLAY],
        )
        .must_be("change backing disk without saving overlay");
        close(&mut db, &fixture.paths[STRESS_OVERLAY]);
        assert!(initial[STRESS_OVERLAY].upgrade().is_none());
        assert!(snapshots[3].upgrade().is_none());
        assert!(
            snapshots
                .last()
                .must_be("recreated snapshot")
                .upgrade()
                .is_none()
        );
        assert_eq!(db.indexed_documents().len(), 2);
        assert_candidate(
            &db,
            &fixture.uris[STRESS_SHARED_ROOT],
            "stress_overlay_",
            None,
        );
        assert_candidate(
            &db,
            &fixture.uris[STRESS_SHARED_ROOT],
            &format!("stress_fresh_{cycle}"),
            Some(&fixture.uris[STRESS_OVERLAY]),
        );
        assert_eq!(
            db.cached_document(&fixture.uris[STRESS_OVERLAY])
                .must_be("fresh backing disk after overlay close")
                .text
                .as_str(),
            fixture.sources[STRESS_OVERLAY],
        );
        snapshots.push(payload_weak(&db, &fixture.uris[STRESS_OVERLAY]));
        close(&mut db, &fixture.paths[STRESS_SHARED_ROOT]);
        assert!(db.indexed_documents().is_empty());
        assert_candidate(&db, &fixture.uris[0], "stress_", None);
        for snapshot in initial.iter().chain(&snapshots) {
            assert!(
                snapshot.upgrade().is_none(),
                "cycle {cycle}: retired snapshot",
            );
        }
        assert_retired_identities(&db, &fixture, &identities);
    }
}

fn assert_retired_identities(db: &WorkspaceDb, fixture: &LifecycleFixture, identities: &[FileId]) {
    for (index, id) in identities.iter().enumerate() {
        assert!(db.cached_document(&fixture.uris[index]).is_none());
        assert!(db.dependents(*id).is_empty());
        assert_eq!(db.file_id_by_path(&fixture.paths[index]), Some(*id));
        assert_eq!(db.file_id_by_uri(&fixture.uris[index]), Some(*id));
    }
}
