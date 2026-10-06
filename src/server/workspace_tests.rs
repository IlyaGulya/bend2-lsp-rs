mod tests {
    use std::{
        collections::HashSet,
        fmt::Write as _,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::analysis::{self, DocumentSnapshot, Revision};
    use crate::workspace::{Document, WorkspaceDb, normalize_path};
    use url::Url;

    fn write_graph(root: &Path) -> std::io::Result<Vec<PathBuf>> {
        let paths: Vec<PathBuf> = (0..100)
            .map(|index| root.join(format!("f{index:03}.bend")))
            .collect();
        for (index, path) in paths.iter().enumerate() {
            let mut source = String::new();
            match index {
                0 => {
                    source.push_str("import ./f001.bend as One\n");
                    source.push_str("import ./f002.bend as Two\n");
                    source.push_str("import ./missing.bend as Missing\n");
                }
                1 | 2 => source.push_str("import ./f003.bend as Shared\n"),
                3 => {
                    source.push_str("import ./f000.bend as Cycle\n");
                    source.push_str("import ./f004.bend as Next\n");
                }
                4..=98 => {
                    let _ = writeln!(source, "import ./f{:03}.bend as Next", index + 1);
                }
                _ => {}
            }
            source.push_str("def value: U32\n  1\n");
            fs::write(path, source)?;
        }
        Ok(paths)
    }

    fn file_uri(path: &Path) -> std::io::Result<Url> {
        Url::from_file_path(path).map_err(|()| std::io::Error::other("file path URI"))
    }

    #[test]
    fn loads_large_shared_cyclic_graph_and_prefers_open_snapshots() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let paths = write_graph(temp.path())?;
        let root_uri = file_uri(&paths[0])?;
        let root_text = fs::read_to_string(&paths[0])?;
        let mut database = WorkspaceDb::default();
        let root_id = database.set_open_document(
            Document::new(root_uri.clone(), "bend".into(), Revision(1), root_text),
            Some(paths[0].clone()),
        );
        database.load_reachable(std::slice::from_ref(&root_id));

        assert_eq!(database.indexed_documents().len(), 100);
        let shared = database
            .file_id_by_path(&paths[3])
            .ok_or_else(|| std::io::Error::other("shared dependency"))?;
        let dependents: HashSet<Url> = database
            .dependents(shared)
            .into_iter()
            .map(|document| document.uri)
            .collect();
        let expected: HashSet<Url> = paths[..3]
            .iter()
            .map(|path| file_uri(path))
            .collect::<std::io::Result<_>>()?;
        assert_eq!(dependents, expected);

        let missing_uri = file_uri(&temp.path().join("missing.bend"))?;
        assert!(
            database.cached_document(&missing_uri).is_none(),
            "missing imports must not become source snapshots"
        );
        let unrelated_path = temp.path().join("unrelated.bend");
        fs::write(&unrelated_path, "def unrelated: U32\n  0\n")?;
        let (unrelated, _) = database
            .sync_disk_path(&unrelated_path, Some("def unrelated: U32\n  0\n".into()))
            .ok_or_else(|| std::io::Error::other("cache unrelated source"))?;
        assert!(database.dependents(unrelated).is_empty());

        let dependency_uri = file_uri(&paths[1])?;
        let unsaved = "def unsaved: U32\n  9\n";
        let dependency_id = database.set_open_document(
            Document::new(
                dependency_uri.clone(),
                "bend".into(),
                Revision(7),
                unsaved.into(),
            ),
            Some(paths[1].clone()),
        );
        database.load_reachable(std::slice::from_ref(&dependency_id));
        let cached = database
            .cached_document(&dependency_uri)
            .ok_or_else(|| std::io::Error::other("open dependency snapshot"))?;
        assert_eq!(cached.text, unsaved);
        let source_graph = database
            .source_graph(root_id)
            .ok_or_else(|| std::io::Error::other("root source graph"))?;
        let overlay = source_graph
            .nodes
            .iter()
            .find(|node| node.path == normalize_path(&paths[1]))
            .and_then(|node| node.snapshot.as_ref())
            .ok_or_else(|| std::io::Error::other("overlay source in graph"))?;
        assert_eq!(overlay.text, unsaved);
        let dependency_id = database
            .close_document(&dependency_uri)
            .ok_or_else(|| std::io::Error::other("closed dependency ID"))?;
        database.load_reachable(std::slice::from_ref(&dependency_id));
        let disk_text = fs::read_to_string(&paths[1])?;
        let cached = database
            .cached_document(&dependency_uri)
            .ok_or_else(|| std::io::Error::other("disk dependency snapshot"))?;
        assert_eq!(cached.text, disk_text);
        let source_graph = database
            .source_graph(root_id)
            .ok_or_else(|| std::io::Error::other("root graph after closing overlay"))?;
        let disk_source = source_graph
            .nodes
            .iter()
            .find(|node| node.path == normalize_path(&paths[1]))
            .and_then(|node| node.snapshot.as_ref())
            .ok_or_else(|| std::io::Error::other("disk source in graph"))?;
        assert_eq!(disk_source.text, disk_text);
        Ok(())
    }

    #[test]
    fn replacing_imports_removes_stale_reverse_edges_and_keeps_new_target() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let main_path = temp.path().join("main.bend");
        let old_path = temp.path().join("old.bend");
        let new_path = temp.path().join("new.bend");
        let old_source = "import ./old.bend as Dep\ndef main: U32\n  Dep.value\n";
        fs::write(&main_path, old_source)?;
        fs::write(
            &old_path,
            "import ./main.bend as Main\ndef value: U32\n  Main.main\n",
        )?;
        fs::write(&new_path, "def value: U32\n  2\n")?;

        let main_uri = file_uri(&main_path)?;
        let mut database = WorkspaceDb::default();
        let main_id = database.set_open_document(
            Document::new(
                main_uri.clone(),
                "bend".into(),
                Revision(1),
                old_source.into(),
            ),
            Some(main_path.clone()),
        );
        database.load_reachable(std::slice::from_ref(&main_id));
        let old_id = database
            .file_id_by_path(&old_path)
            .ok_or_else(|| std::io::Error::other("old dependency ID"))?;
        assert!(
            database
                .dependents(old_id)
                .iter()
                .any(|document| document.uri == main_uri)
        );

        let new_source = "import ./new.bend as Dep\ndef main: U32\n  Dep.value\n";
        let (main_id, imports_changed) = database
            .update_open_snapshot(
                &main_uri,
                Arc::new(DocumentSnapshot::new(Revision(2), new_source.into())),
            )
            .ok_or_else(|| std::io::Error::other("update open root"))?;
        assert!(imports_changed);
        database.load_reachable(std::slice::from_ref(&main_id));
        let new_id = database
            .file_id_by_path(&new_path)
            .ok_or_else(|| std::io::Error::other("new dependency ID"))?;
        assert!(database.dependents(old_id).is_empty());
        assert_eq!(
            database
                .dependents(new_id)
                .iter()
                .map(|document| document.uri.clone())
                .collect::<HashSet<_>>(),
            HashSet::from([main_uri.clone()])
        );
        let main = database
            .open_document(&main_uri)
            .ok_or_else(|| std::io::Error::other("open main document"))?;
        let import = analysis::imports(&main)
            .first()
            .ok_or_else(|| std::io::Error::other("main import"))?;
        let target = database
            .import_target(&main_uri, import.path)
            .ok_or_else(|| std::io::Error::other("resolved new import"))?;
        assert_eq!(target.uri, file_uri(&new_path)?);

        fs::remove_file(&old_path)?;
        let (old_id, imports_changed) = database
            .sync_disk_path(&old_path, None)
            .ok_or_else(|| std::io::Error::other("remove old dependency"))?;
        assert!(imports_changed);
        database.load_reachable(std::slice::from_ref(&old_id));
        assert!(database.cached_document(&file_uri(&old_path)?).is_none());
        assert!(database.dependents(old_id).is_empty());
        Ok(())
    }

    #[test]
    fn local_and_self_imported_calls_merge_and_follow_snapshot_reordering() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("main.bend");
        let uri = file_uri(&path)?;
        let source = "import ./main.bend as Self\n\ndef target(value):\n  value\n\ndef caller():\n  target(1)\n  Self.target(2)\n";
        let mut database = WorkspaceDb::default();
        database.set_open_document(
            Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
            Some(path),
        );
        let target = database
            .symbol_by_name(&uri, "target")
            .ok_or_else(|| std::io::Error::other("target function"))?;
        let caller = database
            .symbol_by_name(&uri, "caller")
            .ok_or_else(|| std::io::Error::other("caller function"))?;
        let incoming = database.incoming_calls(target.id);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].symbol.id, caller.id);
        assert_eq!(incoming[0].ranges.len(), 2);
        let outgoing = database.outgoing_calls(caller.id);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].symbol.id, target.id);
        assert_eq!(outgoing[0].ranges.len(), 2);
        assert_eq!(database.semantic_index_stats().calls, 2);

        let prefix = "def inserted():\n  0\n";
        let shifted_ranges: Vec<_> = incoming[0]
            .ranges
            .iter()
            .map(|range| {
                analysis::TextRange::new(range.start + prefix.len(), range.end + prefix.len())
            })
            .collect();
        database
            .update_open_snapshot(
                &uri,
                Arc::new(DocumentSnapshot::new(
                    Revision(2),
                    source.replacen("def target", &format!("{prefix}def target"), 1),
                )),
            )
            .ok_or_else(|| std::io::Error::other("replace snapshot"))?;
        assert!(database.incoming_calls(target.id).is_empty());
        let current_target = database
            .symbol_by_name(&uri, "target")
            .ok_or_else(|| std::io::Error::other("current target"))?;
        let current_caller = database
            .symbol_by_name(&uri, "caller")
            .ok_or_else(|| std::io::Error::other("current caller"))?;
        let incoming = database.incoming_calls(current_target.id);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].symbol.id, current_caller.id);
        assert_eq!(incoming[0].ranges, shifted_ranges);
        assert_eq!(database.semantic_index_stats().calls, 2);
        Ok(())
    }

    #[test]
    fn external_alias_groups_preserve_noncall_ranges_and_caller_boundaries() -> std::io::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let main_path = temp.path().join("main.bend");
        let dependency_path = temp.path().join("dep.bend");
        // The final unshadowed qualifier is B, immediately before a shadowed B.
        // Reusing its import binding must not reuse its lexical resolution.
        let source = "import ./dep.bend as A\nimport ./dep.bend as B\n\ndef first():\n  A.f(A.g(), B.f())\n  A.f\n\ndef second():\n  B.g(A.g(), B.f())\n\ndef shadowed(B):\n  B.f()\n";
        fs::write(&dependency_path, "def f():\n  0\n\ndef g():\n  1\n")?;
        let main_uri = file_uri(&main_path)?;
        let dependency_uri = file_uri(&dependency_path)?;
        let mut database = WorkspaceDb::default();
        let main_id = database.set_open_document(
            Document::new(main_uri.clone(), "bend".into(), Revision(1), source.into()),
            Some(main_path),
        );
        database.load_reachable(std::slice::from_ref(&main_id));
        let symbol = |uri: &Url, name: &str| {
            database
                .symbol_by_name(uri, name)
                .ok_or_else(|| std::io::Error::other(format!("missing {name}")))
        };
        let f = symbol(&dependency_uri, "f")?;
        let g = symbol(&dependency_uri, "g")?;
        let first = symbol(&main_uri, "first")?;
        let second = symbol(&main_uri, "second")?;
        let ranges = |member: &str, count: usize| {
            source
                .match_indices(member)
                .take(count)
                .map(|(offset, _)| analysis::TextRange::new(offset + 1, offset + 2))
                .collect::<Vec<_>>()
        };
        let f_ranges = ranges(".f", 4);
        let g_ranges = ranges(".g", 3);
        let references = database.references(f.id, false);
        assert!(
            references
                .iter()
                .all(|reference| reference.document.uri == main_uri)
        );
        assert_eq!(
            references
                .iter()
                .map(|reference| (reference.range, reference.kind))
                .collect::<Vec<_>>(),
            f_ranges
                .iter()
                .enumerate()
                .map(|(index, &range)| {
                    let kind = if index == 2 {
                        analysis::ReferenceKind::Read
                    } else {
                        analysis::ReferenceKind::Call
                    };
                    (range, kind)
                })
                .collect::<Vec<_>>()
        );
        let groups = |groups: Vec<crate::workspace::WorkspaceCallGroup>| {
            groups
                .into_iter()
                .map(|group| (group.symbol.id, group.ranges))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            groups(database.incoming_calls(f.id)),
            vec![
                (first.id, f_ranges[..2].to_vec()),
                (second.id, vec![f_ranges[3]])
            ]
        );
        assert_eq!(
            groups(database.incoming_calls(g.id)),
            vec![
                (first.id, vec![g_ranges[0]]),
                (second.id, g_ranges[1..].to_vec())
            ]
        );
        assert_eq!(
            groups(database.outgoing_calls(first.id)),
            vec![(f.id, f_ranges[..2].to_vec()), (g.id, vec![g_ranges[0]])]
        );
        assert_eq!(
            groups(database.outgoing_calls(second.id)),
            vec![(f.id, vec![f_ranges[3]]), (g.id, g_ranges[1..].to_vec())]
        );
        database
            .update_open_snapshot(
                &main_uri,
                Arc::new(DocumentSnapshot::new(
                    Revision(2),
                    "def local():\n  0\n".into(),
                )),
            )
            .ok_or_else(|| std::io::Error::other("replace importing snapshot"))?;
        assert!(database.references(f.id, false).is_empty());
        assert!(database.incoming_calls(f.id).is_empty());
        assert!(database.incoming_calls(g.id).is_empty());
        assert!(database.outgoing_calls(first.id).is_empty());
        Ok(())
    }

    fn local_query_ids(
        database: &WorkspaceDb,
        uri: &Url,
        ranges: [analysis::TextRange; 2],
    ) -> std::io::Result<(
        crate::workspace::GlobalSymbolId,
        crate::workspace::GlobalSymbolId,
        analysis::TokenId,
    )> {
        let target = database
            .symbol_by_name(uri, "target")
            .ok_or_else(|| std::io::Error::other("local target"))?;
        let caller = database
            .symbol_by_name(uri, "caller")
            .ok_or_else(|| std::io::Error::other("local caller"))?;
        let references = database.references(target.id, false);
        assert_eq!(
            references
                .iter()
                .map(|reference| reference.range)
                .collect::<Vec<_>>(),
            ranges
        );
        assert!(
            references
                .iter()
                .all(|reference| &reference.document.uri == uri)
        );
        let incoming = database.incoming_calls(target.id);
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].symbol.id, caller.id);
        assert_eq!(incoming[0].ranges, vec![ranges[0]]);
        let outgoing = database.outgoing_calls(caller.id);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].symbol.id, target.id);
        assert_eq!(outgoing[0].ranges, vec![ranges[0]]);
        let document = database
            .open_document(uri)
            .ok_or_else(|| std::io::Error::other("local document"))?;
        let token = document
            .syntax
            .token_at_or_before(ranges[0].start)
            .ok_or_else(|| std::io::Error::other("local call token"))?;
        assert_eq!(
            database.resolve_call(uri, token).map(|symbol| symbol.id),
            Some(target.id)
        );
        Ok((target.id, caller.id, token))
    }

    #[test]
    fn local_queries_follow_snapshot_and_language_lifecycle() -> std::io::Result<()> {
        let uri = Url::parse("file:///local-only.bend").map_err(std::io::Error::other)?;
        let source = "def target(value):\n  value\ndef caller():\n  target(1)\n  target\ndef shadowed(target):\n  target(2)\n";
        let call = source
            .find("target(1)")
            .ok_or_else(|| std::io::Error::other("call"))?;
        let read = source
            .find("  target\n")
            .ok_or_else(|| std::io::Error::other("read"))?
            + 2;
        let ranges = [
            analysis::TextRange::new(call, call + "target".len()),
            analysis::TextRange::new(read, read + "target".len()),
        ];
        let mut database = WorkspaceDb::default();
        database.set_open_document(
            Document::new(uri.clone(), "bend".into(), Revision(1), source.into()),
            None,
        );
        let (old_target, old_caller, _) = local_query_ids(&database, &uri, ranges)?;
        let prefix = "def inserted():\n  0\n";
        let replacement = format!("{prefix}{source}");
        database
            .update_open_snapshot(
                &uri,
                Arc::new(DocumentSnapshot::new(Revision(2), replacement.clone())),
            )
            .ok_or_else(|| std::io::Error::other("replace local-only snapshot"))?;
        assert!(database.symbol_by_id(old_target).is_none());
        assert!(database.outgoing_calls(old_caller).is_empty());
        let shifted = ranges.map(|range| {
            analysis::TextRange::new(range.start + prefix.len(), range.end + prefix.len())
        });
        let (current_target, _, token) = local_query_ids(&database, &uri, shifted)?;
        database.set_open_document(
            Document::new(
                uri.clone(),
                "plaintext".into(),
                Revision(3),
                replacement.clone(),
            ),
            None,
        );
        assert!(database.symbol_by_id(current_target).is_none());
        let unsupported = database
            .symbol_by_name(&uri, "target")
            .ok_or_else(|| std::io::Error::other("unsupported snapshot identity"))?;
        assert!(database.references(unsupported.id, true).is_empty());
        assert!(database.incoming_calls(unsupported.id).is_empty());
        assert!(database.resolve_call(&uri, token).is_none());
        let unsupported_caller = database
            .symbol_by_name(&uri, "caller")
            .ok_or_else(|| std::io::Error::other("unsupported caller identity"))?;
        assert!(database.outgoing_calls(unsupported_caller.id).is_empty());
        database.set_open_document(
            Document::new(uri.clone(), "bend2".into(), Revision(4), replacement),
            None,
        );
        assert!(database.symbol_by_id(unsupported.id).is_none());
        let (target, caller, _) = local_query_ids(&database, &uri, shifted)?;
        database
            .close_document(&uri)
            .ok_or_else(|| std::io::Error::other("close local-only source"))?;
        assert!(database.symbol_by_id(target).is_none());
        assert!(database.references(target, true).is_empty());
        assert!(database.incoming_calls(target).is_empty());
        assert!(database.outgoing_calls(caller).is_empty());
        Ok(())
    }

    #[test]
    fn external_group_lookup_rejects_same_name_from_retired_epoch() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let main_path = temp.path().join("main.bend");
        let dependency_path = temp.path().join("dep.bend");
        let dependency_source = "def f():\n  0\ndef own():\n  f()\n";
        fs::write(&dependency_path, dependency_source)?;
        let main_uri = file_uri(&main_path)?;
        let dependency_uri = file_uri(&dependency_path)?;
        let mut database = WorkspaceDb::default();
        let main_id = database.set_open_document(
            Document::new(
                main_uri.clone(),
                "bend".into(),
                Revision(1),
                "import ./dep.bend as D\ndef caller():\n  D.f()\n  D.f\n".into(),
            ),
            Some(main_path),
        );
        database.load_reachable(std::slice::from_ref(&main_id));
        let old = database
            .symbol_by_name(&dependency_uri, "f")
            .ok_or_else(|| std::io::Error::other("original dependency"))?;
        let groups = |database: &WorkspaceDb, target| {
            database
                .external_reference_groups(target)
                .map(|group| (group.source(), group.occurrence_count()))
                .collect::<Vec<_>>()
        };
        assert_eq!(groups(&database, old.id), vec![(main_id, 2)]);
        fs::write(
            &dependency_path,
            format!("# new snapshot\n{dependency_source}"),
        )?;
        let (changed, _) = database
            .sync_disk_path(&dependency_path, None)
            .ok_or_else(|| std::io::Error::other("updated dependency"))?;
        database.load_reachable(std::slice::from_ref(&changed));
        let current = database
            .symbol_by_name(&dependency_uri, "f")
            .ok_or_else(|| std::io::Error::other("current dependency"))?;
        assert_ne!(old.id, current.id);
        assert_eq!(old.id.local_symbol(), current.id.local_symbol());
        assert!(groups(&database, old.id).is_empty());
        assert_eq!(groups(&database, current.id), vec![(main_id, 2)]);
        database.set_open_document(
            Document::new(
                main_uri,
                "bend".into(),
                Revision(2),
                "def caller():\n  0\n".into(),
            ),
            None,
        );
        assert!(groups(&database, current.id).is_empty());
        Ok(())
    }
}
