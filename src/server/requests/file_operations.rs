use super::super::{adapters, lsp::Backend, orchestration::run_staging};
use crate::{
    analysis::{DocumentSnapshot, Revision, TextRange},
    workspace::{
        Document, PathRename, WorkspaceDb, import_rename_edits, normalize_path, renamed_path,
        resolve_import_targets,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{
        DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, RenameFilesParams,
        TextDocumentEdit, TextEdit, WorkspaceEdit,
    },
};
use url::Url;

fn path_renames(params: RenameFilesParams) -> Result<Vec<PathRename>> {
    let mut renames = Vec::with_capacity(params.files.len());
    for file in params.files {
        let old = Url::parse(&file.old_uri)
            .ok()
            .and_then(|uri| uri.to_file_path().ok())
            .ok_or_else(|| Error::invalid_params("File rename requires local file URIs"))?;
        let new = Url::parse(&file.new_uri)
            .ok()
            .and_then(|uri| uri.to_file_path().ok())
            .ok_or_else(|| Error::invalid_params("File rename requires local file URIs"))?;
        let old = normalize_path(&old);
        let new = normalize_path(&new);
        if renames
            .iter()
            .any(|rename: &PathRename| rename.old == old || rename.new == new)
        {
            return Err(Error::invalid_params(
                "File rename batch has duplicate sources or destinations",
            ));
        }
        renames.push(PathRename { old, new });
    }
    Ok(renames)
}

fn physical_path(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut links = HashSet::new();
    loop {
        let mut replacement = None;
        for ancestor in current.ancestors() {
            let Ok(suffix) = current.strip_prefix(ancestor) else {
                return current;
            };
            if let Ok(canonical) = std::fs::canonicalize(ancestor) {
                return normalize_path(&canonical.join(suffix));
            }
            if let Ok(target) = std::fs::read_link(ancestor) {
                if !links.insert(ancestor.to_path_buf()) {
                    return path.to_path_buf();
                }
                replacement = Some(normalize_path(
                    &ancestor
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .join(target)
                        .join(suffix),
                ));
                break;
            }
        }
        let Some(next) = replacement else {
            return current;
        };
        current = next;
    }
}

fn physical_destination(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => physical_path(parent).join(name),
        _ => physical_path(path),
    }
}

fn join_suffix(path: &Path, suffix: &Path) -> PathBuf {
    if suffix.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        path.join(suffix)
    }
}

struct PhysicalRename {
    old: PathBuf,
    new: PathBuf,
    requested: usize,
    aliases: bool,
    depth: usize,
}

fn physical_renames(requested: &[PathRename]) -> Result<Vec<PhysicalRename>> {
    let mut sources = HashSet::new();
    let mut destinations = HashSet::new();
    requested
        .iter()
        .enumerate()
        .map(|(index, rename)| {
            let symlink = std::fs::symlink_metadata(&rename.old)
                .or_else(|_| std::fs::symlink_metadata(&rename.new))
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            let old = if symlink {
                physical_destination(&rename.old)
            } else {
                physical_path(&rename.old)
            };
            let new = physical_destination(&rename.new);
            if !sources.insert(old.clone()) || !destinations.insert(new.clone()) {
                return Err(Error::invalid_params(
                    "File rename batch aliases the same filesystem source or destination",
                ));
            }
            let depth = old.components().count();
            Ok(PhysicalRename {
                old,
                new,
                requested: index,
                aliases: !symlink,
                depth,
            })
        })
        .collect()
}

fn physical_match<'a>(
    path: &'a Path,
    renames: &'a [PhysicalRename],
) -> Option<(&'a PhysicalRename, &'a Path)> {
    renames
        .iter()
        .filter(|rename| rename.aliases)
        .filter_map(|rename| {
            path.strip_prefix(&rename.old)
                .ok()
                .map(|suffix| (rename, suffix))
        })
        .max_by_key(|(rename, _)| rename.depth)
}

fn expand_renames(
    requested: &[PathRename],
    paths: Vec<(PathBuf, bool)>,
) -> Result<Vec<PathRename>> {
    let physical = physical_renames(requested)?;
    let indexed: Vec<_> = paths
        .into_iter()
        .map(|(path, owned)| {
            let resolved = physical_path(&path);
            (path, resolved, owned)
        })
        .collect();
    let mut expanded = requested.to_vec();
    let mut destinations: HashMap<PathBuf, (PathBuf, PathBuf)> = HashMap::new();
    for (path, resolved, owned) in &indexed {
        if *owned
            && physical
                .iter()
                .any(|rename| resolved.starts_with(&rename.new))
        {
            return Err(Error::invalid_params(
                "File rename would overwrite compiler-owned source",
            ));
        }
        let Some((rename, suffix)) = physical_match(resolved, &physical) else {
            continue;
        };
        let new = join_suffix(&requested[rename.requested].new, suffix);
        let destination = join_suffix(&rename.new, suffix);
        if let Some((source, _)) = destinations.get(&destination)
            && source != resolved
        {
            return Err(Error::invalid_params(
                "File rename batch merges distinct filesystem sources",
            ));
        }
        destinations.insert(destination, (resolved.clone(), new.clone()));
        if renamed_path(path, requested) == *path && *path != new {
            expanded.push(PathRename {
                old: path.clone(),
                new,
            });
        }
    }
    for (path, resolved, _) in indexed {
        if renamed_path(&path, requested) != path || physical_match(&resolved, &physical).is_some()
        {
            continue;
        }
        if let Some((_, new)) = destinations.get(&resolved)
            && path != *new
        {
            expanded.push(PathRename {
                old: path,
                new: new.clone(),
            });
        }
    }
    Ok(expanded)
}

struct WillSource {
    document: Document,
    path: PathBuf,
    open: bool,
    imports: Vec<(TextRange, PathBuf)>,
}

fn will_sources(database: &WorkspaceDb) -> Vec<WillSource> {
    database
        .loaded_documents()
        .into_iter()
        .filter_map(|document| {
            if database.is_compiler_document(&document.uri) {
                return None;
            }
            let path = database.document_path(&document.uri)?;
            let open = database.is_document_open(&document.uri);
            Some(WillSource {
                document,
                path,
                open,
                imports: Vec::new(),
            })
        })
        .collect()
}

fn prepare_will_sources(sources: Vec<WillSource>) -> Result<Vec<WillSource>> {
    let mut prepared = Vec::with_capacity(sources.len());
    for mut source in sources {
        if !source.open {
            match std::fs::read_to_string(&source.path) {
                Ok(text) => {
                    source.document.snapshot =
                        Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::invalid_params(format!(
                        "Cannot read indexed import source {}: {error}",
                        source.path.display()
                    )));
                }
            }
        }
        source.imports = resolve_import_targets(&source.path, &source.document);
        prepared.push(source);
    }
    Ok(prepared)
}

struct RenameSource {
    uri: Url,
    old_uri: Url,
    path: PathBuf,
    snapshot: Option<Arc<DocumentSnapshot>>,
    open: bool,
}

struct PreparedSource {
    source: RenameSource,
    imports: Vec<(TextRange, PathBuf)>,
    disk_snapshot: Option<Arc<DocumentSnapshot>>,
    disk_imports: Vec<(TextRange, PathBuf)>,
}

fn rename_sources(database: &WorkspaceDb, renames: &[PathRename]) -> Vec<RenameSource> {
    let affected = database.rename_import_edits(renames).unwrap_or_default();
    database
        .loaded_documents()
        .into_iter()
        .filter_map(|document| {
            if database.is_compiler_document(&document.uri) {
                return None;
            }
            let path = database.document_path(&document.uri)?;
            let new_path = renamed_path(&path, renames);
            let open = database.is_document_open(&document.uri);
            if !open
                && new_path == path
                && !affected
                    .iter()
                    .any(|(affected, _)| affected.uri == document.uri)
            {
                return None;
            }
            Some(RenameSource {
                uri: Url::from_file_path(&new_path).ok()?,
                old_uri: document.uri,
                path: new_path,
                snapshot: Some(document.snapshot),
                open,
            })
        })
        .collect()
}

fn prepare_sources(mut sources: Vec<RenameSource>) -> Vec<PreparedSource> {
    // Aliases of one moved file share a single disk snapshot. An open overlay
    // wins only when compatibility has established equal open revisions/text.
    sources.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| right.open.cmp(&left.open))
    });
    sources.dedup_by(|left, right| left.path == right.path);
    sources
        .into_iter()
        .map(|mut source| {
            if !source.open {
                source.snapshot = None;
            }
            let disk_snapshot = std::fs::read_to_string(&source.path)
                .ok()
                .map(|text| Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text)));
            let disk_imports = disk_snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                resolve_import_targets(&source.path, snapshot)
            });
            let imports = source.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                resolve_import_targets(&source.path, snapshot)
            });
            PreparedSource {
                source,
                imports,
                disk_snapshot,
                disk_imports,
            }
        })
        .collect()
}

impl Backend {
    pub(in crate::server) async fn handle_will_rename_files(
        &self,
        params: RenameFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let requested = path_renames(params)?;
        loop {
            let read = self.workspace_ready_read().await;
            let generation = self.workspace.generation();
            let (sources, paths) = {
                let database = self.workspace.read();
                (
                    will_sources(&database),
                    database
                        .indexed_paths()
                        .map(|(path, owned)| (path.to_path_buf(), owned))
                        .collect(),
                )
            };
            drop(read);
            let requested = requested.clone();
            let Some(prepared) = run_staging(self.workspace.staging.clone(), move || {
                Ok::<_, Error>((
                    expand_renames(&requested, paths)?,
                    prepare_will_sources(sources)?,
                ))
            })
            .await
            else {
                return Err(Error::internal_error());
            };
            let (renames, sources) = prepared?;
            let _read = self.workspace_ready_read().await;
            if generation != self.workspace.generation() {
                continue;
            }
            let database = self.workspace.read();
            if !database.rename_paths_compatible(&renames) {
                return Err(Error::invalid_params(
                    "File rename conflicts with indexed buffers or compiler-owned source",
                ));
            }
            let mut changes = Vec::new();
            for source in sources {
                let edits =
                    import_rename_edits(&source.path, &source.document, &renames, |range| {
                        source
                            .imports
                            .iter()
                            .find(|(path, _)| *path == range)
                            .map(|(_, target)| target.as_path())
                    })
                    .ok_or_else(|| {
                        Error::invalid_params("Moved file cannot be represented by a Bend import")
                    })?;
                if !edits.is_empty() {
                    changes.push((source.document, source.open, edits));
                }
            }
            if changes.is_empty() {
                return Ok(None);
            }
            changes.sort_unstable_by(|(left, _, _), (right, _, _)| left.uri.cmp(&right.uri));
            let edits = changes
                .into_iter()
                .map(|(document, open, edits)| TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: document.uri.clone(),
                        version: open.then_some(document.revision.0),
                    },
                    edits: edits
                        .into_iter()
                        .map(|(range, new_text)| {
                            OneOf::Left(TextEdit {
                                range: adapters::range(&document, range),
                                new_text,
                            })
                        })
                        .collect(),
                })
                .collect();
            return Ok(Some(WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(edits)),
                ..Default::default()
            }));
        }
    }

    pub(in crate::server) async fn handle_did_rename_files(&self, params: RenameFilesParams) {
        let requested = match path_renames(params) {
            Ok(renames) => renames,
            Err(error) => {
                tracing::warn!(%error, "Ignoring invalid file rename notification");
                return;
            }
        };
        let Some((documents, retired_uris)) = self.commit_file_renames(&requested).await else {
            return;
        };
        for uri in retired_uris {
            if self.document(&uri).is_some() {
                continue;
            }
            self.diagnostics
                .cancel_if(&uri, || self.document(&uri).is_none())
                .await;
            self.remove_import_diagnostics(&uri).await;
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
        let roots: Vec<_> = {
            let database = self.workspace.read();
            documents
                .iter()
                .filter_map(|document| database.file_id_by_uri(&document.uri))
                .collect()
        };
        self.load_reachable_async(&roots).await;
        for document in documents {
            let version = document.revision.0;
            self.schedule_diagnostics(document.uri, version).await;
        }
    }

    async fn commit_file_renames(
        &self,
        requested: &[PathRename],
    ) -> Option<(Vec<Document>, Vec<Url>)> {
        loop {
            let read = self.file_rename_ready_read().await;
            let generation = self.workspace.generation();
            let paths = self
                .workspace
                .read()
                .indexed_paths()
                .map(|(path, owned)| (path.to_path_buf(), owned))
                .collect();
            drop(read);
            let requested = requested.to_vec();
            let renames = match run_staging(self.workspace.staging.clone(), move || {
                expand_renames(&requested, paths)
            })
            .await?
            {
                Ok(renames) => renames,
                Err(error) => {
                    tracing::warn!(%error, "Ignoring conflicting filesystem rename aliases");
                    return None;
                }
            };
            let read = self.file_rename_ready_read().await;
            if generation != self.workspace.generation() {
                continue;
            }
            let sources = {
                let database = self.workspace.read();
                if !database.rename_paths_compatible(&renames) {
                    tracing::warn!(
                        "Ignoring file rename conflicting with indexed buffers or compiler-owned source"
                    );
                    return None;
                }
                rename_sources(&database, &renames)
            };
            let retired_uris = sources
                .iter()
                .filter(|source| source.old_uri != source.uri)
                .map(|source| source.old_uri.clone())
                .collect();
            drop(read);
            let prepared = run_staging(self.workspace.staging.clone(), move || {
                prepare_sources(sources)
            })
            .await?;
            let _update = self.workspace.updates.write().await;
            if let Some(documents) = self.workspace.commit_file_rename(generation, |database| {
                let merged = database.rename_paths(&renames);
                for PreparedSource {
                    source,
                    imports,
                    disk_snapshot,
                    disk_imports,
                } in prepared
                {
                    database.sync_disk_snapshot_prepared(&source.path, disk_snapshot, disk_imports);
                    if source.open
                        && let Some(snapshot) = source.snapshot
                    {
                        database.update_open_snapshot_prepared(&source.uri, snapshot, imports);
                    }
                }
                Some((merged, (database.open_documents(), retired_uris)))
            }) {
                return Some(documents);
            }
        }
    }
}
