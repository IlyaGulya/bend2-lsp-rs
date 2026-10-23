mod support;

use bend2_lsp::{
    analysis::{DocumentSnapshot, ReferenceRole, Revision},
    workspace::{Document, FileId, SemanticBase, SemanticTarget, SemanticView, WorkspaceDb},
};
use std::{fs, path::Path, sync::Arc};
use support::Must;
use tempfile::tempdir;
use url::Url;

fn uri(path: &Path) -> Url {
    Url::from_file_path(path).must_be("file URI")
}

fn open(db: &mut WorkspaceDb, path: &Path, source: &str) -> FileId {
    let root = db.set_open_document(
        Document::new(uri(path), "bend".into(), Revision(1), source.into()),
        Some(path.into()),
    );
    db.load_reachable(&[root]);
    root
}

fn view(db: &WorkspaceDb, root: FileId, base: Option<SemanticBase>) -> SemanticView {
    db.capture_semantic_view(root, base)
        .must_be("captured view")
        .build()
}

fn target(view: &SemanticView, file: FileId, needle: &str) -> SemanticTarget {
    let document = view.document(file).must_be("view document");
    let offset = document.text.rfind(needle).must_be("occurrence") + needle.len() - 1;
    view.resolve_at(file, offset)
        .must_be("resolved occurrence")
        .target
}

fn base(path: &Path) -> SemanticBase {
    SemanticBase {
        document: Document::new(
            uri(path),
            "bend".into(),
            Revision::UNVERSIONED,
            "law U32: Kind(0)\nlaw U32.add:\n  for x: U32\n  for y: U32\n  U32\n".into(),
        ),
        generation: 1,
        provenance: Arc::from("workspace-test-prelude"),
        source_lease: None,
    }
}

#[test]
fn reference_groups_preserve_interleaved_forward_uses_and_distinct_locals() {
    let temp = tempdir().must_be("temporary workspace");
    let source =
        "def alpha(x):\n  beta(x)\ndef beta(x):\n  alpha(x)\ndef main():\n  alpha(beta(1))\n";
    let mut db = WorkspaceDb::default();
    let root = open(&mut db, &temp.path().join("main.bend"), source);
    let entry = view(&db, root, None);
    for (name, roles) in [
        (
            "alpha",
            [
                ReferenceRole::Binding,
                ReferenceRole::Call,
                ReferenceRole::Call,
            ],
        ),
        (
            "beta",
            [
                ReferenceRole::Call,
                ReferenceRole::Binding,
                ReferenceRole::Call,
            ],
        ),
    ] {
        let resolved = target(&entry, root, name);
        assert!(matches!(resolved, SemanticTarget::Global(_)));
        let actual: Vec<_> = entry
            .references(resolved)
            .iter()
            .map(|occurrence| {
                (
                    occurrence.site.file,
                    occurrence.site.range.start,
                    occurrence.role,
                )
            })
            .collect();
        let expected: Vec<_> = source
            .match_indices(name)
            .zip(roles)
            .map(|((offset, _), role)| (root, offset, role))
            .collect();
        assert_eq!(actual, expected);
    }
    let local = target(&entry, root, "x");
    assert!(matches!(local, SemanticTarget::Local(_)));
    let offsets: Vec<_> = source
        .match_indices('x')
        .map(|(offset, _)| offset)
        .collect();
    assert_eq!(
        entry
            .references(local)
            .iter()
            .map(|occurrence| occurrence.site.range.start)
            .collect::<Vec<_>>(),
        [offsets[2], offsets[3]]
    );
}

#[test]
fn inherited_base_depends_on_entry_import_order_and_not_path_order() {
    let temp = tempdir().must_be("temporary workspace");
    let dep = temp.path().join("aaa.bend");
    fs::write(&dep, "def next(x: U32) -> U32:\n  U32.add(x, 1)\n").must_be("write dependency");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("zzz.bend"),
        "import Base\nimport ./aaa.bend as D\ndef main() -> U32:\n  D.next(0)\n",
    );
    let dep_id = db.file_id_by_path(&dep).must_be("dependency ID");
    let prelude = base(&temp.path().join("Base.bend"));
    let with_base = view(&db, root, Some(prelude.clone()));
    let standalone = view(&db, dep_id, Some(prelude.clone()));
    let inherited = target(&with_base, dep_id, "U32.add");
    let site = with_base
        .target_site(inherited)
        .must_be("inherited Base target");
    assert_eq!(
        with_base.document(site.file).must_be("Base source").uri,
        uri(&temp.path().join("Base.bend"))
    );
    assert_eq!(
        target(&standalone, dep_id, "U32.add"),
        SemanticTarget::Unresolved
    );
    assert!(Arc::ptr_eq(
        &with_base.document(dep_id).must_be("shared facts").snapshot,
        &standalone.document(dep_id).must_be("shared facts").snapshot
    ));
    let later = open(
        &mut db,
        &temp.path().join("later.bend"),
        "import ./aaa.bend as D\nimport Base\ndef main() -> U32:\n  D.next(0)\n",
    );
    let late_base = view(&db, later, Some(prelude));
    assert_eq!(
        target(&late_base, dep_id, "U32.add"),
        SemanticTarget::Unresolved
    );
    assert_eq!(
        with_base
            .completion_candidates(dep_id, "U32.a")
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>(),
        ["U32.add"]
    );
    assert!(standalone.completion_candidates(dep_id, "U32.a").is_empty());
    assert!(late_base.completion_candidates(dep_id, "U32.a").is_empty());
}

#[test]
fn proof_entries_select_different_bodies_for_the_same_imported_law() {
    let temp = tempdir().must_be("temporary workspace");
    let claims = temp.path().join("claims.bend");
    fs::write(&claims, "law identity:\n  for x: U32\n  U32\n").must_be("write claims");
    let mut db = WorkspaceDb::default();
    let a_path = temp.path().join("a.bend");
    let b_path = temp.path().join("b.bend");
    let a_source = "import Base\nimport ./claims.bend as C\ndef C.identity(x):\n  x\n";
    let a = open(&mut db, &a_path, a_source);
    let b = open(
        &mut db,
        &b_path,
        "import Base\nimport ./claims.bend as C\ndef C.identity(x):\n  U32.add(x, 0)\n",
    );
    let prelude = base(&temp.path().join("Base.bend"));
    let first = view(&db, a, Some(prelude.clone()));
    let second = view(&db, b, Some(prelude.clone()));
    let again = view(&db, a, Some(prelude.clone()));
    for (view, body) in [(&first, &a_path), (&second, &b_path), (&again, &a_path)] {
        let entity = view
            .entities()
            .iter()
            .find(|entity| entity.name == "claims:identity")
            .must_be("law entity");
        assert_eq!(entity.sites.len(), 2);
        assert_eq!(
            view.document(entity.declaration.file)
                .must_be("claims source")
                .uri,
            uri(&claims)
        );
        assert_eq!(
            view.document(entity.body_owner.must_be("body owner").file)
                .must_be("body source")
                .uri,
            uri(body)
        );
        assert_eq!(entity.signature_owner, entity.declaration);
        assert_eq!(
            view.calls_from(entity.id).count(),
            usize::from(body == &b_path)
        );
    }
    db.close_document(&uri(&a_path))
        .must_be("close first proof");
    db.load_reachable(&[b]);
    assert!(!db.validate_semantic_view(&first, Some(&prelude)));
    assert_eq!(open(&mut db, &a_path, a_source), a);
    let reopened = view(&db, a, Some(prelude));
    let entity = reopened
        .entities()
        .iter()
        .find(|entity| entity.name == "claims:identity")
        .must_be("reopened law");
    assert_eq!(entity.body_owner.must_be("reopened body").file, a);
    assert_eq!(reopened.calls_from(entity.id).count(), 0);
    assert_eq!(entity.sites.len(), 2);
}

#[test]
fn nested_dotted_import_name_is_indivisible_despite_local_alias_prefix() {
    let temp = tempdir().must_be("temporary workspace");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, "def Foo.target(x: U32) -> U32:\n  x\n").must_be("write nested export");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./dep.bend as D\ndef caller(D: U32) -> U32:\n  D.Foo.target!(D)\n",
    );
    let view = view(&db, root, None);
    let resolved = target(&view, root, "D.Foo.target");
    let SemanticTarget::Global(id) = resolved else {
        panic!("dotted global target expected");
    };
    assert_eq!(view.entity(id).must_be("entity").name, "dep:Foo.target");
    let call = view.calls_to(resolved).next().must_be("offload call");
    assert_eq!(call.role, ReferenceRole::Offload);
    assert!(matches!(
        target(&view, root, "!(D"),
        SemanticTarget::Local(_)
    ));
}

#[test]
fn imported_diamond_registers_sites_once_in_compiler_dfs_order() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(
        temp.path().join("shared.bend"),
        "def target() -> U32:\n  0\n",
    )
    .must_be("write shared");
    fs::write(
        temp.path().join("z.bend"),
        "import ./shared.bend as S\ndef z() -> U32:\n  S.target()\n",
    )
    .must_be("write first");
    fs::write(
        temp.path().join("a.bend"),
        "import ./shared.bend as T\ndef a() -> U32:\n  T.target()\n",
    )
    .must_be("write second");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./z.bend as Z\nimport ./a.bend as A\ndef main() -> U32:\n  Z.z()\n",
    );
    let view = view(&db, root, None);
    assert_eq!(
        view.entities()
            .iter()
            .map(|entity| entity.name.as_str())
            .collect::<Vec<_>>(),
        ["shared:target", "z:z", "a:a", "main"]
    );
    let shared = view
        .entities()
        .iter()
        .find(|entity| entity.name == "shared:target")
        .must_be("shared entity");
    assert_eq!(view.calls_to(SemanticTarget::Global(shared.id)).count(), 2);
}

#[test]
fn stale_capture_cannot_publish_after_overlay_replacement_or_close_reopen() {
    let temp = tempdir().must_be("temporary workspace");
    let path = temp.path().join("main.bend");
    let mut db = WorkspaceDb::default();
    let root = open(&mut db, &path, "def target() -> U32:\n  0\n");
    let captured = db.capture_semantic_view(root, None).must_be("capture");
    let snapshot = db
        .open_document(&uri(&path))
        .must_be("old overlay")
        .snapshot;
    db.update_open_snapshot(
        &uri(&path),
        Arc::new(DocumentSnapshot::new(
            Revision(2),
            "def other() -> U32:\n  1\n".into(),
        )),
    )
    .must_be("new overlay");
    assert!(!db.publish_semantic_view(Arc::new(captured.build()), None));
    let old = view(&db, root, None);
    db.close_document(&uri(&path)).must_be("close");
    db.set_open_document(
        Document::with_snapshot(uri(&path), "bend".into(), snapshot),
        Some(path),
    );
    assert!(!db.validate_semantic_view(&old, None));
}

#[test]
fn sibling_edit_invalidates_only_dependent_prepared_roots() {
    let temp = tempdir().must_be("temporary workspace");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, "def value() -> U32:\n  0\n").must_be("write dependency");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./dep.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let unrelated = open(
        &mut db,
        &temp.path().join("unrelated.bend"),
        "def alone() -> U32:\n  0\n",
    );
    let prepared = Arc::new(view(&db, root, None));
    let independent = Arc::new(view(&db, unrelated, None));
    assert!(db.publish_semantic_view(prepared.clone(), None));
    assert!(db.publish_semantic_view(independent.clone(), None));
    open(&mut db, &dep, "def value() -> U32:\n  1\n");
    assert!(db.semantic_view(root).is_none());
    assert!(Arc::ptr_eq(
        &db.semantic_view(unrelated).must_be("unrelated view"),
        &independent
    ));
    assert!(!db.validate_semantic_view(&prepared, None));
}

#[test]
fn prelude_identity_change_rejects_staged_view_and_invalidation_drops_cache() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import Base\ndef main() -> U32:\n  U32.add(1, 2)\n",
    );
    let original = base(&temp.path().join("Base.bend"));
    let prepared = Arc::new(view(&db, root, Some(original.clone())));
    assert!(db.publish_semantic_view(prepared.clone(), Some(&original)));
    let mut newer = original.clone();
    newer.generation += 1;
    assert!(!db.validate_semantic_view(&prepared, Some(&newer)));
    db.invalidate_semantic_base();
    assert!(db.semantic_view(root).is_none());
    assert!(
        prepared
            .target_site(target(&prepared, root, "U32.add"))
            .is_some()
    );
}

#[test]
fn closing_last_root_releases_view_ownership_without_invalidating_reader() {
    let temp = tempdir().must_be("temporary workspace");
    let path = temp.path().join("main.bend");
    let mut db = WorkspaceDb::default();
    let root = open(&mut db, &path, "def main() -> U32:\n  0\n");
    let reader = Arc::new(view(&db, root, None));
    let snapshot = Arc::downgrade(&reader.document(root).must_be("source facts").snapshot);
    assert!(db.publish_semantic_view(reader.clone(), None));
    db.close_document(&uri(&path)).must_be("close");
    db.load_reachable(&[root]);
    assert!(db.semantic_view(root).is_none());
    assert!(snapshot.upgrade().is_some());
    assert_eq!(reader.entities().len(), 1);
    drop(reader);
    assert!(snapshot.upgrade().is_none());
}

#[test]
fn conflicting_cross_entry_interpretations_refuse_rename() {
    let temp = tempdir().must_be("temporary workspace");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, "def use() -> U32:\n  U32.add(1, 2)\n").must_be("write shared file");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import Base\nimport ./dep.bend as D\ndef main() -> U32:\n  D.use()\n",
    );
    let dep_id = db.file_id_by_path(&dep).must_be("dependency");
    let prelude = base(&temp.path().join("Base.bend"));
    let inherited = view(&db, root, Some(prelude.clone()));
    let standalone = view(&db, dep_id, Some(prelude));
    assert!(inherited.rename_conflicts(target(&inherited, dep_id, "U32.add"), &standalone));
}

#[test]
fn overlay_replaces_imported_targets_without_reparsing_unchanged_entry() {
    let temp = tempdir().must_be("temporary workspace");
    let path = temp.path().join("main.bend");
    let dep = temp.path().join("dep.bend");
    fs::write(&dep, "def value() -> U32:\n  0\n").must_be("disk export");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &path,
        "import ./dep.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let before = view(&db, root, None);
    let entry = before
        .document(root)
        .must_be("entry facts")
        .snapshot
        .clone();
    open(&mut db, &dep, "def replacement() -> U32:\n  1\n");
    let after = view(&db, root, None);
    assert!(
        before
            .target_site(target(&before, root, "D.value"))
            .is_some()
    );
    assert_eq!(target(&after, root, "D.value"), SemanticTarget::Unresolved);
    assert!(Arc::ptr_eq(
        &entry,
        &after.document(root).must_be("same entry facts").snapshot
    ));
    db.close_document(&uri(&dep)).must_be("close overlay");
    db.load_reachable(&[root]);
    let restored = view(&db, root, None);
    assert!(
        restored
            .target_site(target(&restored, root, "D.value"))
            .is_some()
    );
    assert!(!db.validate_semantic_view(&before, None));
}

#[test]
fn invalid_law_fill_is_not_grouped_and_duplicate_alias_keeps_explicit_issue() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(
        temp.path().join("claims.bend"),
        "law identity:\n  for x: U32\n  U32\n",
    )
    .must_be("claims");
    fs::write(
        temp.path().join("other.bend"),
        "def identity(x: U32) -> U32:\n  x\n",
    )
    .must_be("other");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./claims.bend as C\nimport ./other.bend as C\ndef C.identity(x: U32) -> U32:\n  x\n",
    );
    let view = view(&db, root, None);
    let law = view
        .entities()
        .iter()
        .find(|entity| entity.name == "claims:identity")
        .must_be("law");
    assert!(law.body_owner.is_none());
    assert_eq!(law.sites.len(), 1);
    assert!(
        view.declarations()
            .iter()
            .any(|declaration| declaration.site.file == root
                && declaration.target == SemanticTarget::Unresolved)
    );
    assert!(!view.issues().is_empty());
}

#[test]
fn law_template_arity_survives_interleaved_constructors_and_rejected_declarations() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let source = concat!(
        "type Marker is Data:\n  Marker{}\n",
        "law needs_two:\n  for ~A: Kind(0)\n  for ~B: Kind(0)\n  A\n",
        "law accepts_one:\n  for ~A: Kind(0)\n  for value: A\n  A\n",
        "def duplicate(x):\n  x\n",
        "def duplicate(x):\n  x\n",
        "def needs_two(A):\n  A\n",
        "def accepts_one(A, value):\n  value\n",
    );
    let root = open(&mut db, &temp.path().join("main.bend"), source);
    let view = view(&db, root, None);
    for (name, filled) in [("needs_two", false), ("accepts_one", true)] {
        let entity = view
            .entities()
            .iter()
            .find(|entity| entity.name == name)
            .must_be("law signature");
        let body_start = source
            .find(&format!("def {name}("))
            .must_be("written implementation")
            + "def ".len();
        assert_eq!(
            entity.body_owner.map(|owner| owner.range.start),
            filled.then_some(body_start),
        );
        let declaration = view
            .declarations()
            .iter()
            .find(|declaration| {
                declaration.site.file == root && declaration.site.range.start == body_start
            })
            .must_be("implementation declaration");
        assert_eq!(
            declaration.target,
            if filled {
                SemanticTarget::Global(entity.id)
            } else {
                SemanticTarget::Unresolved
            },
        );
        assert_eq!(
            view.resolve_at(root, entity.signature_owner.range.start)
                .must_be("signature binding")
                .target,
            SemanticTarget::Global(entity.id),
        );
    }
}

#[test]
fn rename_rejects_local_capture_and_global_collision_without_source_rescan() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "def target(x: U32) -> U32:\n  x\ndef other(x: U32) -> U32:\n  x\ndef caller(value: U32) -> U32:\n  target(value)\n",
    );
    let view = view(&db, root, None);
    let global = target(&view, root, "target");
    assert!(!view.rename_is_safe(global, "other"));
    assert!(!view.rename_is_safe(global, "value"));
    assert!(view.rename_is_safe(global, "fresh"));
    let local = target(&view, root, "value");
    assert!(!view.rename_is_safe(local, "target"));
    assert!(view.rename_is_safe(local, "fresh"));
}

#[test]
fn a_type_and_constructor_with_the_same_spelling_keep_distinct_targets() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "type Foo is Kind(0):\n  Foo{}\ndef main() -> Foo:\n  Foo{}\n",
    );
    let view = view(&db, root, None);
    let type_use = view
        .occurrences(root)
        .iter()
        .find(|occurrence| {
            occurrence.role == ReferenceRole::Type
                && view
                    .document(root)
                    .must_be("source")
                    .text
                    .get(occurrence.full_range.start..occurrence.full_range.end)
                    == Some("Foo")
        })
        .must_be("type occurrence");
    let constructor_use = view
        .occurrences(root)
        .iter()
        .find(|occurrence| occurrence.role == ReferenceRole::Constructor)
        .must_be("constructor occurrence");
    let SemanticTarget::Global(type_id) = type_use.target else {
        panic!("type target expected");
    };
    let SemanticTarget::Global(constructor_id) = constructor_use.target else {
        panic!("constructor target expected");
    };
    assert_ne!(type_id, constructor_id);
    assert_eq!(
        view.entity(type_id).must_be("type entity").kind,
        bend2_lsp::analysis::DeclarationKind::Type
    );
    assert_eq!(
        view.entity(constructor_id)
            .must_be("constructor entity")
            .kind,
        bend2_lsp::analysis::DeclarationKind::Constructor
    );
}

#[test]
fn completion_projects_nested_aliases_without_exposing_canonical_namespace_keys() {
    let temp = tempdir().must_be("temporary workspace");
    fs::create_dir(temp.path().join("nested")).must_be("nested directory");
    let dependency = temp.path().join("nested/dep.bend");
    fs::write(
        temp.path().join("nested/leaf.bend"),
        "def target() -> U32:\n  0\n",
    )
    .must_be("leaf");
    fs::write(
        &dependency,
        "import ./leaf.bend as L\ndef Foo.target() -> U32:\n  L.target()\n",
    )
    .must_be("nested module");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./nested/dep.bend as D\ndef local() -> U32:\n  D.Foo.target()\n",
    );
    let dependency_id = db.file_id_by_path(&dependency).must_be("dependency ID");
    let view = view(&db, root, None);
    let projected = view.completion_candidates(root, "D.Foo.t");
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].name, "D.Foo.target");
    assert_eq!(projected[0].target, target(&view, root, "D.Foo.target"));
    assert_eq!(
        view.completion_candidates(dependency_id, "L.tar")[0].name,
        "L.target"
    );
    assert!(view.completion_candidates(root, "tar").is_empty());
    assert_eq!(view.completion_candidates(root, "loc")[0].name, "local");
    assert!(
        view.completion_candidates(root, "")
            .iter()
            .all(|candidate| !candidate.name.contains(':'))
    );
}

#[test]
fn definite_editor_recovery_keeps_targets_while_preserving_declaration_uncertainty() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "def target: U32\n  0\ndef main: U32\n  target\n",
    );
    let view = view(&db, root, None);
    assert!(
        view.declarations()
            .iter()
            .all(|declaration| declaration.uncertain)
    );
    let resolved = target(&view, root, "target");
    assert!(matches!(resolved, SemanticTarget::Global(_)));
    assert_eq!(view.references(resolved).len(), 2);
    assert_eq!(view.completion_candidates(root, "tar")[0].target, resolved);
}

#[test]
fn dotted_global_rename_checks_the_complete_proposed_local_spelling() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "def Foo.target() -> U32:\n  0\ndef caller(Foo.other: U32) -> U32:\n  Foo.target\n",
    );
    let view = view(&db, root, None);
    let global = target(&view, root, "Foo.target");
    assert!(matches!(global, SemanticTarget::Global(_)));
    assert!(!view.rename_is_safe(global, "other"));
    assert!(view.rename_is_safe(global, "fresh"));
}

#[test]
fn readiness_tracks_only_dirty_open_roots_and_stale_publish_cannot_clear_it() {
    let temp = tempdir().must_be("temporary workspace");
    let a_path = temp.path().join("a.bend");
    let b_path = temp.path().join("b.bend");
    let mut db = WorkspaceDb::default();
    assert!(db.semantic_views_ready_for(None));
    let a = open(&mut db, &a_path, "def a() -> U32:\n  0\n");
    let b = open(&mut db, &b_path, "def b() -> U32:\n  0\n");
    let a_view = Arc::new(view(&db, a, None));
    let b_view = Arc::new(view(&db, b, None));
    assert!(!db.semantic_views_ready_for(None));
    assert!(db.publish_semantic_view(a_view.clone(), None));
    assert!(db.semantic_views_ready_for(Some(a)));
    assert!(!db.semantic_views_ready_for(Some(b)));
    assert!(!db.semantic_views_ready_for(None));
    assert!(db.publish_semantic_view(b_view, None));
    assert!(db.semantic_views_ready_for(None));
    db.update_open_snapshot(
        &uri(&a_path),
        Arc::new(DocumentSnapshot::new(
            Revision(2),
            "def a() -> U32:\n  1\n".into(),
        )),
    )
    .must_be("new root revision");
    assert!(!db.semantic_views_ready_for(Some(a)));
    assert!(db.semantic_views_ready_for(Some(b)));
    assert!(!db.publish_semantic_view(a_view, None));
    assert!(!db.semantic_views_ready_for(None));
    db.close_document(&uri(&a_path)).must_be("close dirty root");
    assert!(db.semantic_views_ready_for(Some(a)));
    assert!(db.semantic_views_ready_for(None));
}

#[test]
fn old_base_view_holds_its_backing_source_directory_until_reader_drop() {
    let root_directory = tempdir().must_be("root directory");
    let lease = Arc::new(tempdir().must_be("Base directory"));
    let base_path = lease.path().join("Base.bend");
    let mut prelude = base(&base_path);
    fs::write(&base_path, &prelude.document.text).must_be("Base source file");
    prelude.source_lease = Some(lease.clone());
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &root_directory.path().join("main.bend"),
        "import Base\ndef main() -> U32:\n  U32.add(1, 2)\n",
    );
    let reader = Arc::new(view(&db, root, Some(prelude.clone())));
    assert!(db.publish_semantic_view(reader.clone(), Some(&prelude)));
    db.invalidate_semantic_base();
    drop(prelude);
    drop(lease);
    assert!(base_path.exists());
    drop(reader);
    assert!(!base_path.exists());
}

#[cfg(unix)]
#[test]
fn symlink_imports_share_one_canonical_namespace_and_declaration() {
    let temp = tempdir().must_be("temporary workspace");
    let dependency = temp.path().join("dep.bend");
    let alias = temp.path().join("alias.bend");
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("dependency");
    std::os::unix::fs::symlink(&dependency, &alias).must_be("source symlink");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./alias.bend as A\nimport ./dep.bend as D\ndef main() -> U32:\n  A.value() + D.value()\n",
    );
    let view = view(&db, root, None);
    let a = target(&view, root, "A.value");
    let d = target(&view, root, "D.value");
    assert_eq!(a, d);
    let SemanticTarget::Global(id) = a else {
        panic!("canonical global target expected");
    };
    assert_eq!(
        view.entity(id).must_be("canonical entity").name,
        "dep:value"
    );
    assert_eq!(view.entities().len(), 2);
    assert_eq!(view.references(a).len(), 3);
}

#[cfg(unix)]
#[test]
fn symlink_overlay_uses_canonical_parent_and_invalidates_importing_views() {
    let temp = tempdir().must_be("temporary workspace");
    fs::create_dir(temp.path().join("real")).must_be("real directory");
    fs::create_dir(temp.path().join("aliases")).must_be("alias directory");
    let dependency = temp.path().join("real/dep.bend");
    let leaf = temp.path().join("real/leaf.bend");
    let alias = temp.path().join("aliases/alias.bend");
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("disk dependency");
    fs::write(&leaf, "def value() -> U32:\n  1\n").must_be("canonical sibling");
    std::os::unix::fs::symlink(&dependency, &alias).must_be("source symlink");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./aliases/alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let before = Arc::new(view(&db, root, None));
    assert!(db.publish_semantic_view(before.clone(), None));
    let alias_id = open(
        &mut db,
        &alias,
        "import ./leaf.bend as L\ndef value() -> U32:\n  L.value()\n",
    );
    assert!(db.semantic_view(root).is_none());
    assert!(!db.validate_semantic_view(&before, None));
    let overlaid = view(&db, root, None);
    let site = overlaid
        .target_site(target(&overlaid, root, "D.value"))
        .must_be("overlay target");
    assert_eq!(site.file, alias_id);
    assert_eq!(
        overlaid
            .document(site.file)
            .must_be("overlay provenance")
            .uri,
        uri(&alias)
    );
    let leaf_site = overlaid
        .target_site(target(&overlaid, alias_id, "L.value"))
        .must_be("canonical sibling target");
    assert_eq!(
        overlaid
            .document(leaf_site.file)
            .must_be("sibling source")
            .uri,
        uri(&fs::canonicalize(&leaf).must_be("canonical sibling path"))
    );
    db.close_document(&uri(&alias))
        .must_be("close alias overlay");
    db.load_reachable(&[root]);
    let restored = view(&db, root, None);
    assert_eq!(
        restored
            .target_site(target(&restored, root, "D.value"))
            .must_be("disk target")
            .file,
        alias_id
    );
}

#[cfg(unix)]
#[test]
fn differing_physical_overlays_remain_uri_coherent_and_refuse_ambiguous_edits() {
    let temp = tempdir().must_be("temporary workspace");
    let dependency = temp.path().join("dep.bend");
    let alias = temp.path().join("alias.bend");
    let real_text = "def value() -> U32:\n  1\n";
    let alias_text = "def value() -> U32:\n  2\n";
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("disk source");
    std::os::unix::fs::symlink(&dependency, &alias).must_be("source symlink");
    let mut db = WorkspaceDb::default();
    let real_id = open(&mut db, &dependency, real_text);
    let pending = db
        .capture_semantic_view(real_id, None)
        .must_be("captured single overlay");
    let alias_id = open(&mut db, &alias, alias_text);
    assert_ne!(real_id, alias_id);
    assert_eq!(
        db.open_document(&uri(&dependency))
            .must_be("real overlay")
            .text,
        real_text
    );
    assert_eq!(
        db.open_document(&uri(&alias)).must_be("alias overlay").text,
        alias_text
    );
    assert!(!db.publish_semantic_view(Arc::new(pending.build()), None));
    for (id, path, text) in [
        (real_id, &dependency, real_text),
        (alias_id, &alias, alias_text),
    ] {
        let view = view(&db, id, None);
        assert_eq!(view.document(id).must_be("requested source").uri, uri(path));
        assert_eq!(view.document(id).must_be("requested source").text, text);
        assert!(view.issues().iter().any(|issue| matches!(
            issue,
            bend2_lsp::workspace::SemanticIssue::ConflictingOverlays { .. }
        )));
        assert!(
            view.declarations()
                .iter()
                .all(|declaration| declaration.target == SemanticTarget::Ambiguous)
        );
        assert!(!view.rename_is_safe(SemanticTarget::Ambiguous, "new_name"));
    }
    db.update_open_snapshot(
        &uri(&alias),
        Arc::new(DocumentSnapshot::new(Revision(7), real_text.into())),
    )
    .must_be("make buffers agree");
    let agreed = view(&db, alias_id, None);
    assert!(agreed.issues().iter().all(|issue| !matches!(
        issue,
        bend2_lsp::workspace::SemanticIssue::ConflictingOverlays { .. }
    )));
    assert_eq!(
        agreed.document(alias_id).must_be("alias revision").revision,
        Revision(7)
    );
    let agreed = Arc::new(agreed);
    let real_view = Arc::new(view(&db, real_id, None));
    assert_eq!(
        real_view.physical_uri(real_id),
        agreed.physical_uri(alias_id)
    );
    assert!(!real_view.rename_conflicts(target(&real_view, real_id, "value"), &agreed));
    assert!(db.publish_semantic_view(real_view, None));
    assert!(db.publish_semantic_view(agreed, None));
    assert_eq!(db.views_for_file(alias_id).len(), 2);
    assert_eq!(db.views_for_file(real_id).len(), 2);
}

#[cfg(unix)]
#[test]
fn watched_symlink_retarget_refreshes_indexed_imports_without_new_source_facts() {
    let temp = tempdir().must_be("temporary workspace");
    let first = temp.path().join("first.bend");
    let second = temp.path().join("second.bend");
    let alias = temp.path().join("alias.bend");
    fs::write(&first, "def value() -> U32:\n  1\n").must_be("first target");
    fs::write(&second, "def value() -> U32:\n  2\n").must_be("second target");
    std::os::unix::fs::symlink(&first, &alias).must_be("initial symlink");
    let root_path = temp.path().join("main.bend");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &root_path,
        "import ./alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let before = Arc::new(view(&db, root, None));
    let source = db.open_document(&uri(&root_path)).must_be("root source");
    assert!(db.publish_semantic_view(before.clone(), None));
    assert_eq!(db.importer_documents_for_path(&alias).len(), 1);
    fs::remove_file(&alias).must_be("remove old symlink");
    std::os::unix::fs::symlink(&second, &alias).must_be("retarget symlink");
    assert_eq!(db.refresh_imports(root, &source.snapshot), Some(true));
    db.load_reachable(&[root]);
    let after = view(&db, root, None);
    assert!(Arc::ptr_eq(
        &source.snapshot,
        &after
            .document(root)
            .must_be("unchanged source facts")
            .snapshot
    ));
    let SemanticTarget::Global(id) = target(&after, root, "D.value") else {
        panic!("retargeted global expected");
    };
    assert_eq!(after.entity(id).must_be("new entity").name, "second:value");
    assert!(!db.validate_semantic_view(&before, None));
}

#[test]
fn body_revisions_preserve_missing_import_watches_and_replace_changed_paths() {
    let temp = tempdir().must_be("temporary workspace");
    let root_path = temp.path().join("main.bend");
    let root_uri = uri(&root_path);
    let mut db = WorkspaceDb::default();
    open(
        &mut db,
        &root_path,
        "import ./missing as D\ndef main():\n  D.value()\n",
    );
    for (revision, source, expected, retired) in [
        (
            2,
            "# shifted import offsets\nimport ./missing as D\ndef main():\n  D.value(1)\n",
            "missing",
            None,
        ),
        (
            3,
            "# shifted import offsets\nimport ./another as D\ndef main():\n  D.value(2)\n",
            "another",
            Some("missing"),
        ),
    ] {
        db.update_open_snapshot(
            &root_uri,
            Arc::new(DocumentSnapshot::new(Revision(revision), source.into())),
        )
        .must_be("update importing body");
        for path in [expected.to_owned(), format!("{expected}.bend")] {
            let importers: Vec<_> = db
                .importer_documents_for_path(&temp.path().join(path))
                .into_iter()
                .map(|document| document.uri)
                .collect();
            assert_eq!(importers.as_slice(), std::slice::from_ref(&root_uri));
        }
        if let Some(retired) = retired {
            for path in [retired.to_owned(), format!("{retired}.bend")] {
                assert!(
                    db.importer_documents_for_path(&temp.path().join(path))
                        .is_empty()
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn body_revision_re_resolves_import_symlinks_with_unchanged_watch_spelling() {
    let temp = tempdir().must_be("temporary workspace");
    let first = temp.path().join("first.bend");
    let second = temp.path().join("second.bend");
    let alias = temp.path().join("alias.bend");
    fs::write(&first, "def value() -> U32:\n  1\n").must_be("first dependency");
    fs::write(&second, "def value() -> U32:\n  2\n").must_be("second dependency");
    std::os::unix::fs::symlink(&first, &alias).must_be("initial import alias");
    let root_path = temp.path().join("main.bend");
    let root_uri = uri(&root_path);
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &root_path,
        "import ./alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let before = Arc::new(view(&db, root, None));
    assert!(db.publish_semantic_view(before.clone(), None));
    fs::remove_file(&alias).must_be("remove old import alias");
    std::os::unix::fs::symlink(&second, &alias).must_be("retarget import alias");
    let (_, changed) = db
        .update_open_snapshot(
            &root_uri,
            Arc::new(DocumentSnapshot::new(
                Revision(2),
                "import ./alias.bend as D\ndef main() -> U32:\n  D.value() + 1\n".into(),
            )),
        )
        .must_be("update body and refresh resolved imports");
    assert!(changed);
    assert!(!db.validate_semantic_view(&before, None));
    db.load_reachable(&[root]);
    let after = view(&db, root, None);
    let site = after
        .target_site(target(&after, root, "D.value"))
        .must_be("retargeted declaration");
    assert_eq!(
        after.physical_uri(site.file),
        Some(&uri(
            &fs::canonicalize(&second).must_be("physical second dependency")
        ))
    );
    let importers: Vec<_> = db
        .importer_documents_for_path(&alias)
        .into_iter()
        .map(|document| document.uri)
        .collect();
    assert_eq!(importers, [root_uri]);
}

#[test]
fn canonical_import_identity_preserves_verified_logical_uri_and_lookup_aliases() {
    let temp = tempdir().must_be("temporary workspace");
    let dependency = temp.path().join("dep.bend");
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("dependency source");
    let physical = fs::canonicalize(&dependency).must_be("physical dependency path");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./dep.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let dependency_id = db
        .file_id_by_path(&dependency)
        .must_be("logical dependency lookup");
    assert_eq!(db.file_id_by_path(&physical), Some(dependency_id));
    let view = view(&db, root, None);
    let site = view
        .target_site(target(&view, root, "D.value"))
        .must_be("resolved dependency");
    assert_eq!(site.file, dependency_id);
    assert_eq!(
        view.document(site.file)
            .must_be("logical source provenance")
            .uri,
        uri(&dependency)
    );
    assert_eq!(view.physical_uri(site.file), Some(&uri(&physical)));
}

#[cfg(unix)]
#[test]
fn retargeted_alias_does_not_move_the_real_path_import_to_the_new_file() {
    let temp = tempdir().must_be("temporary workspace");
    let first = temp.path().join("first.bend");
    let second = temp.path().join("second.bend");
    let alias = temp.path().join("alias.bend");
    fs::write(&first, "def value() -> U32:\n  1\n").must_be("first source");
    fs::write(&second, "def value() -> U32:\n  2\n").must_be("second source");
    std::os::unix::fs::symlink(&first, &alias).must_be("initial alias");
    let mut db = WorkspaceDb::default();
    let real_root = open(
        &mut db,
        &temp.path().join("real_root.bend"),
        "import ./first.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let alias_root = open(
        &mut db,
        &temp.path().join("alias_root.bend"),
        "import ./alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    open(
        &mut db,
        &temp.path().join("existing_target.bend"),
        "import ./second.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let importers = db.importer_documents_for_path(&alias);
    assert_eq!(importers.len(), 2);
    fs::remove_file(&alias).must_be("remove old alias");
    std::os::unix::fs::symlink(&second, &alias).must_be("new alias target");
    for source in importers {
        let file = db.file_id_by_uri(&source.uri).must_be("importer identity");
        db.refresh_imports(file, &source.snapshot)
            .must_be("refresh importer facts");
    }
    db.load_reachable(&[alias_root, real_root]);
    for (root, expected) in [(alias_root, &second), (real_root, &first)] {
        let view = view(&db, root, None);
        let site = view
            .target_site(target(&view, root, "D.value"))
            .must_be("physical target");
        assert_eq!(
            view.physical_uri(site.file),
            Some(&uri(
                &fs::canonicalize(expected).must_be("expected physical source")
            ))
        );
        if root == alias_root {
            assert_eq!(db.file_id_by_uri(&uri(&alias)), Some(site.file));
            assert_eq!(db.file_id_by_path(&alias), Some(site.file));
        }
    }
}

#[cfg(unix)]
#[test]
fn an_import_lookup_alias_is_not_an_open_document_until_it_is_opened_explicitly() {
    let temp = tempdir().must_be("temporary workspace");
    let real = temp.path().join("real.bend");
    let alias = temp.path().join("alias.bend");
    let original = "def value() -> U32:\n  1\n";
    fs::write(&real, original).must_be("real source");
    std::os::unix::fs::symlink(&real, &alias).must_be("source alias");
    let mut db = WorkspaceDb::default();
    let real_id = open(&mut db, &real, original);
    open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    assert!(db.cached_document(&uri(&alias)).is_some());
    assert!(db.open_document(&uri(&alias)).is_none());
    assert!(
        db.update_open_snapshot(
            &uri(&alias),
            Arc::new(DocumentSnapshot::new(
                Revision(9),
                "def incorrect() -> U32:\n  0\n".into()
            ))
        )
        .is_none()
    );
    assert!(db.close_document(&uri(&alias)).is_none());
    assert_eq!(
        db.open_document(&uri(&real))
            .must_be("real buffer remains open")
            .text,
        original
    );
    let replacement = "def value() -> U32:\n  2\n";
    let alias_id = open(&mut db, &alias, replacement);
    assert_ne!(real_id, alias_id);
    let real_buffer = db.open_document(&uri(&real)).must_be("real owned buffer");
    let alias_buffer = db.open_document(&uri(&alias)).must_be("alias owned buffer");
    assert_eq!(real_buffer.uri, uri(&real));
    assert_eq!(real_buffer.text, original);
    assert_eq!(alias_buffer.uri, uri(&alias));
    assert_eq!(alias_buffer.text, replacement);
}

#[test]
fn bare_completion_prefers_a_type_over_its_same_spelling_constructor() {
    let temp = tempdir().must_be("temporary workspace");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "type Thing is Kind(0):\n  Thing{}\ndef main() -> Thing:\n  Thing{}\n",
    );
    let view = view(&db, root, None);
    let candidates = view.completion_candidates(root, "Thi");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].name, "Thing");
    let SemanticTarget::Global(id) = candidates[0].target else {
        panic!("type completion target expected");
    };
    assert_eq!(
        view.entity(id).must_be("type completion entity").kind,
        bend2_lsp::analysis::DeclarationKind::Type
    );
    let constructor = view
        .occurrences(root)
        .iter()
        .find(|occurrence| occurrence.role == ReferenceRole::Constructor)
        .must_be("constructor use");
    assert_ne!(constructor.target, candidates[0].target);
}

#[test]
fn base_can_fill_its_own_laws_but_an_importing_entry_cannot_fill_native_claims() {
    let temp = tempdir().must_be("temporary workspace");
    let prelude = SemanticBase {
        document: Document::new(
            uri(&temp.path().join("Base.bend")),
            "bend".into(),
            Revision::UNVERSIONED,
            concat!(
                "law U32: Kind(0)\n",
                "def helper(x: U32) -> U32:\n  x\n",
                "law claim:\n  for x: U32\n  U32\n",
                "def claim(x):\n  helper(x)\n",
                "law external_claim:\n  for x: U32\n  U32\n",
            )
            .into(),
        ),
        generation: 1,
        provenance: Arc::from("Base registration-order regression"),
        source_lease: None,
    };
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import Base\ndef use(x: U32) -> U32:\n  claim(x)\n",
    );
    let valid = view(&db, root, Some(prelude.clone()));
    let claim = valid
        .entities()
        .iter()
        .find(|entity| entity.name == "claim")
        .must_be("Base claim");
    assert!(claim.base);
    assert_eq!(claim.sites.len(), 2);
    let body = claim.body_owner.must_be("Base claim implementation");
    assert_eq!(body.file, claim.declaration.file);
    assert_ne!(body.range, claim.declaration.range);
    assert_eq!(claim.signature_owner, claim.declaration);
    assert!(valid.issues().is_empty());
    assert_eq!(valid.calls_from(claim.id).count(), 1);
    assert!(
        valid
            .calls_to(SemanticTarget::Global(claim.id))
            .any(|call| call.site.file == root)
    );
    let illegal = open(
        &mut db,
        &temp.path().join("illegal.bend"),
        "import Base\ndef external_claim(x):\n  x\n",
    );
    let invalid = view(&db, illegal, Some(prelude));
    let native = invalid
        .entities()
        .iter()
        .find(|entity| entity.name == "external_claim")
        .must_be("unfilled native claim");
    assert!(native.body_owner.is_none());
    assert_eq!(native.sites.len(), 1);
    assert!(
        invalid
            .declarations()
            .iter()
            .any(|declaration| declaration.site.file == illegal
                && declaration.target == SemanticTarget::Unresolved)
    );
    assert!(invalid.issues().iter().any(|issue| matches!(issue,
        bend2_lsp::workspace::SemanticIssue::InvalidDeclaration { site } if site.file == illegal)));
}

#[test]
fn pinned_base_law_implementations_have_effective_bodies_and_indexed_calls() {
    let temp = tempdir().must_be("temporary workspace");
    let source = include_str!("fixtures/semantic/compiler/base.bend");
    let prelude = SemanticBase {
        document: Document::new(
            Url::parse("bend-base:///Base.bend").must_be("Base URI"),
            "bend".into(),
            Revision::UNVERSIONED,
            source.into(),
        ),
        generation: 1,
        provenance: Arc::from("pinned compiler Base fixture"),
        source_lease: None,
    };
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("proof.bend"),
        concat!(
            "import Base\n",
            "def prove() -> {U32.add(1, 2) == U32.add(2, 1) : U32}:\n",
            "  U32.add_comm(1, 2)\n",
        ),
    );
    let view = view(&db, root, Some(prelude));
    let claim = view
        .entities()
        .iter()
        .find(|entity| entity.name == "U32.add_comm")
        .must_be("pinned Base law");
    let body = claim.body_owner.must_be("pinned Base law implementation");
    let law_start = source.find("law U32.add_comm").must_be("written Base law") + "law ".len();
    let def_start = source.find("def U32.add_comm").must_be("written Base def") + "def ".len();
    assert_eq!(claim.declaration.range.start, law_start);
    assert_eq!(body.range.start, def_start);
    assert_eq!(body.file, claim.declaration.file);
    assert_eq!(claim.sites.len(), 2);
    assert_eq!(claim.signature_owner, claim.declaration);
    let invalid: Vec<_> = view
        .issues()
        .iter()
        .filter_map(|issue| match issue {
            bend2_lsp::workspace::SemanticIssue::InvalidDeclaration { site }
                if site.file == body.file =>
            {
                Some((
                    source.get(site.range.start..site.range.end),
                    view.document(body.file)
                        .must_be("Base facts")
                        .syntax
                        .declaration_sites()
                        .iter()
                        .find(|declaration| declaration.name_range == site.range)
                        .copied(),
                ))
            }
            _ => None,
        })
        .collect();
    assert!(
        invalid.is_empty(),
        "Invalid pinned Base declarations: {invalid:#?}"
    );
    assert!(
        view.calls_to(SemanticTarget::Global(claim.id))
            .any(|call| call.site.file == root)
    );
    assert!(view.calls_from(claim.id).any(|call| {
        match call.target {
            SemanticTarget::Global(id) => view
                .entity(id)
                .is_some_and(|entity| entity.name == "Word.add_comm"),
            _ => false,
        }
    }));
    assert!(
        view.declarations()
            .iter()
            .filter(|declaration| declaration.site.file == body.file
                && (declaration.site.range.start == law_start
                    || declaration.site.range.start == def_start))
            .all(|declaration| declaration.target == SemanticTarget::Global(claim.id))
    );
}

#[test]
fn fuzzy_completion_stays_inside_the_written_alias_and_exact_qualifier() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(
        temp.path().join("dep.bend"),
        "def Widget.readValue() -> U32:\n  0\ndef Widget.resetState() -> U32:\n  0\ndef Other.readValue() -> U32:\n  0\n",
    )
    .must_be("dependency exports");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./dep.bend as D\ndef readValue() -> U32:\n  0\ndef main() -> U32:\n  0\n",
    );
    let view = view(&db, root, None);
    let labels = |query| {
        view.completion_candidates(root, query)
            .into_iter()
            .map(|candidate| candidate.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(labels("D.Widget.rdV"), ["D.Widget.readValue"]);
    assert_eq!(labels("D.Other.rdV"), ["D.Other.readValue"]);
    assert!(labels("D.Wdgt.rdV").is_empty());
    assert!(labels("Missing.Widget.rdV").is_empty());
    assert_eq!(labels("rdV"), ["readValue"]);
}

#[test]
fn prepared_library_identity_invalidates_only_affected_immutable_views() {
    let temp = tempdir().must_be("temporary workspace");
    let library = temp.path().join("library");
    fs::create_dir(&library).must_be("library directory");
    let library = fs::canonicalize(&library).must_be("physical library directory");
    let mut db = WorkspaceDb::default();
    db.set_semantic_library_path(Some(library.clone()));
    let affected = open(
        &mut db,
        &library.join("entry.bend"),
        "def value() -> U32:\n  0\n",
    );
    let unrelated = open(
        &mut db,
        &temp.path().join("main.bend"),
        "def main() -> U32:\n  0\n",
    );
    let reader = Arc::new(view(&db, affected, None));
    let stable = Arc::new(view(&db, unrelated, None));
    let captured = db
        .capture_semantic_view(affected, None)
        .must_be("cold capture");
    assert!(db.publish_semantic_view(reader.clone(), None));
    assert!(db.publish_semantic_view(stable.clone(), None));

    let next_library = library
        .parent()
        .must_be("physical workspace")
        .join("other_library");
    db.set_semantic_library_path(Some(next_library));
    assert!(db.semantic_view(affected).is_none());
    assert!(!db.validate_semantic_view(&reader, None));
    assert!(!db.publish_semantic_view(Arc::new(captured.build()), None));
    assert!(Arc::ptr_eq(
        &stable,
        &db.semantic_view(unrelated)
            .must_be("unaffected prepared view")
    ));
    assert_eq!(
        reader.document(affected).must_be("old reader").text,
        "def value() -> U32:\n  0\n"
    );
}

#[test]
fn library_changes_reconsider_native_edges_omitted_from_the_old_view() {
    let temp = tempdir().must_be("temporary workspace");
    let library = temp.path().join("library");
    fs::create_dir(&library).must_be("library directory");
    let dependency = library.join("value.bend");
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("dependency source");
    let library = fs::canonicalize(&library).must_be("physical library directory");
    let mut db = WorkspaceDb::default();
    db.set_semantic_library_path(Some(library.clone()));
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./library/value.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let dependency = db
        .file_id_by_path(&dependency)
        .must_be("dependency identity");
    let rejected = Arc::new(view(&db, root, None));
    let pending = db
        .capture_semantic_view(root, None)
        .must_be("rejected capture");
    assert!(rejected.document(dependency).is_none());
    assert_eq!(
        target(&rejected, root, "D.value"),
        SemanticTarget::Unresolved
    );
    assert!(db.publish_semantic_view(rejected.clone(), None));

    db.set_semantic_library_path(None);
    assert!(db.semantic_view(root).is_none());
    assert!(!db.validate_semantic_view(&rejected, None));
    assert!(!db.publish_semantic_view(Arc::new(pending.build()), None));
    let accepted = Arc::new(view(&db, root, None));
    assert!(accepted.document(dependency).is_some());
    assert!(matches!(
        target(&accepted, root, "D.value"),
        SemanticTarget::Global(_)
    ));
    assert!(db.publish_semantic_view(accepted.clone(), None));

    db.set_semantic_library_path(Some(library));
    assert!(db.semantic_view(root).is_none());
    assert!(!db.validate_semantic_view(&accepted, None));
    assert_eq!(
        target(&rejected, root, "D.value"),
        SemanticTarget::Unresolved
    );
    assert_eq!(
        target(&view(&db, root, None), root, "D.value"),
        SemanticTarget::Unresolved
    );
}

#[test]
fn physical_uri_tracks_path_removal_and_reattachment_without_rewriting_logical_uri() {
    let temp = tempdir().must_be("temporary workspace");
    let path = temp.path().join("identity.bend");
    let source = "def identity(value):\n  value\n";
    fs::write(&path, source).must_be("physical source");
    let physical_path = fs::canonicalize(&path).must_be("canonical source");
    let physical_uri = uri(&physical_path);
    let prefix = physical_uri
        .as_str()
        .strip_suffix("identity.bend")
        .must_be("source URI prefix");
    let logical_uri = Url::parse(&format!("{prefix}%69dentity.bend")).must_be("escaped source URI");
    let mut db = WorkspaceDb::default();
    let root = db.set_open_document(
        Document::new(
            logical_uri.clone(),
            "bend".into(),
            Revision(1),
            source.into(),
        ),
        Some(physical_path.clone()),
    );
    let before = Arc::new(view(&db, root, None));
    assert_eq!(before.physical_uri(root), Some(&physical_uri));
    assert_eq!(before.physical_file_id(&physical_uri), Some(root));
    assert!(db.publish_semantic_view(before.clone(), None));

    assert_eq!(
        db.set_open_document(
            Document::new(
                logical_uri.clone(),
                "bend".into(),
                Revision(2),
                source.into()
            ),
            None,
        ),
        root
    );
    assert!(!db.validate_semantic_view(&before, None));
    let detached = view(&db, root, None);
    assert_eq!(detached.physical_uri(root), Some(&logical_uri));
    assert_eq!(detached.physical_file_id(&physical_uri), None);
    assert_eq!(
        detached.document(root).must_be("detached source").uri,
        logical_uri
    );

    db.set_open_document(
        Document::new(
            logical_uri.clone(),
            "bend".into(),
            Revision(3),
            source.into(),
        ),
        Some(physical_path),
    );
    let reattached = view(&db, root, None);
    assert_eq!(reattached.physical_uri(root), Some(&physical_uri));
    assert_eq!(
        reattached.document(root).must_be("reattached source").uri,
        logical_uri
    );
    assert_eq!(detached.physical_uri(root), Some(&logical_uri));
    assert_eq!(before.physical_uri(root), Some(&physical_uri));
}

#[cfg(unix)]
#[test]
fn replacing_an_overlay_recaptures_a_retargeted_symlinks_physical_identity() {
    let temp = tempdir().must_be("temporary workspace");
    let first = temp.path().join("first.bend");
    let second = temp.path().join("second.bend");
    let alias = temp.path().join("alias.bend");
    let source = "def value() -> U32:\n  0\n";
    fs::write(&first, source).must_be("first source");
    fs::write(&second, source).must_be("second source");
    let first_physical = uri(&fs::canonicalize(&first).must_be("physical first source"));
    let second_physical = uri(&fs::canonicalize(&second).must_be("physical second source"));
    std::os::unix::fs::symlink(&first, &alias).must_be("initial alias");
    let mut db = WorkspaceDb::default();
    let root = open(&mut db, &alias, source);
    let before = Arc::new(view(&db, root, None));
    let pending = db.capture_semantic_view(root, None).must_be("old capture");
    assert!(db.publish_semantic_view(before.clone(), None));
    assert_eq!(before.physical_uri(root), Some(&first_physical));

    fs::remove_file(&alias).must_be("remove initial alias");
    std::os::unix::fs::symlink(&second, &alias).must_be("retarget alias");
    db.update_open_snapshot(
        &uri(&alias),
        Arc::new(DocumentSnapshot::new(Revision(2), source.into())),
    )
    .must_be("replace overlay");
    assert!(db.semantic_view(root).is_none());
    assert!(!db.publish_semantic_view(Arc::new(pending.build()), None));
    let after = view(&db, root, None);
    assert_eq!(after.physical_uri(root), Some(&second_physical));
    assert_eq!(
        after.document(root).must_be("logical overlay").uri,
        uri(&alias)
    );
    assert_eq!(before.physical_uri(root), Some(&first_physical));
}

#[test]
fn indexed_parameter_hints_use_law_signature_ownership_and_inclusive_ranges() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(
        temp.path().join("claims.bend"),
        "law identity:\n  for original: U32\n  U32\n",
    )
    .must_be("law signature");
    let mut db = WorkspaceDb::default();
    let source = "import Base\nimport ./claims.bend as C\ndef C.identity(renamed):\n  renamed\ndef main() -> U32:\n  C.identity(7)\n";
    let root = open(&mut db, &temp.path().join("main.bend"), source);
    let view = view(&db, root, Some(base(&temp.path().join("Base.bend"))));
    let argument = source.rfind('7').must_be("argument offset");
    assert_eq!(
        view.parameter_hints(
            root,
            bend2_lsp::analysis::TextRange::new(argument, argument)
        ),
        [(argument, "original: ".into())]
    );
    assert!(
        view.parameter_hints(root, bend2_lsp::analysis::TextRange::new(0, argument - 1))
            .is_empty()
    );
    assert!(
        view.parameter_hints(
            root,
            bend2_lsp::analysis::TextRange::new(argument + 1, source.len())
        )
        .is_empty()
    );
}

#[test]
fn alias_rename_simulates_complete_atoms_and_only_actual_new_prefix_capture() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(
        temp.path().join("dep.bend"),
        "def target(x: U32) -> U32:\n  x\n",
    )
    .must_be("aliased dependency");
    for (source, safe) in [
        (
            "import ./dep.bend as D\ndef use(D: U32) -> U32:\n  D.target(0)\n",
            true,
        ),
        (
            "import ./dep.bend as D\ndef use(E: U32) -> U32:\n  D.target(0)\n",
            true,
        ),
        (
            "import ./dep.bend as D\ndef use(E.target: U32) -> U32:\n  D.target(0)\n",
            false,
        ),
        (
            "import ./dep.bend as D\ndef other() -> U32:\n  E.missing\ndef use() -> U32:\n  D.target(0)\n",
            true,
        ),
        (
            "import ./dep.bend as D\ndef other() -> U32:\n  E.target\ndef use() -> U32:\n  D.target(0)\n",
            false,
        ),
        (
            "import ./dep.bend as D\ndef E.target(x: U32) -> U32:\n  x\ndef use() -> U32:\n  D.target(0)\n",
            false,
        ),
        (
            "import ./dep.bend as D\ndef E.unrelated(x: U32) -> U32:\n  x\ndef use() -> U32:\n  D.target(0)\n",
            false,
        ),
    ] {
        let mut db = WorkspaceDb::default();
        let root = open(&mut db, &temp.path().join("main.bend"), source);
        let view = view(&db, root, None);
        let document = view.document(root).must_be("entry source");
        let alias = document.syntax.imports()[0].alias.must_be("module alias");
        let ranges = view.alias_rename_ranges(root, alias, "E");
        assert_eq!(ranges.is_some(), safe, "source: {source}");
        if let Some(ranges) = ranges {
            assert_eq!(ranges.len(), 2, "source: {source}");
            assert_eq!(ranges[0], alias);
            assert_eq!(
                source.get(ranges[1].start..ranges[1].end),
                Some("D"),
                "source: {source}"
            );
        }
    }
}

#[test]
fn alias_rename_validates_native_alias_words_not_coloring_keywords() {
    let temp = tempdir().must_be("temporary workspace");
    fs::write(temp.path().join("dep.bend"), "def target() -> U32:\n  0\n")
        .must_be("aliased dependency");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./dep.bend as D\ndef main() -> U32:\n  D.target()\n",
    );
    let view = view(&db, root, None);
    let alias = view.document(root).must_be("entry source").syntax.imports()[0]
        .alias
        .must_be("alias range");
    for valid in ["as", "unsafe", "def", "Type", "_", "module_2"] {
        assert!(
            view.alias_rename_ranges(root, alias, valid).is_some(),
            "{valid}"
        );
    }
    for invalid in ["", "2module", "module.member", "module-name", "módulo"] {
        assert!(
            view.alias_rename_ranges(root, alias, invalid).is_none(),
            "{invalid}"
        );
    }
}

#[test]
fn unresolved_and_editor_only_aliases_keep_complete_atom_rename_safety() {
    let temp = tempdir().must_be("temporary workspace");
    for imported in ["./missing.bend", "./módulo.bend", "./module"] {
        for (binder, safe, uses) in [
            ("D", true, 1),
            ("D.target", true, 0),
            ("E", true, 1),
            ("E.target", false, 0),
        ] {
            let source =
                format!("import {imported} as D\ndef use({binder}: U32) -> U32:\n  D.target\n");
            let mut db = WorkspaceDb::default();
            let root = open(&mut db, &temp.path().join("main.bend"), &source);
            let view = view(&db, root, None);
            let alias = view.document(root).must_be("entry source").syntax.imports()[0]
                .alias
                .must_be("alias range");
            let ranges = view.alias_rename_ranges(root, alias, "E");
            assert_eq!(ranges.is_some(), safe, "{source}");
            if let Some(ranges) = ranges {
                assert_eq!(ranges.len(), uses + 1, "{source}");
                assert_eq!(ranges[0], alias);
                assert!(
                    ranges
                        .iter()
                        .all(|range| { source.get(range.start..range.end) == Some("D") })
                );
            }
        }
    }
}

#[test]
fn invalid_import_headers_do_not_acquire_disk_or_base_ownership() {
    let temp = tempdir().must_be("temporary workspace");
    let dependency = temp.path().join("dep.bend");
    fs::write(&dependency, "def value() -> U32:\n  0\n").must_be("dependency source");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import Base as X junk\nimport ./dep.bend as D junk\ndef main() -> U32:\n  D.value()\n",
    );
    assert!(db.cached_document(&uri(&dependency)).is_none());
    let input = db
        .capture_semantic_view(root, Some(base(&temp.path().join("Base.bend"))))
        .must_be("invalid-header capture");
    assert!(!input.requires_base());
    let view = input.build();
    assert_eq!(view.documents().len(), 1);
    assert_eq!(target(&view, root, "D.value"), SemanticTarget::Unresolved);
    assert_eq!(
        view.issues()
            .iter()
            .filter(|issue| matches!(
                issue,
                bend2_lsp::workspace::SemanticIssue::MissingImport { .. }
            ))
            .count(),
        2
    );
}

#[cfg(unix)]
#[test]
fn invalid_real_import_namespace_does_not_pull_in_a_child_or_its_base() {
    let temp = tempdir().must_be("temporary workspace");
    let rejected = temp.path().join("invalid.name.bend");
    let alias = temp.path().join("alias.bend");
    fs::write(&rejected, "import Base\ndef value() -> U32:\n  0\n")
        .must_be("invalid physical namespace source");
    std::os::unix::fs::symlink(&rejected, &alias).must_be("validly spelled alias");
    let mut db = WorkspaceDb::default();
    let root = open(
        &mut db,
        &temp.path().join("main.bend"),
        "import ./alias.bend as D\ndef main() -> U32:\n  D.value()\n",
    );
    let input = db
        .capture_semantic_view(root, Some(base(&temp.path().join("Base.bend"))))
        .must_be("captured invalid namespace");
    assert!(!input.requires_base());
    let view = input.build();
    assert_eq!(view.documents().len(), 1);
    assert_eq!(target(&view, root, "D.value"), SemanticTarget::Unresolved);
    assert!(view.issues().iter().any(|issue| matches!(
        issue,
        bend2_lsp::workspace::SemanticIssue::MissingImport { file, .. } if *file == root
    )));
}
