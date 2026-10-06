// Appended only to an exported disposable semantic.rs; never production source.
pub mod lifecycle_diagnostic {
    use super::*;
    use crate::analysis::Revision;
    use crate::workspace::resolve_import_targets;
    use std::fmt::Write;
    use std::hint::black_box;

    struct Occurrences {
        snapshot: Arc<DocumentSnapshot>,
        imports_prelude: bool,
        targets: Vec<PreparedTarget>,
        indices: HashMap<TemplateTarget, usize>,
        call_targets: Vec<usize>,
        occurrences: Vec<ReferenceOrdinal>,
    }

    impl Occurrences {
        fn empty(snapshot: Arc<DocumentSnapshot>, imports_prelude: bool) -> Self {
            Self { snapshot, imports_prelude, targets: Vec::new(), indices: HashMap::new(),
                call_targets: Vec::new(), occurrences: Vec::new() }
        }

        fn populate(&mut self) {
            prepare_occurrences(&self.snapshot, &mut self.targets, &mut self.indices,
                &mut self.call_targets, &mut self.occurrences);
        }
    }

    // Only diagnostic wrappers are non-inlined. Production callees are untouched.
    #[inline(never)]
    fn stage_occurrences(state: &mut Occurrences) { state.populate(); }

    #[inline(never)]
    fn stage_calls(state: &mut Occurrences) -> PreparedCalls {
        prepare_calls(&state.snapshot, state.imports_prelude, &mut state.targets,
            &mut state.indices, &mut state.call_targets)
    }

    #[inline(never)]
    fn stage_prepare(snapshot: Arc<DocumentSnapshot>) -> PreparedSemanticSnapshot {
        prepare_semantic_snapshot(snapshot)
    }

    #[inline(never)]
    fn stage_import_lookup(snapshot: &DocumentSnapshot, alias: &str) -> Option<TemplateModule> {
        template_module(snapshot, alias, false)
    }

    #[inline(never)]
    fn stage_install_initial(db: &mut WorkspaceDb, source: FileId, prepared: PreparedSemanticSnapshot, imports_changed: bool) {
        black_box(1_u8); // Distinct identity prevents merged diagnostic install entries.
        db.install_semantics(source, Some(prepared), imports_changed);
    }

    #[inline(never)]
    fn stage_install_replace(db: &mut WorkspaceDb, source: FileId, prepared: PreparedSemanticSnapshot, imports_changed: bool) {
        black_box(2_u8);
        db.install_semantics(source, Some(prepared), imports_changed);
    }

    #[inline(never)]
    fn stage_install_local(db: &mut WorkspaceDb, source: FileId, prepared: PreparedSemanticSnapshot, imports_changed: bool) {
        black_box(3_u8);
        db.install_semantics(source, Some(prepared), imports_changed);
    }

    #[inline(never)]
    fn stage_remove(db: &mut WorkspaceDb, source: FileId) { db.semantic.remove(source); }

    #[inline(never)]
    fn stage_bind(db: &mut WorkspaceDb, source: FileId, snapshot: &DocumentSnapshot,
        target: TemplateTarget) -> Option<TargetKey> {
        db.bind_target(source, snapshot, target)
    }

    struct Fixture {
        directory: tempfile::TempDir,
        db: WorkspaceDb,
        source: FileId,
        snapshot: Arc<DocumentSnapshot>,
        occurrence_count: usize,
        call_count: usize,
        alias: &'static str,
        imports_changed: bool,
    }

    fn common_source() -> String {
        let mut text = String::from("def identity(value):\n  value\n");
        for index in 0..6 {
            writeln!(text, "def common_worker_{index:02}(value):\n  identity(value)").unwrap();
        }
        text
    }

    fn source_text(case: &str) -> (String, usize, usize, &'static str) {
        match case {
            "common" => (common_source(), 0, 0, ""),
            "root" => {
                let mut text = String::new();
                for target in 1..100 {
                    let alias = if target == 99 { "Common".into() } else { format!("File{target:03}") };
                    writeln!(text, "import ./f{target:03}.bend as {alias}").unwrap();
                }
                for function in 0..6 {
                    writeln!(text, "def worker_000_{function:02}(value):").unwrap();
                    text.push_str("  ");
                    for _ in 0..8 { text.push_str("Common.identity("); }
                    text.push_str("value");
                    for _ in 0..8 { text.push(')'); }
                    text.push('\n');
                }
                (text, 48, 48, "Common")
            }
            "aliases" => ("import ./f099.bend as A\nimport ./f099.bend as B\ndef client(value):\n  A.identity(value)\n  B.identity(value)\n  A.identity\n".into(), 3, 2, "A"),
            "interleaved" => ("import ./f099.bend as A\nimport ./f099.bend as B\ndef client(value):\n  A.identity(A.alternate(value))\n  B.identity(value)\n  A.identity\ndef second(value):\n  B.alternate(B.identity(value))\n  A.alternate(value)\n".into(), 7, 6, "A"),
            "shadow" => ("import ./f099.bend as Common\ndef client(value):\n  Common.identity(value)\ndef shadow(Common):\n  Common.identity(1)\n".into(), 1, 1, "Common"),
            "base" => ("import Base as Base\ndef client(value):\n  library_value(value)\n  Base.identity(value)\n  Base.identity\n".into(), 2, 2, "Base"),
            "base_unaliased" => ("import Base\ndef client(value):\n  library_value(value)\n  Base.identity(value)\n  Base.identity\n".into(), 0, 2, "Base"),
            _ => panic!("unknown fixed fixture"),
        }
    }

    fn fixture(case: &str, replacement: bool) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let mut db = WorkspaceDb::default();
        let path = directory.path().join("f000.bend");
        let dep_path = directory.path().join("f099.bend");
        let dep_uri = Url::from_file_path(&dep_path).unwrap();
        db.set_open_document(Document::new(dep_uri, "bend".into(), Revision(1),
            "def identity(value):\n  value\ndef alternate(value):\n  value\ndef library_value(value):\n  value\n".into()), Some(dep_path));
        if matches!(case, "base" | "base_unaliased") {
            let base_dir = tempfile::tempdir().unwrap();
            let base_path = base_dir.path().join("Base.bend");
            let base_snapshot = Arc::new(DocumentSnapshot::new(Revision(1),
                "def identity(value):\n  value\ndef library_value(value):\n  value\n".into()));
            db.register_compiler_document(Url::from_file_path(&base_path).unwrap(), base_path,
                prepare_semantic_snapshot(base_snapshot), base_dir);
        }
        let (text, occurrence_count, call_count, alias) = source_text(case);
        let uri = Url::from_file_path(&path).unwrap();
        let old = Arc::new(DocumentSnapshot::new(Revision(1), text.clone()));
        let source = if replacement {
            db.set_open_document(Document::with_snapshot(uri, "bend".into(), old.clone()), Some(path.clone()))
        } else {
            let source = db.intern(uri, Some(path.clone()));
            db.entries[source.0].language_id = "bend".into();
            source
        };
        if replacement {
            assert_eq!(db.semantic.contribution(source).is_some(), occurrence_count != 0 || call_count != 0);
            let prepared = prepare_semantic_snapshot(old);
            assert_eq!(prepared.occurrences.len(), occurrence_count);
            assert_eq!(prepared.call_indices.len(), call_count);
        }
        let snapshot = Arc::new(DocumentSnapshot::new(Revision(if replacement { 2 } else { 1 }),
            if replacement { format!("{text}# replacement\n") } else { text }));
        db.entries[source.0].open_snapshot = Some(snapshot.clone());
        let imports = resolve_import_targets(&path, &snapshot);
        let imports_changed = db.refresh_imports_with_targets(source, imports);
        // Freeze primitive installation state: import maps/effective snapshot already
        // updated, old semantic snapshot/epoch/contribution still intact on replacement.
        Fixture { directory, db, source, snapshot, occurrence_count, call_count, alias, imports_changed }
    }

    fn check_prepared(prepared: &PreparedSemanticSnapshot, fixture: &Fixture) {
        assert!(Arc::ptr_eq(&prepared.snapshot, &fixture.snapshot));
        assert_eq!(prepared.occurrences.len(), fixture.occurrence_count);
        assert_eq!(prepared.call_indices.len(), fixture.call_count);
        assert_eq!(prepared.imports_prelude, fixture.alias == "Base");
        for target in &prepared.targets {
            assert!(target.occurrences.end <= prepared.occurrences.len());
            assert!(target.calls.end <= prepared.calls.len());
        }
    }

    pub fn semantic_lifecycle_diagnostic_main() {
        let args: Vec<_> = std::env::args().collect();
        assert_eq!(args.len(), 3, "expected stage and fixture");
        let stage = args[1].as_str();
        let case = args[2].as_str();
        let replacing = matches!(stage, "install_replace" | "install_local" | "remove");
        let mut fixture = fixture(case, replacing);
        let validation = prepare_semantic_snapshot(fixture.snapshot.clone());
        check_prepared(&validation, &fixture);
        let imports_prelude = validation.imports_prelude;
        // Validation output is destroyed before measurement, never by an entry wrapper.
        drop(validation);
        match stage {
            "occurrences" => {
                let mut state = Occurrences::empty(fixture.snapshot.clone(), imports_prelude);
                stage_occurrences(black_box(&mut state));
                assert_eq!(state.occurrences.len(), fixture.occurrence_count);
                black_box(&state);
            }
            "calls" => {
                let mut state = Occurrences::empty(fixture.snapshot.clone(), imports_prelude);
                state.populate();
                let calls = stage_calls(black_box(&mut state));
                assert_eq!(calls.call_indices.len(), fixture.call_count);
                black_box(&calls);
            }
            "prepare" => {
                let prepared = stage_prepare(black_box(fixture.snapshot.clone()));
                check_prepared(&prepared, &fixture);
                black_box(&prepared);
            }
            "import_lookup" => {
                assert!(!fixture.alias.is_empty());
                let module = stage_import_lookup(black_box(&fixture.snapshot), black_box(fixture.alias));
                match case {
                    "base" => assert!(matches!(module, Some(TemplateModule::CompilerBase))),
                    "base_unaliased" => assert!(module.is_none()),
                    _ => assert!(matches!(module, Some(TemplateModule::Import(_)))),
                }
                let _ = black_box(module);
            }
            "bind" => {
                let prepared = prepare_semantic_snapshot(fixture.snapshot.clone());
                let target = prepared.targets.first().unwrap().target;
                let key = stage_bind(black_box(&mut fixture.db), fixture.source,
                    black_box(&fixture.snapshot), black_box(target));
                assert!(key.is_some());
                let _ = black_box(key);
            }
            "remove" => {
                let before = fixture.db.semantic.stats;
                let old_counts = fixture.db.semantic.contribution(fixture.source)
                    .map_or((0, 0), |old| (old.occurrence_count, old.call_count));
                stage_remove(black_box(&mut fixture.db), fixture.source);
                assert!(fixture.db.semantic.contribution(fixture.source).is_none());
                assert!(!fixture.db.semantic.prelude_files.contains(&fixture.source));
                assert_eq!(fixture.db.semantic.stats.occurrences, before.occurrences - old_counts.0);
                assert_eq!(fixture.db.semantic.stats.calls, before.calls - old_counts.1);
            }
            "install_initial" | "install_replace" | "install_local" => {
                let prepared = prepare_semantic_snapshot(fixture.snapshot.clone());
                let mut expected_references: Vec<_> = prepared.occurrences.iter()
                    .map(|ordinal| {
                        let range = fixture.snapshot.syntax.reference_entries()[ordinal.0].range;
                        (range.start, range.end)
                    }).collect();
                let mut expected_calls: Vec<_> = prepared.call_indices.iter()
                    .map(|&index| {
                        let range = fixture.snapshot.syntax.calls()[index].callee_range;
                        (range.start, range.end)
                    }).collect();
                if matches!(case, "base" | "base_unaliased") {
                    // Literal consumer-visible name spans, independent of prepared rows.
                    let qualified: Vec<_> = fixture.snapshot.text.match_indices("Base.identity")
                        .map(|(start, _)| (start + "Base.".len(), start + "Base.identity".len())).collect();
                    assert_eq!(qualified.len(), 2);
                    let library = fixture.snapshot.text.find("library_value(").unwrap();
                    let mut literal_calls = vec![(library, library + "library_value".len()), qualified[0]];
                    let mut literal_references = if case == "base" { qualified } else { Vec::new() };
                    expected_references.sort_unstable();
                    expected_calls.sort_unstable();
                    literal_references.sort_unstable();
                    literal_calls.sort_unstable();
                    assert_eq!(expected_references, literal_references);
                    assert_eq!(expected_calls, literal_calls);
                }
                let before = fixture.db.semantic.stats.files_rebuilt;
                let old_epoch = fixture.db.entries[fixture.source.0].semantic_epoch;
                match stage {
                    "install_initial" => stage_install_initial(black_box(&mut fixture.db), fixture.source, prepared, fixture.imports_changed),
                    "install_replace" => stage_install_replace(black_box(&mut fixture.db), fixture.source, prepared, fixture.imports_changed),
                    "install_local" => stage_install_local(black_box(&mut fixture.db), fixture.source, prepared, fixture.imports_changed),
                    _ => unreachable!(),
                }
                assert_eq!(fixture.db.semantic.stats.files_rebuilt, before + 1);
                assert!(fixture.db.entries[fixture.source.0].semantic_active);
                assert!(Arc::ptr_eq(fixture.db.entries[fixture.source.0].semantic_snapshot.as_ref().unwrap(), &fixture.snapshot));
                assert_eq!(fixture.db.entries[fixture.source.0].semantic_epoch, old_epoch.and_then(|epoch| epoch.checked_add(1)));
                let contribution = fixture.db.semantic.contribution(fixture.source);
                assert_eq!(contribution.is_some(), fixture.occurrence_count != 0 || fixture.call_count != 0);
                if let Some(contribution) = contribution {
                    assert_eq!(contribution.occurrence_count, fixture.occurrence_count);
                    assert_eq!(contribution.call_count, fixture.call_count);
                }
                let target_file = if matches!(case, "base" | "base_unaliased") { fixture.db.semantic.compiler_base.unwrap() }
                    else { fixture.db.file_id_by_path(&fixture.directory.path().join("f099.bend")).unwrap() };
                let target_uri = fixture.db.entries[target_file.0].uri.clone();
                let source_uri = &fixture.db.entries[fixture.source.0].uri;
                let mut observed_references = Vec::new();
                let mut observed_calls = Vec::new();
                for name in ["identity", "alternate", "library_value"] {
                    if let Some(target) = fixture.db.symbol_by_name(&target_uri, name) {
                        observed_references.extend(fixture.db.references(target.id, false).into_iter()
                            .filter(|row| &row.document.uri == source_uri)
                            .map(|row| (row.range.start, row.range.end)));
                        observed_calls.extend(fixture.db.incoming_calls(target.id).into_iter()
                            .filter(|group| &group.source.uri == source_uri)
                            .flat_map(|group| group.ranges.into_iter().map(|range| (range.start, range.end))));
                    }
                }
                expected_references.sort_unstable();
                expected_calls.sort_unstable();
                observed_references.sort_unstable();
                observed_calls.sort_unstable();
                assert_eq!(observed_references, expected_references);
                assert_eq!(observed_calls, expected_calls);
            }
            _ => panic!("unknown fixed stage"),
        }
        println!("{}", serde_json::json!({"stage": stage, "fixture": case,
            "invariants_valid": true, "occurrences": fixture.occurrence_count,
            "calls": fixture.call_count, "snapshot_revision": fixture.snapshot.revision.0,
            "directory_retained": fixture.directory.path().is_dir()}));
        black_box(&fixture);
        // All returned columns/snapshots/DB fixture destructors run after toggled entry.
    }
}
