mod support;

use std::{fs, path::Path, sync::Arc};

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
    assert_eq!(dependents[0].uri, uri(&root));
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
    assert_eq!(dependents[0].uri, uri(&root));
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
    assert_eq!(dependents[0].uri, uri(&root));
}
