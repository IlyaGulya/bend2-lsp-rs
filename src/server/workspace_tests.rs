mod tests {
    use std::{
        collections::HashSet,
        fmt::Write as _,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::analysis::{self, DocumentSnapshot, Revision};
    use crate::workspace::{Document, PathRename, WorkspaceDb, normalize_path};
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
        let old_payload = Arc::downgrade(
            &database
                .cached_document(&file_uri(&old_path)?)
                .ok_or_else(|| std::io::Error::other("old dependency payload"))?
                .snapshot,
        );
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
        assert!(
            old_payload.upgrade().is_none(),
            "replacing an import must release the orphaned cyclic dependency"
        );
        assert!(database.cached_document(&file_uri(&old_path)?).is_none());
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
        let (removed_id, _) = database
            .sync_disk_path(&old_path, None)
            .ok_or_else(|| std::io::Error::other("remove old dependency"))?;
        assert_eq!(removed_id, old_id);
        database.load_reachable(std::slice::from_ref(&old_id));
        assert!(database.cached_document(&file_uri(&old_path)?).is_none());
        assert!(database.dependents(old_id).is_empty());
        Ok(())
    }

    #[test]
    fn renamed_identity_keeps_imports_of_an_already_loaded_destination() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let old_directory = temp.path().join("old");
        let new_directory = temp.path().join("new");
        fs::create_dir_all(&old_directory)?;
        fs::create_dir_all(&new_directory)?;
        let old_dependency = old_directory.join("dep.bend");
        let old_consumer = old_directory.join("consumer.bend");
        let new_dependency = new_directory.join("renamed.bend");
        let new_consumer = new_directory.join("consumer.bend");
        fs::write(&old_dependency, "def value = 1\n")?;
        fs::write(
            &old_consumer,
            "import ./dep.bend as Dep\ndef exported = Dep.value\n",
        )?;
        let root_path = temp.path().join("main.bend");
        let root_uri = file_uri(&root_path)?;
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(
            Document::new(
                root_uri.clone(),
                "bend".into(),
                Revision(1),
                "import ./old/dep.bend as Dep\nimport ./old/consumer.bend as Consumer\ndef main = Dep.value\n".into(),
            ),
            Some(root_path),
        );
        database.load_reachable(&[root]);
        let original_consumer = database
            .file_id_by_path(&old_consumer)
            .ok_or_else(|| std::io::Error::other("original consumer identity"))?;
        fs::rename(&old_dependency, &new_dependency)?;
        fs::rename(&old_consumer, &new_consumer)?;
        fs::write(
            &new_consumer,
            "import ./renamed.bend as Dep\ndef exported = Dep.value\n",
        )?;
        database.update_open_snapshot(
            &root_uri,
            Arc::new(DocumentSnapshot::new(
                Revision(2),
                "import ./new/renamed.bend as Dep\nimport ./new/consumer.bend as Consumer\ndef main = Dep.value\n".into(),
            )),
        )
        .ok_or_else(|| std::io::Error::other("update root imports"))?;
        database.load_reachable(&[root]);
        database.rename_paths(&[
            PathRename {
                old: old_dependency,
                new: new_dependency.clone(),
            },
            PathRename {
                old: old_consumer,
                new: new_consumer.clone(),
            },
        ]);
        let consumer_uri = file_uri(&new_consumer)?;
        assert_eq!(
            database.file_id_by_uri(&consumer_uri),
            Some(original_consumer)
        );
        let consumer = database
            .cached_document(&consumer_uri)
            .ok_or_else(|| std::io::Error::other("renamed consumer"))?;
        let import = analysis::imports(&consumer)
            .first()
            .ok_or_else(|| std::io::Error::other("consumer import"))?;
        let target = database
            .import_target(&consumer_uri, import.path)
            .ok_or_else(|| std::io::Error::other("renamed consumer import target"))?;
        assert_eq!(target.uri, file_uri(&new_dependency)?);
        Ok(())
    }

    #[test]
    fn disk_sync_respects_demand_and_invalidates_retired_generations() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root_path = temp.path().join("main.bend");
        let dep_path = temp.path().join("dep.bend");
        let unknown_path = temp.path().join("unknown.bend");
        let dep_uri = file_uri(&dep_path)?;
        let root_uri = file_uri(&root_path)?;
        let disk_source = "def value: U32\n  1\n";
        let importing_source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
        fs::write(&dep_path, disk_source)?;
        let mut database = WorkspaceDb::default();
        let (dep_id, changed) = database
            .sync_disk_path(&dep_path, Some(disk_source.into()))
            .ok_or_else(|| std::io::Error::other("intern unneeded disk identity"))?;
        assert!(!changed);
        assert!(database.cached_document(&dep_uri).is_none());
        assert_eq!(database.file_id_by_uri(&dep_uri), Some(dep_id));
        let initial_generation = database.disk_generation(dep_id);
        let late_snapshot = Arc::new(DocumentSnapshot::new(
            Revision::UNVERSIONED,
            "def value: U32\n  99\n".into(),
        ));
        assert!(
            database
                .sync_disk_snapshot_prepared(&unknown_path, Some(late_snapshot.clone()), Vec::new())
                .is_none()
        );
        assert!(database.file_id_by_path(&unknown_path).is_none());
        assert!(
            database
                .sync_disk_snapshot_prepared(&dep_path, Some(late_snapshot.clone()), Vec::new())
                .is_none()
        );
        assert_eq!(database.disk_generation(dep_id), initial_generation);

        let root = database.set_open_document(
            Document::new(
                root_uri.clone(),
                "bend".into(),
                Revision(1),
                importing_source.into(),
            ),
            Some(root_path.clone()),
        );
        assert!(database.is_needed(dep_id));
        database.load_reachable(&[root]);
        let loaded_generation = database.disk_generation(dep_id);
        assert_ne!(loaded_generation, initial_generation);
        database
            .update_open_snapshot(
                &root_uri,
                Arc::new(DocumentSnapshot::new(
                    Revision(2),
                    "def main: U32\n  0\n".into(),
                )),
            )
            .ok_or_else(|| std::io::Error::other("remove dependency demand"))?;
        assert!(!database.is_needed(dep_id));
        assert_ne!(database.disk_generation(dep_id), loaded_generation);
        assert!(
            database
                .sync_disk_snapshot_prepared(&dep_path, Some(late_snapshot), Vec::new())
                .is_none()
        );
        database.load_reachable(&[root]);
        assert!(database.cached_document(&dep_uri).is_none());
        assert_eq!(database.file_id_by_path(&dep_path), Some(dep_id));
        Ok(())
    }

    #[test]
    fn compiler_navigation_source_survives_editor_overlay_close_and_retirement()
    -> std::io::Result<()> {
        let directory = tempfile::tempdir()?;
        let generated_path = directory.path().join("Base.bend");
        let dependency_path = directory.path().join("dep.bend");
        let canonical_path = directory.path().join("canonical.bend");
        let user_path = directory.path().join("user.bend");
        let generated_uri = file_uri(&generated_path)?;
        let generated_source =
            "import ./canonical.bend as Canonical\ndef generated: U32\n  Canonical.value\n";
        fs::write(&generated_path, generated_source)?;
        fs::write(&dependency_path, "def value: U32\n  2\n")?;
        fs::write(&canonical_path, "def value: U32\n  3\n")?;
        let snapshot = Arc::new(DocumentSnapshot::new(
            Revision::UNVERSIONED,
            generated_source.into(),
        ));
        let generated_payload = Arc::downgrade(&snapshot);
        let mut database = WorkspaceDb::default();
        database.register_compiler_document(
            generated_uri.clone(),
            generated_path.clone(),
            snapshot,
            directory,
        );
        let id = database.set_open_document(
            Document::new(
                generated_uri.clone(),
                "bend".into(),
                Revision(1),
                "import ./dep.bend as Dep\ndef generated: U32\n  Dep.value\n".into(),
            ),
            Some(generated_path.clone()),
        );
        database.load_reachable(&[id]);
        let dependency_payload = Arc::downgrade(
            &database
                .cached_document(&file_uri(&dependency_path)?)
                .ok_or_else(|| std::io::Error::other("overlay dependency"))?
                .snapshot,
        );
        database
            .close_document(&generated_uri)
            .ok_or_else(|| std::io::Error::other("close generated source overlay"))?;
        database.load_reachable(&[id]);

        let retained = database
            .cached_document(&generated_uri)
            .ok_or_else(|| std::io::Error::other("retained compiler navigation source"))?;
        assert_eq!(retained.text, generated_source);
        assert!(generated_payload.upgrade().is_some());
        assert!(dependency_payload.upgrade().is_none());
        assert_eq!(fs::read_to_string(&generated_path)?, generated_source);
        assert!(database.is_compiler_document(&generated_uri));
        assert!(database.source_graph(id).is_none());
        assert!(database.indexed_documents().is_empty());
        drop(retained);
        let canonical_source = "def value: U32\n  4\n";
        fs::write(&canonical_path, canonical_source)?;
        let user = database.set_open_document(
            Document::new(
                file_uri(&user_path)?,
                "bend".into(),
                Revision(1),
                "import ./Base.bend as Base\ndef main: U32\n  Base.generated\n".into(),
            ),
            Some(user_path),
        );
        database.load_reachable(&[user]);
        let retained = database
            .cached_document(&generated_uri)
            .ok_or_else(|| std::io::Error::other("reactivated generated source"))?;
        let canonical_import = analysis::imports(&retained)
            .first()
            .ok_or_else(|| std::io::Error::other("generated source canonical import"))?;
        let canonical = database
            .import_target(&generated_uri, canonical_import.path)
            .ok_or_else(|| std::io::Error::other("reactivated generated import navigation"))?;
        assert_eq!(canonical.text, canonical_source);
        assert_eq!(canonical.uri, file_uri(&canonical_path)?);
        assert!(
            database
                .cached_document(&file_uri(&dependency_path)?)
                .is_none()
        );
        drop(canonical);
        drop(retained);
        drop(database);
        assert!(generated_payload.upgrade().is_none());
        assert!(!generated_path.exists());
        Ok(())
    }
}
