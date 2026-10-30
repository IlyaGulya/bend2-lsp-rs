use super::{
    FILE_LIMIT, Identities,
    disk::{is_bend, read_source},
    paths::{TrackedInputs, input_identity},
    rules::trusted_input,
};
use crate::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{Document, resolve_import_targets},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct Captured {
    pub(super) sources: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) open: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) inputs: TrackedInputs,
}

#[derive(Clone, Copy)]
pub(super) struct CachedSources<'a> {
    pub(super) documents: &'a BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) previously_open: &'a BTreeSet<PathBuf>,
}

type OpenSnapshots = (
    BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    BTreeMap<PathBuf, PathBuf>,
    BTreeMap<PathBuf, BTreeSet<String>>,
);

pub(super) fn open_snapshots(documents: &[Document]) -> OpenSnapshots {
    let mut open: BTreeMap<PathBuf, Arc<DocumentSnapshot>> = BTreeMap::new();
    let mut aliases: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut first_uri: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut conflicting: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for document in documents {
        let Ok(path) = document.uri.to_file_path() else {
            continue;
        };
        let Some(identity) = input_identity(&path) else {
            continue;
        };
        // Record the original path even when it already is its own canonical
        // target: an ordinary file can later become a symlink, and a virtual
        // leaf's parent directory can retarget without the leaf existing.
        // This transient map is bounded by the already-owned open documents;
        // only mappings kept for this project count against ENTRY_LIMIT.
        aliases.insert(path, identity.clone());
        if let Some(existing) = open.get_mut(&identity) {
            if existing.text != document.snapshot.text {
                let uris = conflicting.entry(identity.clone()).or_default();
                if let Some(first) = first_uri.get(&identity) {
                    uris.insert(first.clone());
                }
                uris.insert(document.uri.to_string());
            }
        } else {
            first_uri.insert(identity.clone(), document.uri.to_string());
            open.insert(identity, document.snapshot.clone());
        }
    }
    (open, aliases, conflicting)
}

// The analyzer must never check one arbitrary spelling when two open buffers
// of one physical source disagree. Only a conflict inside the captured source
// view is ambiguous; unrelated external buffers never disable this project.
pub(super) fn reject_conflicting_open_sources(
    conflicting: &BTreeMap<PathBuf, BTreeSet<String>>,
    sources: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> io::Result<()> {
    let Some((identity, uris)) = conflicting
        .iter()
        .find(|(identity, _)| sources.contains_key(*identity))
    else {
        return Ok(());
    };
    let mut spellings = String::new();
    for uri in uris {
        if !spellings.is_empty() {
            spellings.push_str(", ");
        }
        spellings.push_str(uri);
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "bend-lint skipped: open buffers for {} disagree; unify or close one buffer before checking ({spellings})",
            identity.display()
        ),
    ))
}

pub(super) fn config_entries(config: &serde_json::Value, root: &Path) -> Vec<PathBuf> {
    config
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|path| root.join(path))
        .collect()
}

fn in_project(root: &Path, canonical_root: Option<&Path>, files: &Identities, path: &Path) -> bool {
    path.starts_with(root)
        || canonical_root.is_some_and(|root| path.starts_with(root))
        || files.contains_key(path)
}

pub(super) fn seeds(
    root: &Path,
    canonical_root: Option<&Path>,
    files: &Identities,
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    rule_inputs: &Identities,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> Vec<PathBuf> {
    let mut pending: Vec<PathBuf> = files
        .keys()
        .filter(|path| is_bend(path.as_path()))
        .cloned()
        .collect();
    // Open buffers inside the project participate even when the file is new and
    // has never been saved, including paths below directories that do not exist.
    pending.extend(
        open.keys()
            .filter(|path| {
                let path = path.as_path();
                !trusted_input(config_path, rule_paths, rule_inputs, path)
                    && in_project(root, canonical_root, files, path)
            })
            .cloned(),
    );
    pending
}

fn imports(path: &Path, snapshot: &DocumentSnapshot) -> Vec<PathBuf> {
    resolve_import_targets(path, snapshot)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

pub(super) fn build_sources(
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    rule_inputs: &Identities,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    mut pending: Vec<PathBuf>,
    cache: Option<CachedSources<'_>>,
) -> io::Result<Captured> {
    let mut captured = Captured {
        sources: BTreeMap::new(),
        open: BTreeMap::new(),
        inputs: TrackedInputs::default(),
    };
    let mut visited = HashSet::new();
    let mut bytes = 0usize;
    while let Some(path) = pending.pop() {
        let identity = input_identity(&path);
        let key = match &identity {
            Some(identity) => identity.clone(),
            None => path.clone(),
        };
        // Retain every original path, including ordinary paths that already
        // are canonical and virtual leaves that do not exist yet, and record
        // before de-duplicating by canonical key so two different lexical
        // aliases of one source both remain fenced. Symlinked parent
        // directories are included. This also applies on the with_overlays
        // cache path, which resolves the same original path.
        captured.inputs.insert_alias(&path, &key)?;
        if !visited.insert(key.clone()) {
            continue;
        }
        if let Some(snapshot) = open
            .get(&key)
            .filter(|_| !trusted_input(config_path, rule_paths, rule_inputs, &key))
        {
            if captured.sources.len() >= FILE_LIMIT {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 4096 captured sources",
                ));
            }
            pending.extend(imports(&key, snapshot));
            captured.open.insert(key.clone(), snapshot.clone());
            captured.sources.insert(key, snapshot.clone());
            continue;
        }
        let cached = cache.and_then(|cache| {
            if cache.previously_open.contains(&key) {
                None
            } else {
                cache.documents.get(&key)
            }
        });
        if let Some(snapshot) = cached {
            if captured.sources.len() >= FILE_LIMIT {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 4096 captured sources",
                ));
            }
            pending.extend(imports(&key, snapshot));
            captured.sources.insert(key, snapshot.clone());
            continue;
        }
        if !path.exists() {
            captured.inputs.insert_missing(path, identity)?;
            continue;
        }
        let (canonical, text) = read_source(&path, &mut bytes)?;
        if captured.sources.contains_key(&canonical) {
            continue;
        }
        if let Some(snapshot) = open
            .get(&canonical)
            .filter(|_| !trusted_input(config_path, rule_paths, rule_inputs, &canonical))
        {
            if captured.sources.len() >= FILE_LIMIT {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 4096 captured sources",
                ));
            }
            pending.extend(imports(&canonical, snapshot));
            captured.open.insert(canonical.clone(), snapshot.clone());
            captured.sources.insert(canonical, snapshot.clone());
            continue;
        }
        if captured.sources.len() >= FILE_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 4096 captured sources",
            ));
        }
        let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
        pending.extend(imports(&canonical, &snapshot));
        captured.sources.insert(canonical, snapshot);
    }
    Ok(captured)
}
