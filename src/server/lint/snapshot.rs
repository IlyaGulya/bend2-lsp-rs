use crate::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{Document, resolve_import_targets},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use url::Url;

const FILE_LIMIT: usize = 4096;
const BYTE_LIMIT: usize = 32 * 1024 * 1024;
const CONFIG_NAME: &str = "bend-lint.json";
const RULES_DIRECTORY: &str = "bend-lint-rules";

type Identities = BTreeMap<PathBuf, [u8; 32]>;

#[derive(Default)]
struct DiskFiles {
    identities: Identities,
    aliases: BTreeMap<PathBuf, PathBuf>,
    rule_paths: HashSet<PathBuf>,
    bytes: usize,
}

struct Captured {
    sources: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    open: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    missing: BTreeMap<PathBuf, Option<PathBuf>>,
}

// Immutable analysis view for one project. `sources` is exactly what the
// analyzer sees: open buffers and new named files replace the saved text, and
// every other captured source is sent as an overlay too so the analyzer cannot
// observe a mix of disk generations.
pub(super) struct ProjectSnapshot {
    pub(super) root: PathBuf,
    canonical_root: Option<PathBuf>,
    pub(super) sources: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) overlays: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) uris: BTreeMap<PathBuf, Url>,
    files: Identities,
    aliases: BTreeMap<PathBuf, PathBuf>,
    rule_paths: HashSet<PathBuf>,
    config_path: PathBuf,
    missing_paths: BTreeMap<PathBuf, Option<PathBuf>>,
    open_sources: BTreeSet<PathBuf>,
}

impl DiskFiles {
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        let canonical = fs::canonicalize(path)?;
        if let Some(previous) = self.aliases.get(path) {
            if previous != &canonical {
                return Err(io::Error::other(
                    "bend-lint source alias changed during capture",
                ));
            }
        } else if canonical != path {
            self.aliases.insert(path.to_path_buf(), canonical.clone());
        }
        Ok(canonical)
    }

    fn read(&mut self, path: &Path) -> io::Result<PathBuf> {
        let path = self.canonicalize(path)?;
        if self.identities.contains_key(&path) {
            return Ok(path);
        }
        if self.identities.len() >= FILE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 4096 files"));
        }
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(io::Error::other(
                "bend-lint requires regular saved source files",
            ));
        }
        let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
        if self.bytes.saturating_add(size) > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        let mut reader = fs::File::open(&path)?
            .take(u64::try_from(BYTE_LIMIT - self.bytes + 1).map_err(io::Error::other)?);
        let mut hasher = Sha256::new();
        let mut buffer = [0; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            self.bytes += count;
            hasher.update(&buffer[..count]);
        }
        if self.bytes > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        self.identities.insert(path.clone(), hasher.finalize().into());
        Ok(path)
    }
}

fn project_files(root: &Path) -> io::Result<DiskFiles> {
    let mut files = DiskFiles::default();
    let mut pending = vec![(root.to_path_buf(), false)];
    let mut visited = HashSet::new();
    let mut entries = 0;
    while let Some((directory, under_rules)) = pending.pop() {
        let directory = files.canonicalize(&directory)?;
        if !visited.insert((directory.clone(), under_rules)) {
            continue;
        }
        if visited.len() > FILE_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 4096 directories",
            ));
        }
        for entry in fs::read_dir(&directory)? {
            entries += 1;
            if entries > 32_768 {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 32768 directory entries",
                ));
            }
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some("node_modules" | ".git" | "target" | ".cache")
            ) {
                continue;
            }
            let metadata = fs::metadata(&path)?;
            if metadata.is_dir() {
                if pending.len() + visited.len() >= FILE_LIMIT {
                    return Err(io::Error::other(
                        "bend-lint snapshot exceeds 4096 directories",
                    ));
                }
                pending.push((path, under_rules || name == RULES_DIRECTORY));
            } else if path
                .extension()
                .is_some_and(|extension| extension == "bend")
                || name == CONFIG_NAME
                || under_rules
            {
                let canonical = files.read(&path)?;
                if under_rules {
                    files.rule_paths.insert(canonical);
                }
            }
        }
    }
    Ok(files)
}

fn is_bend(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "bend")
}

fn trusted_input(config_path: &Path, rule_paths: &HashSet<PathBuf>, path: &Path) -> bool {
    path == config_path || rule_paths.contains(path)
}

fn read_text(path: &Path, limit: usize) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    if size > limit {
        return Err(io::Error::other(
            "bend-lint source text exceeds the 32 MiB snapshot limit",
        ));
    }
    let mut reader = fs::File::open(path)?
        .take(u64::try_from(limit).map_err(io::Error::other)? + 1);
    let mut text = String::with_capacity(size);
    reader.read_to_string(&mut text)?;
    if text.len() != size {
        return Err(io::Error::other(
            "bend-lint source changed while it was captured",
        ));
    }
    Ok(text)
}

fn read_source(path: &Path, bytes: &mut usize) -> io::Result<(PathBuf, String)> {
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    let remaining = BYTE_LIMIT
        .checked_sub(*bytes)
        .ok_or(io::Error::other("bend-lint snapshot exceeds 32 MiB of source text"))?;
    if size > remaining {
        return Err(io::Error::other(
            "bend-lint snapshot exceeds 32 MiB of source text",
        ));
    }
    let text = read_text(&canonical, remaining)?;
    *bytes += text.len();
    Ok((canonical, text))
}

fn open_snapshots(documents: &[Document]) -> BTreeMap<PathBuf, Arc<DocumentSnapshot>> {
    let mut open = BTreeMap::new();
    for document in documents {
        let Ok(path) = document.uri.to_file_path() else {
            continue;
        };
        let Some(identity) = input_identity(&path) else {
            continue;
        };
        open.insert(identity, document.snapshot.clone());
    }
    open
}

fn ensure_trusted_inputs_saved(
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> io::Result<()> {
    // Configuration and rule files stay trusted saved inputs: an unsaved rule
    // buffer must not silently replace the rules the analyzer loads from disk.
    if let Some((path, _)) = open
        .iter()
        .find(|(path, snapshot)| {
            trusted_input(config_path, rule_paths, path) && !disk_matches(path, &snapshot.text)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "bend-lint skipped: save {} before checking this project; configuration and rule files are trusted saved inputs",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn config_entries(config: &serde_json::Value, root: &Path) -> Vec<PathBuf> {
    config
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|path| root.join(path))
        .collect()
}

fn in_project(
    root: &Path,
    canonical_root: Option<&Path>,
    files: &Identities,
    path: &Path,
) -> bool {
    path.starts_with(root)
        || canonical_root.is_some_and(|root| path.starts_with(root))
        || files.contains_key(path)
}

fn seeds(
    root: &Path,
    canonical_root: Option<&Path>,
    config: &serde_json::Value,
    files: &Identities,
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> Vec<PathBuf> {
    let mut pending: Vec<PathBuf> = files
        .keys()
        .filter(|path| is_bend(path.as_path()))
        .cloned()
        .collect();
    pending.extend(config_entries(config, root));
    // Open buffers inside the project participate even when the file is new and
    // has never been saved, including paths below directories that do not exist.
    pending.extend(
        open.keys()
            .filter(|path| {
                let path = path.as_path();
                !trusted_input(config_path, rule_paths, path)
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

fn build_sources(
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    mut pending: Vec<PathBuf>,
    cache: Option<(
        &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
        &BTreeSet<PathBuf>,
    )>,
) -> io::Result<Captured> {
    let mut captured = Captured {
        sources: BTreeMap::new(),
        open: BTreeMap::new(),
        missing: BTreeMap::new(),
    };
    let mut visited = HashSet::new();
    let mut bytes = 0usize;
    while let Some(path) = pending.pop() {
        let identity = input_identity(&path);
        let key = match &identity {
            Some(identity) => identity.clone(),
            None => path.clone(),
        };
        if !visited.insert(key.clone()) {
            continue;
        }
        if let Some(snapshot) = open
            .get(&key)
            .filter(|_| !trusted_input(config_path, rule_paths, &key))
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
        let cached = cache.and_then(|(sources, previous_open)| {
            if previous_open.contains(&key) {
                None
            } else {
                sources.get(&key)
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
            if captured.missing.len() >= FILE_LIMIT {
                return Err(io::Error::other(
                    "bend-lint snapshot exceeds 4096 missing inputs",
                ));
            }
            captured.missing.insert(path, identity);
            continue;
        }
        let (canonical, text) = read_source(&path, &mut bytes)?;
        if captured.sources.contains_key(&canonical) {
            continue;
        }
        if let Some(snapshot) = open
            .get(&canonical)
            .filter(|_| !trusted_input(config_path, rule_paths, &canonical))
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

fn overlay_map(
    captured: &Captured,
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
) -> io::Result<BTreeMap<PathBuf, Arc<DocumentSnapshot>>> {
    let mut overlays = BTreeMap::new();
    for (path, snapshot) in &captured.sources {
        if trusted_input(config_path, rule_paths, path) {
            continue;
        }
        // Oversized JSON keys and lossy native path strings are never used. A
        // non-UTF-8 source that is clean on disk may be read by the analyzer
        // instead; a dirty one cannot be represented and is never silently
        // replaced by its saved text.
        if path.to_str().is_some() {
            overlays.insert(path.clone(), snapshot.clone());
        }
    }
    for (path, snapshot) in &captured.open {
        if path.to_str().is_some() || trusted_input(config_path, rule_paths, path) {
            continue;
        }
        if !disk_matches(path, &snapshot.text) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "bend-lint skipped: save {} before checking this project; its non-UTF-8 path cannot be sent as an unsaved overlay",
                    path.display()
                ),
            ));
        }
    }
    Ok(overlays)
}

fn source_uris(
    documents: &[Document],
    sources: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> BTreeMap<PathBuf, Url> {
    let mut uris = BTreeMap::new();
    for path in sources.keys() {
        if let Some(uri) = file_url(path) {
            uris.insert(path.clone(), uri);
        }
    }
    for document in documents {
        let Ok(path) = document.uri.to_file_path() else {
            continue;
        };
        let Some(identity) = input_identity(&path) else {
            continue;
        };
        if sources.contains_key(&identity) {
            uris.insert(identity, document.uri.clone());
        }
    }
    uris
}

pub(super) fn project_root(file: &Path) -> Option<PathBuf> {
    file.parent().and_then(|parent| {
        parent
            .ancestors()
            .find(|path| path.join(CONFIG_NAME).exists())
            .map(Path::to_path_buf)
    })
}

impl ProjectSnapshot {
    pub(super) fn capture(file: &Path, documents: &[Document]) -> io::Result<Self> {
        let root = project_root(file).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "No bend-lint.json found for {}; create and review a project configuration before checking",
                    file.display()
                ),
            )
        })?;
        let files = project_files(&root)?;
        let canonical_root = files.aliases.get(&root).cloned();
        let config_path = fs::canonicalize(root.join(CONFIG_NAME))?;
        let config_text = read_text(&config_path, BYTE_LIMIT)?;
        let config: serde_json::Value =
            serde_json::from_str(&config_text).map_err(io::Error::other)?;
        let open = open_snapshots(documents);
        ensure_trusted_inputs_saved(&config_path, &files.rule_paths, &open)?;
        let pending = seeds(
            &root,
            canonical_root.as_deref(),
            &config,
            &files.identities,
            &config_path,
            &files.rule_paths,
            &open,
        );
        let captured = build_sources(
            &config_path,
            &files.rule_paths,
            &open,
            pending,
            None,
        )?;
        let overlays = overlay_map(&captured, &config_path, &files.rule_paths)?;
        let uris = source_uris(documents, &captured.sources);
        let open_sources: BTreeSet<PathBuf> = captured.open.keys().cloned().collect();
        Ok(Self {
            root,
            canonical_root,
            sources: captured.sources,
            overlays,
            uris,
            files: files.identities,
            aliases: files.aliases,
            rule_paths: files.rule_paths,
            config_path,
            missing_paths: captured.missing,
            open_sources,
        })
    }

    // Rebuild the analysis view for new editor text while reusing captured disk
    // sources. Callers must have verified `unchanged()` first.
    pub(super) fn with_overlays(&self, documents: &[Document]) -> io::Result<Self> {
        let open = open_snapshots(documents);
        ensure_trusted_inputs_saved(&self.config_path, &self.rule_paths, &open)?;
        let config_text = read_text(&self.config_path, BYTE_LIMIT)?;
        let config: serde_json::Value =
            serde_json::from_str(&config_text).map_err(io::Error::other)?;
        let pending = seeds(
            &self.root,
            self.canonical_root.as_deref(),
            &config,
            &self.files,
            &self.config_path,
            &self.rule_paths,
            &open,
        );
        let captured = build_sources(
            &self.config_path,
            &self.rule_paths,
            &open,
            pending,
            Some((&self.sources, &self.open_sources)),
        )?;
        let overlays = overlay_map(&captured, &self.config_path, &self.rule_paths)?;
        let uris = source_uris(documents, &captured.sources);
        let open_sources: BTreeSet<PathBuf> = captured.open.keys().cloned().collect();
        Ok(Self {
            root: self.root.clone(),
            canonical_root: self.canonical_root.clone(),
            sources: captured.sources,
            overlays,
            uris,
            files: self.files.clone(),
            aliases: self.aliases.clone(),
            rule_paths: self.rule_paths.clone(),
            config_path: self.config_path.clone(),
            missing_paths: captured.missing,
            open_sources,
        })
    }

    pub(super) fn refreshable(&self, file: &Path) -> bool {
        project_root(file).is_some_and(|root| root == self.root) && self.unchanged()
    }

    pub(super) fn config_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for (path, identity) in &self.files {
            if !trusted_input(&self.config_path, &self.rule_paths, path) {
                continue;
            }
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            hasher.update(identity);
        }
        hasher.finalize().into()
    }

    pub(super) fn covers_path(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
            || self
                .canonical_root
                .as_ref()
                .is_some_and(|root| path.starts_with(root))
            || self.files.contains_key(path)
            || self.sources.contains_key(path)
            || self.missing_paths.contains_key(path)
            || self
                .missing_paths
                .values()
                .any(|identity| identity.as_deref() == Some(path))
            || self.aliases.keys().any(|alias| path.starts_with(alias))
    }

    pub(super) fn unchanged(&self) -> bool {
        if self
            .missing_paths
            .iter()
            .any(|(path, identity)| path.exists() || input_identity(path) != *identity)
        {
            return false;
        }
        for (path, snapshot) in &self.sources {
            if self.files.contains_key(path) || self.open_sources.contains(path) {
                continue;
            }
            // Captured disk sources outside the project tree still have to
            // describe the same bytes; captured overlays are fenced by open
            // document revisions instead.
            let digest: [u8; 32] = Sha256::digest(snapshot.text.as_bytes()).into();
            if !file_identity(path).is_ok_and(|identity| identity == digest) {
                return false;
            }
        }
        // Overlaid sources are the immutable analysis view, so their saved disk
        // text may legitimately differ; every other input is compared exactly.
        let Ok(tree) = project_files(&self.root) else {
            return false;
        };
        // Equal canonical bytes do not prove that a source/configuration alias
        // still names the same file, including aliases in parent directories.
        tree.identities == self.files
            && tree.rule_paths == self.rule_paths
            && tree.aliases == self.aliases
    }

    // A retained result may be offered as a fix only while every covered open
    // buffer still matches the immutable analysis view it was computed from.
    pub(super) fn open_documents_current(&self, documents: &[Document]) -> bool {
        documents.iter().all(|document| {
            let Ok(path) = document.uri.to_file_path() else {
                return true;
            };
            let identity = input_identity(&path);
            let covered = self.covers_path(&path)
                || identity
                    .as_ref()
                    .is_some_and(|identity| self.covers_path(identity));
            if !covered {
                return true;
            }
            let Some(identity) = identity else {
                return false;
            };
            self.sources.get(&identity).map_or_else(
                || disk_matches(&path, &document.text),
                |source| source.text == document.text,
            )
        })
    }

    pub(super) fn source(&self, path: &str) -> Option<(PathBuf, &Arc<DocumentSnapshot>)> {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if let Some(snapshot) = self.sources.get(&path) {
            return Some((path, snapshot));
        }
        let identity = input_identity(&path)?;
        let snapshot = self.sources.get(&identity)?;
        Some((identity, snapshot))
    }
}

// Resolve existing ancestors too: virtual or missing imported files still have
// an identity through a symlinked parent, even before the leaf exists on disk.
pub(super) fn input_identity(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Some(canonical);
    }
    path.parent()?.ancestors().find_map(|ancestor| {
        let mut canonical = fs::canonicalize(ancestor).ok()?;
        canonical.push(path.strip_prefix(ancestor).ok()?);
        Some(canonical)
    })
}

fn file_identity(path: &Path) -> io::Result<[u8; 32]> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other(
            "bend-lint requires regular saved source files",
        ));
    }
    let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
    if size > BYTE_LIMIT {
        return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
    }
    let mut reader = fs::File::open(path)?
        .take(u64::try_from(BYTE_LIMIT).map_err(io::Error::other)? + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

fn disk_matches(path: &Path, text: &str) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || usize::try_from(metadata.len()).ok() != Some(text.len()) {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0; 8192];
    for chunk in text.as_bytes().chunks(buffer.len()) {
        if file.read_exact(&mut buffer[..chunk.len()]).is_err() || &buffer[..chunk.len()] != chunk {
            return false;
        }
    }
    file.read(&mut buffer[..1]).is_ok_and(|count| count == 0)
}

fn file_url(path: &Path) -> Option<Url> {
    #[cfg(windows)]
    {
        // Canonical Windows paths use a verbatim prefix, which must not become
        // a literal '?' file-URI authority. Open-buffer URIs still take priority.
        let text = path.to_str()?;
        let normal = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
        };
        Url::from_file_path(normal).ok()
    }
    #[cfg(not(windows))]
    Url::from_file_path(path).ok()
}
