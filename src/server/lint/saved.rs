use crate::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{Document, SourceGraph, resolve_import_targets},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use url::Url;

const FILE_LIMIT: usize = 4096;
const BYTE_LIMIT: usize = 32 * 1024 * 1024;

type Identities = BTreeMap<PathBuf, [u8; 32]>;

#[derive(Default)]
struct DiskFiles {
    contents: BTreeMap<PathBuf, String>,
    identities: Identities,
    bytes: usize,
    keep_contents: bool,
}

pub(super) struct SavedProject {
    pub(super) root: PathBuf,
    pub(super) entry: PathBuf,
    pub(super) sources: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) uris: BTreeMap<PathBuf, Url>,
    files: Identities,
    tree_paths: HashSet<PathBuf>,
}

impl DiskFiles {
    fn read(&mut self, path: &Path) -> io::Result<PathBuf> {
        let path = fs::canonicalize(path)?;
        if self.identities.contains_key(&path) {
            return Ok(path);
        }
        if self.identities.len() >= FILE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 4096 files"));
        }
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(io::Error::other("bend-lint requires regular saved source files"));
        }
        let size = usize::try_from(metadata.len()).map_err(io::Error::other)?;
        if self.bytes.saturating_add(size) > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        let mut reader = fs::File::open(&path)?.take(u64::try_from(BYTE_LIMIT - self.bytes + 1).map_err(io::Error::other)?);
        let digest = if self.keep_contents {
            let mut text = String::new();
            reader.read_to_string(&mut text)?;
            self.bytes += text.len();
            let digest = Sha256::digest(text.as_bytes()).into();
            self.contents.insert(path.clone(), text);
            digest
        } else {
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
            hasher.finalize().into()
        };
        if self.bytes > BYTE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 32 MiB"));
        }
        self.identities.insert(path.clone(), digest);
        Ok(path)
    }
}

fn project_files(root: &Path, keep_contents: bool) -> io::Result<DiskFiles> {
    let mut files = DiskFiles { keep_contents, ..DiskFiles::default() };
    let mut pending = vec![(root.to_path_buf(), false)];
    let mut visited = HashSet::new();
    let mut entries = 0;
    while let Some((directory, under_rules)) = pending.pop() {
        let directory = fs::canonicalize(directory)?;
        if !visited.insert((directory.clone(), under_rules)) {
            continue;
        }
        if visited.len() > FILE_LIMIT {
            return Err(io::Error::other("bend-lint snapshot exceeds 4096 directories"));
        }
        for entry in fs::read_dir(&directory)? {
            entries += 1;
            if entries > 32_768 {
                return Err(io::Error::other("bend-lint snapshot exceeds 32768 directory entries"));
            }
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if matches!(name.to_str(), Some("node_modules" | ".git" | "target" | ".cache")) {
                continue;
            }
            let metadata = fs::metadata(&path)?;
            if metadata.is_dir() {
                if pending.len() + visited.len() >= FILE_LIMIT {
                    return Err(io::Error::other("bend-lint snapshot exceeds 4096 directories"));
                }
                pending.push((path, under_rules || name == "bend-lint-rules"));
            } else if path.extension().is_some_and(|extension| extension == "bend")
                || name == "bend-lint.json"
                || under_rules
            {
                files.read(&path)?;
            }
        }
    }
    Ok(files)
}

fn source_paths(config: &serde_json::Value, root: &Path, graph: &SourceGraph, files: &DiskFiles) -> Vec<PathBuf> {
    let mut paths: Vec<_> = files.contents.keys()
        .filter(|path| path.extension().is_some_and(|extension| extension == "bend"))
        .cloned().collect();
    paths.extend(graph.nodes.iter().map(|node| node.path.clone()));
    if let Some(entries) = config.get("entries").and_then(serde_json::Value::as_array) {
        paths.extend(entries.iter().filter_map(serde_json::Value::as_str).map(|path| root.join(path)));
    }
    paths
}

fn source_uris(
    graph: &SourceGraph,
    documents: &[Document],
    sources: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> BTreeMap<PathBuf, Url> {
    let mut uris = BTreeMap::new();
    for path in sources.keys() {
        if let Some(uri) = file_url(path) {
            uris.insert(path.clone(), uri);
        }
    }
    for node in &graph.nodes {
        if let (Ok(path), Ok(uri)) = (fs::canonicalize(&node.path), Url::from_file_path(&node.path)) {
            uris.insert(path, uri);
        }
    }
    for document in documents {
        if let Some(path) = document.uri.to_file_path().ok().and_then(|path| fs::canonicalize(path).ok()) {
            uris.insert(path, document.uri.clone());
        }
    }
    uris
}

impl SavedProject {
    pub(super) fn capture(file: &Path, graph: &SourceGraph, documents: &[Document]) -> io::Result<Self> {
        let entry = file.to_path_buf();
        let root = entry.parent()
            .and_then(|parent| parent.ancestors().find(|path| path.join("bend-lint.json").exists()))
            .ok_or_else(|| io::Error::other("bend-lint requires a nearest ancestor bend-lint.json"))?
            .to_path_buf();
        let mut files = project_files(&root, true)?;
        let tree_paths = files.identities.keys().cloned().collect();
        let config_text = files.contents.get(&fs::canonicalize(root.join("bend-lint.json"))?)
            .ok_or_else(|| io::Error::other("missing bend-lint.json snapshot"))?;
        let config: serde_json::Value = serde_json::from_str(config_text).map_err(io::Error::other)?;
        let mut pending = source_paths(&config, &root, graph, &files);
        let mut sources = BTreeMap::new();
        while let Some(path) = pending.pop() {
            // Missing imports are the compiler's concern. Their creation
            // invalidates the job through watched-file notifications.
            if !path.exists() {
                continue;
            }
            let path = files.read(&path)?;
            if sources.contains_key(&path) {
                continue;
            }
            let text = files.contents.remove(&path)
                .ok_or_else(|| io::Error::other("missing saved source"))?;
            let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
            pending.extend(resolve_import_targets(&path, &snapshot).into_iter().map(|(_, path)| path));
            sources.insert(path, snapshot);
        }
        let uris = source_uris(graph, documents, &sources);
        let saved = Self {
            root, entry, sources,
            files: files.identities,
            tree_paths,
            uris,
        };
        if !Self::documents_saved(documents) {
            return Err(io::Error::other("bend-lint skipped: an open buffer differs from disk"));
        }
        Ok(saved)
    }

    pub(super) fn documents_saved(documents: &[Document]) -> bool {
        // Program rules may inspect entries beyond the current import graph.
        // Conservatively require every open file buffer to agree with disk.
        documents.iter().filter(|document| document.uri.scheme() == "file").all(|document| {
            document.uri.to_file_path().ok().is_some_and(|path| disk_matches(&path, &document.text))
        })
    }

    pub(super) fn unchanged(&self) -> bool {
        let Ok(mut tree) = project_files(&self.root, false) else { return false; };
        if tree.identities.len() != self.tree_paths.len()
            || tree.identities.keys().any(|path| !self.tree_paths.contains(path))
        {
            return false;
        }
        self.files.keys().filter(|path| !self.tree_paths.contains(*path))
            .all(|path| tree.read(path).is_ok()) && tree.identities == self.files
    }

    pub(super) fn source(&self, path: &str) -> Option<(PathBuf, &Arc<DocumentSnapshot>)> {
        let path = Path::new(path);
        let path = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        let path = fs::canonicalize(path).ok()?;
        self.sources.get(&path).map(|snapshot| (path, snapshot))
    }
}

fn disk_matches(path: &Path, text: &str) -> bool {
    let Ok(metadata) = fs::metadata(path) else { return false; };
    if !metadata.is_file() || usize::try_from(metadata.len()).ok() != Some(text.len()) {
        return false;
    }
    let Ok(mut file) = fs::File::open(path) else { return false; };
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
