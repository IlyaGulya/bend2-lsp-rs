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
}
