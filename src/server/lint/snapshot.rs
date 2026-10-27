use crate::{
    analysis::{self, DocumentSnapshot, Revision},
    workspace::{Document, normalize_path, resolve_import_targets},
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
const ENTRY_LIMIT: usize = 32_768;
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
    aliases: BTreeMap<PathBuf, PathBuf>,
}

#[derive(Clone, Copy)]
struct CachedSources<'a> {
    documents: &'a BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    previously_open: &'a BTreeSet<PathBuf>,
}

// The analyzer compiles the rule program from disk: every file reachable from
// a rule file by a local import is executed code. Keep its bytes, the lexical
// spellings that reached it, and the targets that do not exist yet, so the
// same alias/creation fences as captured sources apply to rule inputs too.
#[derive(Default)]
struct RuleClosure {
    identities: Identities,
    aliases: BTreeMap<PathBuf, PathBuf>,
    missing: BTreeMap<PathBuf, Option<PathBuf>>,
    snapshots: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
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
    rule_inputs: Identities,
    rule_aliases: BTreeMap<PathBuf, PathBuf>,
    rule_missing: BTreeMap<PathBuf, Option<PathBuf>>,
    config_path: PathBuf,
    missing_paths: BTreeMap<PathBuf, Option<PathBuf>>,
    open_sources: BTreeSet<PathBuf>,
    source_aliases: BTreeMap<PathBuf, PathBuf>,
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
        self.identities
            .insert(path.clone(), hasher.finalize().into());
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
            if entries > ENTRY_LIMIT {
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

fn trusted_input(
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    rule_inputs: &Identities,
    path: &Path,
) -> bool {
    // Configuration and rule files are compiled from disk by the analyzer.
    // Helpers imported by a rule are part of that compiled program even when
    // they live outside `bend-lint-rules/` or outside the project, so they are
    // trusted saved inputs too and can never be replaced by an editor overlay.
    path == config_path || rule_paths.contains(path) || rule_inputs.contains_key(path)
}

// The analyzer builds its rule program from the import closure of the rule
// files with no project fence, and Bend executes absolute and bare local
// imports as readily as `./`/`../` ones. Classify imports the way the compiler
// does, skip only `Base` and BendHub packages (pinned external context), and
// capture the closure cold from disk with the existing snapshot budgets; warm
// queries only reuse it.
//
// This closure is the LSP's trusted read-set and tool-role classification, not
// an extension of the analyzer's own build-cache key. Upstream's `getClosure`
// key only follows `./`/`../` spellings, so a saved bare or absolute helper
// still restarts our preparation and invalidates retained results, but whether
// the producer rebuilds that spelling is its cache-key gap, tracked separately.
fn rule_closure(rule_paths: &HashSet<PathBuf>) -> io::Result<RuleClosure> {
    let mut closure = RuleClosure::default();
    let mut pending: Vec<PathBuf> = rule_paths
        .iter()
        .filter(|path| is_bend(path.as_path()))
        .cloned()
        .collect();
    let mut visited = HashSet::new();
    let mut bytes = 0;
    while let Some(path) = pending.pop() {
        let identity = input_identity(&path);
        let key = identity.clone().unwrap_or_else(|| path.clone());
        insert_alias(&mut closure.aliases, &path, &key)?;
        if !visited.insert(key) {
            continue;
        }
        if !path.exists() {
            if closure.missing.len() >= FILE_LIMIT {
                return Err(io::Error::other(
                    "bend-lint rule inputs exceed 4096 missing files",
                ));
            }
            closure.missing.insert(path, identity);
            continue;
        }
        if closure.identities.len() >= FILE_LIMIT {
            return Err(io::Error::other("bend-lint rule inputs exceed 4096 files"));
        }
        let (canonical, text) = read_source(&path, &mut bytes)?;
        let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
        let digest: [u8; 32] = Sha256::digest(snapshot.text.as_bytes()).into();
        closure.identities.insert(canonical.clone(), digest);
        closure
            .snapshots
            .insert(canonical.clone(), snapshot.clone());
        // Only Bend sources execute imports; every other closure file is a leaf
        // effect input whose bytes still key the prepared program.
        if is_bend(&canonical) {
            pending.extend(
                analysis::imports(&snapshot)
                    .iter()
                    .filter_map(|import| rule_target(&canonical, import.path_text(&snapshot.text))),
            );
        }
    }
    Ok(closure)
}

// Mirrors the workspace/compiler import classification: `Base` and BendHub
// package references (hash or `name@version` package) are pinned external
// context; every other spelling names a local file, including absolute and
// bare relative paths.
fn external_import(imported: &str) -> bool {
    if imported == "Base" {
        return true;
    }
    if imported.starts_with("./") || imported.starts_with("../") {
        return false;
    }
    let Some((package, _)) = imported.split_once('/') else {
        return false;
    };
    let hash = package.strip_prefix("0x").unwrap_or("");
    package.strip_prefix("0x").is_some_and(|hash| {
        hash.len() == 32
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) || !hash.is_empty()
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || package.contains('@')
}

fn rule_target(source: &Path, imported: &str) -> Option<PathBuf> {
    // Bend writes effect imports quoted; upstream strips the quotes before it
    // resolves the path, so the same spelling reaches the same file here.
    let imported = imported.trim_matches('"');
    if external_import(imported) {
        return None;
    }
    let path = Path::new(imported);
    if path.is_absolute() {
        return Some(normalize_path(path));
    }
    source
        .parent()
        .map(|parent| normalize_path(&parent.join(imported)))
}

fn merge_rule_metadata(
    captured: &mut Captured,
    aliases: &BTreeMap<PathBuf, PathBuf>,
    missing: &BTreeMap<PathBuf, Option<PathBuf>>,
) -> io::Result<()> {
    for (original, identity) in aliases {
        insert_alias(&mut captured.aliases, original, identity)?;
    }
    for (path, identity) in missing {
        if captured.missing.contains_key(path) {
            continue;
        }
        if captured.missing.len() >= FILE_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 4096 missing inputs",
            ));
        }
        captured.missing.insert(path.clone(), identity.clone());
    }
    Ok(())
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
    let mut reader =
        fs::File::open(path)?.take(u64::try_from(limit).map_err(io::Error::other)? + 1);
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
    let remaining = BYTE_LIMIT.checked_sub(*bytes).ok_or(io::Error::other(
        "bend-lint snapshot exceeds 32 MiB of source text",
    ))?;
    if size > remaining {
        return Err(io::Error::other(
            "bend-lint snapshot exceeds 32 MiB of source text",
        ));
    }
    let text = read_text(&canonical, remaining)?;
    *bytes += text.len();
    Ok((canonical, text))
}

fn insert_alias(
    aliases: &mut BTreeMap<PathBuf, PathBuf>,
    original: &Path,
    canonical: &Path,
) -> io::Result<()> {
    // Resolver paths and document URIs can spell the same lexical path
    // differently (`.`/`..` segments). Store both ends normalized so alias
    // lookup, coverage, and later input_identity validation agree.
    let original = normalize_path(original);
    let canonical = normalize_path(canonical);
    // Distinct lexical aliases can resolve to one canonical source, so the
    // source count budget does not bound this map. Every original path is
    // retained (ordinary, virtual, or symlinked), and the existing entry
    // budget bounds it with a clear failure instead of silent truncation.
    if !aliases.contains_key(&original) && aliases.len() >= ENTRY_LIMIT {
        return Err(io::Error::other(
            "bend-lint snapshot exceeds 32768 source aliases",
        ));
    }
    aliases.insert(original, canonical);
    Ok(())
}

fn open_snapshots(
    documents: &[Document],
) -> (
    BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    BTreeMap<PathBuf, PathBuf>,
) {
    let mut open = BTreeMap::new();
    let mut aliases = BTreeMap::new();
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
        open.insert(identity, document.snapshot.clone());
    }
    (open, aliases)
}

fn ensure_trusted_inputs_saved(
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    rule_inputs: &Identities,
    open: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
) -> io::Result<()> {
    // Configuration and rule files stay trusted saved inputs: an unsaved rule
    // buffer must not silently replace the rules the analyzer loads from disk.
    // Imported rule helpers are compiled from disk too, so they are gated here
    // rather than sent as overlays the rule program never reads.
    if let Some((path, _)) = open.iter().find(|(path, snapshot)| {
        trusted_input(config_path, rule_paths, rule_inputs, path)
            && !disk_matches(path, &snapshot.text)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "bend-lint skipped: save {} before checking this project; configuration, rule files, and imported rule helpers are trusted saved inputs",
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

fn in_project(root: &Path, canonical_root: Option<&Path>, files: &Identities, path: &Path) -> bool {
    path.starts_with(root)
        || canonical_root.is_some_and(|root| path.starts_with(root))
        || files.contains_key(path)
}

fn seeds(
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

fn build_sources(
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
        missing: BTreeMap::new(),
        aliases: BTreeMap::new(),
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
        insert_alias(&mut captured.aliases, &path, &key)?;
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

fn overlay_map(
    captured: &Captured,
    config_path: &Path,
    rule_paths: &HashSet<PathBuf>,
    rule_inputs: &Identities,
) -> io::Result<BTreeMap<PathBuf, Arc<DocumentSnapshot>>> {
    let mut overlays = BTreeMap::new();
    for (path, snapshot) in &captured.sources {
        if trusted_input(config_path, rule_paths, rule_inputs, path) {
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
        if path.to_str().is_some() || trusted_input(config_path, rule_paths, rule_inputs, path) {
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

// Findings for a closed import must name the same logical document the
// compiler and the editor use. Captured source keys are canonical, so a
// symlinked project root would otherwise publish `/private/var/...` while the
// client and compiler spell the same file `/var/...`. Prefer the project-root
// spelling, then any captured lexical alias (an external helper reached
// through a relative or symlinked import), and keep the canonical file URI as
// the lossless fallback.
fn logical_uri(
    path: &Path,
    root: &Path,
    canonical_root: Option<&Path>,
    aliases: &BTreeMap<PathBuf, PathBuf>,
) -> Option<Url> {
    // A native non-UTF-8 source key still has no representable URI, exactly as
    // before; logical spellings never lossily rewrite its native path.
    path.to_str()?;
    if let Some(relative) = canonical_root.and_then(|canonical| path.strip_prefix(canonical).ok())
        && let Some(uri) = file_url(&normalize_path(&root.join(relative)))
    {
        return Some(uri);
    }
    aliases
        .iter()
        .find(|(original, identity)| *identity == path && file_url(original).is_some())
        .and_then(|(original, _)| file_url(original))
        .or_else(|| file_url(path))
}

fn source_uris(
    documents: &[Document],
    sources: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    root: &Path,
    canonical_root: Option<&Path>,
    aliases: &BTreeMap<PathBuf, PathBuf>,
) -> BTreeMap<PathBuf, Url> {
    let mut uris = BTreeMap::new();
    for path in sources.keys() {
        if let Some(uri) = logical_uri(path, root, canonical_root, aliases) {
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
        let rule = rule_closure(&files.rule_paths)?;
        let (open, open_aliases) = open_snapshots(documents);
        ensure_trusted_inputs_saved(&config_path, &files.rule_paths, &rule.identities, &open)?;
        let mut pending = seeds(
            &root,
            canonical_root.as_deref(),
            &files.identities,
            &config_path,
            &files.rule_paths,
            &rule.identities,
            &open,
        );
        pending.extend(config_entries(&config, &root));
        // Every executed rule input is a checked source (never an overlay), so
        // findings about a rule or helper resolve instead of being rejected.
        pending.extend(rule.identities.keys().cloned());
        let already_open = BTreeSet::new();
        let mut captured = build_sources(
            &config_path,
            &files.rule_paths,
            &rule.identities,
            &open,
            pending,
            Some(CachedSources {
                documents: &rule.snapshots,
                previously_open: &already_open,
            }),
        )?;
        for (original, identity) in open_aliases {
            if captured.sources.contains_key(&identity) {
                insert_alias(&mut captured.aliases, &original, &identity)?;
            }
        }
        merge_rule_metadata(&mut captured, &rule.aliases, &rule.missing)?;
        let overlays = overlay_map(&captured, &config_path, &files.rule_paths, &rule.identities)?;
        let uris = source_uris(
            documents,
            &captured.sources,
            &root,
            canonical_root.as_deref(),
            &captured.aliases,
        );
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
            rule_inputs: rule.identities,
            rule_aliases: rule.aliases,
            rule_missing: rule.missing,
            config_path,
            missing_paths: captured.missing,
            open_sources,
            source_aliases: captured.aliases,
        })
    }

    // Rebuild the analysis view for new editor text while reusing captured disk
    // sources. Callers must have verified `unchanged()` first.
    pub(super) fn with_overlays(&self, documents: &[Document]) -> io::Result<Self> {
        let (open, open_aliases) = open_snapshots(documents);
        // `unchanged()` verified this closure is still the saved rule program,
        // so a warm rebuild reuses it instead of re-reading helper files.
        ensure_trusted_inputs_saved(
            &self.config_path,
            &self.rule_paths,
            &self.rule_inputs,
            &open,
        )?;
        let config_text = read_text(&self.config_path, BYTE_LIMIT)?;
        let config: serde_json::Value =
            serde_json::from_str(&config_text).map_err(io::Error::other)?;
        let mut pending = seeds(
            &self.root,
            self.canonical_root.as_deref(),
            &self.files,
            &self.config_path,
            &self.rule_paths,
            &self.rule_inputs,
            &open,
        );
        pending.extend(config_entries(&config, &self.root));
        pending.extend(self.rule_inputs.keys().cloned());
        let mut captured = build_sources(
            &self.config_path,
            &self.rule_paths,
            &self.rule_inputs,
            &open,
            pending,
            Some(CachedSources {
                documents: &self.sources,
                previously_open: &self.open_sources,
            }),
        )?;
        for (original, identity) in open_aliases {
            if captured.sources.contains_key(&identity) {
                insert_alias(&mut captured.aliases, &original, &identity)?;
            }
        }
        merge_rule_metadata(&mut captured, &self.rule_aliases, &self.rule_missing)?;
        let overlays = overlay_map(
            &captured,
            &self.config_path,
            &self.rule_paths,
            &self.rule_inputs,
        )?;
        let uris = source_uris(
            documents,
            &captured.sources,
            &self.root,
            self.canonical_root.as_deref(),
            &captured.aliases,
        );
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
            rule_inputs: self.rule_inputs.clone(),
            rule_aliases: self.rule_aliases.clone(),
            rule_missing: self.rule_missing.clone(),
            config_path: self.config_path.clone(),
            missing_paths: captured.missing,
            open_sources,
            source_aliases: captured.aliases,
        })
    }

    pub(super) fn refreshable(&self, file: &Path) -> bool {
        project_root(file).is_some_and(|root| root == self.root) && self.unchanged()
    }

    pub(super) fn entry_path(&self, path: &Path) -> io::Result<&Path> {
        self.sources
            .get_key_value(path)
            .or_else(|| {
                self.source_aliases
                    .get(path)
                    .and_then(|identity| self.sources.get_key_value(identity))
            })
            .map(|(identity, _)| identity.as_path())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Lint entry is absent from the captured project source view",
                )
            })
    }

    pub(super) fn config_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for (path, identity) in &self.files {
            // Rule inputs are hashed from the compiled closure below so a saved
            // helper outside the project tree changes the preparation key too.
            if !trusted_input(&self.config_path, &self.rule_paths, &self.rule_inputs, path)
                || self.rule_inputs.contains_key(path)
            {
                continue;
            }
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            hasher.update(identity);
        }
        // Preparation compiles the analyzer's rule program from disk; every
        // file in that closure is therefore a preparation input. For `./`/`../`
        // helpers this also matches the analyzer's build-cache key; bare and
        // absolute spellings are local executable inputs here even though the
        // producer's key omits them.
        for (path, identity) in &self.rule_inputs {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
            hasher.update(identity);
        }
        hasher.finalize().into()
    }

    pub(super) fn covers_path(&self, path: &Path) -> bool {
        // Original source mappings stay coverage even after their canonical
        // target changed: a retargeted external alias, or its newly resolved
        // target, must still refresh the retained project result.
        let normalized = normalize_path(path);
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
            || self.source_aliases.contains_key(path)
            || self.source_aliases.contains_key(&normalized)
            || self.rule_inputs.contains_key(path)
            || self.rule_inputs.contains_key(&normalized)
    }

    pub(super) fn unchanged(&self) -> bool {
        if self
            .missing_paths
            .iter()
            .any(|(path, identity)| path.exists() || input_identity(path) != *identity)
        {
            return false;
        }
        // Import and open-buffer paths, including symlinked parent
        // directories, must still resolve to the file the analysis captured.
        // A retarget is an input change even when the old target's bytes are
        // unchanged. Resolution goes through input_identity so virtual leaves
        // that do not exist yet are validated through their existing ancestors.
        if !self.source_aliases.iter().all(|(alias, expected)| {
            input_identity(alias).is_some_and(|actual| actual == *expected)
        }) {
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
        // The compiled rule closure is a preparation input even when a helper
        // is not captured as a linted source; a saved helper must restart
        // preparation rather than rebuild inside the next check's timeout.
        // In-project rule inputs reuse the tree walk digests already read.
        let Ok(tree) = project_files(&self.root) else {
            return false;
        };
        for (path, expected) in &self.rule_inputs {
            if let Some(identity) = tree.identities.get(path) {
                if identity != expected {
                    return false;
                }
                continue;
            }
            if self.sources.contains_key(path) && !self.open_sources.contains(path) {
                // The captured-source loop above already compared exact bytes.
                continue;
            }
            if !file_identity(path).is_ok_and(|identity| identity == *expected) {
                return false;
            }
        }
        // Overlaid sources are the immutable analysis view, so their saved disk
        // text may legitimately differ; every other input is compared exactly.
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
    let mut reader =
        fs::File::open(path)?.take(u64::try_from(BYTE_LIMIT).map_err(io::Error::other)? + 1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create project directory");
        }
        fs::write(path, text).expect("write project source");
    }

    fn document(path: &Path, text: &str) -> Document {
        Document::new(
            Url::from_file_path(path).expect("file URI"),
            "bend".into(),
            Revision(1),
            text.into(),
        )
    }

    struct Fixture {
        directory: tempfile::TempDir,
        project: PathBuf,
        root: PathBuf,
    }

    // A rule program is compiled from disk, so every file reachable from these
    // rule imports is executable trusted code rather than a linted source.
    fn fixture(rule: &str) -> Fixture {
        let directory = tempfile::tempdir().expect("rule project");
        let project = directory.path().join("project");
        write(&project.join("bend-lint.json"), "{\"rules\":{}}");
        write(&project.join("main.bend"), "def main: U32\n  0\n");
        write(&project.join(RULES_DIRECTORY).join("style.bend"), rule);
        let root = project.join("main.bend");
        Fixture {
            directory,
            project,
            root,
        }
    }

    #[test]
    fn an_unsaved_rule_helper_outside_the_rules_directory_is_rejected() {
        let fixture = fixture(
            "import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n",
        );
        let helper = fixture.project.join("shared/helper.bend");
        write(&helper, "def value: U32\n  1\n");
        let error =
            ProjectSnapshot::capture(&fixture.root, &[document(&helper, "def value: U32\n  2\n")])
                .err()
                .expect("an unsaved executed rule helper cannot be overlaid");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_saved_rule_helper_never_becomes_a_source_only_overlay() {
        let fixture = fixture(
            "import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n",
        );
        let helper = fixture.project.join("shared/helper.bend");
        write(&helper, "def value: U32\n  1\n");
        let unsaved_root = "def main: U32\n  1\n";
        let snapshot = ProjectSnapshot::capture(
            &fixture.root,
            &[
                document(&helper, "def value: U32\n  1\n"),
                document(&fixture.root, unsaved_root),
            ],
        )
        .expect("a saved rule helper and an ordinary unsaved source");
        let helper = input_identity(&helper).expect("helper identity");
        assert!(!snapshot.overlays.contains_key(&helper));
        assert_eq!(snapshot.sources[&helper].text, "def value: U32\n  1\n");
        let root = input_identity(&fixture.root).expect("root identity");
        assert_eq!(snapshot.overlays[&root].text, unsaved_root);
    }

    #[test]
    fn saved_rule_helper_changes_invalidate_preparation() {
        let fixture = fixture(
            "import ../shared/helper.bend as Local\nimport ../../external/helper.bend as External\n\ndef positive: U32\n  Local.value\n",
        );
        let local = fixture.project.join("shared/helper.bend");
        let external = fixture.directory.path().join("external/helper.bend");
        write(&local, "def value: U32\n  1\n");
        write(&external, "def external: U32\n  1\n");
        let first = ProjectSnapshot::capture(&fixture.root, &[]).expect("capture rule inputs");
        assert!(input_identity(&local).is_some() && input_identity(&external).is_some());
        write(
            &fixture.directory.path().join("external/helper.bend"),
            "def external: U32\n  2\n",
        );
        assert!(
            !first.unchanged(),
            "a saved external rule helper change must invalidate the captured project"
        );
        let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture rule inputs");
        assert_ne!(first.config_digest(), second.config_digest());
        write(
            &fixture.project.join("shared/helper.bend"),
            "def value: U32\n  2\n",
        );
        let third = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture rule inputs");
        assert_ne!(second.config_digest(), third.config_digest());
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_import_keeps_the_project_root_spelling() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("symlink project");
        let real = directory.path().join("real");
        let project = real.join("project");
        write(&project.join("bend-lint.json"), "{\"rules\":{}}");
        write(
            &project.join("main.bend"),
            "import dep.bend as Dep\ndef main: U32\n  Dep.value\n",
        );
        write(&project.join("dep.bend"), "def value: U32\n  1\n");
        let link = directory.path().join("link");
        symlink(&real, &link).expect("create symlinked root");
        let lexical = link.join("project");
        let snapshot = ProjectSnapshot::capture(&lexical.join("main.bend"), &[])
            .expect("capture through the symlinked root");
        let dependency = input_identity(&project.join("dep.bend")).expect("dependency identity");
        assert_eq!(
            snapshot.uris.get(&dependency),
            Some(&Url::from_file_path(lexical.join("dep.bend")).expect("logical file URI")),
            "closed imported findings must use the client's logical project spelling"
        );
    }

    #[test]
    fn an_absolute_local_rule_helper_is_a_fenced_executed_input() {
        let fixture = fixture("def positive: U32\n  0\n");
        let helper = fixture.directory.path().join("external/helper.bend");
        write(&helper, "def value: U32\n  1\n");
        write(
            &fixture.project.join(RULES_DIRECTORY).join("style.bend"),
            &format!(
                "import {} as Helper\n\ndef positive: U32\n  Helper.value\n",
                helper.display()
            ),
        );
        let snapshot =
            ProjectSnapshot::capture(&fixture.root, &[]).expect("absolute local rule import");
        assert!(
            snapshot
                .source(helper.to_str().expect("UTF-8 helper path"))
                .is_some(),
            "an executed helper must resolve as a checked source so its findings are accepted"
        );
        assert!(snapshot.covers_path(&input_identity(&helper).expect("helper identity")));
        let error =
            ProjectSnapshot::capture(&fixture.root, &[document(&helper, "def value: U32\n  2\n")])
                .err()
                .expect("an unsaved executed helper cannot be overlaid");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        write(&helper, "def value: U32\n  3\n");
        assert!(
            !snapshot.unchanged(),
            "a saved absolute helper is a captured and preparation input"
        );
        let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture helpers");
        assert_ne!(snapshot.config_digest(), second.config_digest());
    }

    #[test]
    fn a_bare_import_of_an_external_rule_helper_is_fenced() {
        let fixture =
            fixture("import ../../external/step.bend as Step\n\ndef positive: U32\n  Step.value\n");
        let external = fixture.directory.path().join("external");
        write(
            &external.join("step.bend"),
            "import leaf.bend as Leaf\n\ndef value: U32\n  Leaf.value\n",
        );
        let leaf = external.join("leaf.bend");
        write(&leaf, "def value: U32\n  1\n");
        let error =
            ProjectSnapshot::capture(&fixture.root, &[document(&leaf, "def value: U32\n  2\n")])
                .err()
                .expect("a bare-imported helper is compiled from disk, never overlaid");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let first = ProjectSnapshot::capture(&fixture.root, &[]).expect("capture bare closure");
        write(&leaf, "def value: U32\n  3\n");
        assert!(!first.unchanged(), "a saved bare helper is a rule input");
        let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture closure");
        assert_ne!(first.config_digest(), second.config_digest());
    }

    #[test]
    fn a_created_missing_absolute_rule_helper_invalidates_the_cached_project() {
        let fixture = fixture("def positive: U32\n  0\n");
        let missing = fixture.directory.path().join("external/later.bend");
        write(
            &fixture.project.join(RULES_DIRECTORY).join("style.bend"),
            &format!(
                "import {} as Later\n\ndef positive: U32\n  Later.value\n",
                missing.display()
            ),
        );
        let first =
            ProjectSnapshot::capture(&fixture.root, &[]).expect("missing absolute rule import");
        assert!(
            first.covers_path(&missing),
            "an absent executed helper keeps its capturing entry covered for later saves"
        );
        write(&missing, "def value: U32\n  1\n");
        assert!(
            !first.unchanged(),
            "creating a missing executed helper invalidates the cached closure"
        );
        let second = ProjectSnapshot::capture(&fixture.root, &[]).expect("recapture helpers");
        assert_ne!(first.config_digest(), second.config_digest());
    }

    #[cfg(unix)]
    #[test]
    fn an_external_rule_helper_alias_retarget_invalidates_the_project() {
        use std::os::unix::fs::symlink;

        let fixture = fixture(
            "import ../../external/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n",
        );
        let first = fixture.directory.path().join("first");
        let second = fixture.directory.path().join("second");
        write(&first.join("helper.bend"), "def value: U32\n  1\n");
        write(&second.join("helper.bend"), "def value: U32\n  1\n");
        let external = fixture.directory.path().join("external");
        symlink(&first, &external).expect("link the external helper directory");
        let snapshot = ProjectSnapshot::capture(&fixture.root, &[]).expect("symlinked helper");
        fs::remove_file(&external).expect("remove the external helper link");
        symlink(&second, &external).expect("retarget the external helper directory");
        assert!(
            !snapshot.unchanged(),
            "a retargeted helper alias is an input change even when the old bytes are unchanged"
        );
    }

    #[test]
    fn hub_and_base_rule_imports_are_not_local_inputs() {
        let fixture = fixture(
            "import Base\nimport rules-package@1.2.3.4/layout/format.bend as Format\n\ndef positive: U32\n  0\n",
        );
        let snapshot =
            ProjectSnapshot::capture(&fixture.root, &[]).expect("hub and Base rule imports");
        let phantom = fixture
            .project
            .join("bend-lint-rules/rules-package@1.2.3.4/layout/format.bend");
        assert!(
            snapshot
                .source(phantom.to_str().expect("UTF-8 phantom path"))
                .is_none(),
            "a Hub package body is pinned external context, never a local rule input"
        );
    }

    #[test]
    fn a_trusted_rule_entry_keeps_its_disk_source_and_stays_out_of_overlays() {
        let fixture = fixture("def positive: U32\n  0\n");
        let helper = fixture.project.join("shared/helper.bend");
        write(&helper, "def value: U32\n  1\n");
        let rule = fixture.project.join(RULES_DIRECTORY).join("style.bend");
        let rule_text =
            "import ../shared/helper.bend as Helper\n\ndef positive: U32\n  Helper.value\n";
        write(&rule, rule_text);
        let snapshot = ProjectSnapshot::capture(
            &rule,
            &[
                document(&rule, rule_text),
                document(&fixture.root, "def main: U32\n  1\n"),
            ],
        )
        .expect("capture a trusted rule file as the lint entry");
        let identity = input_identity(&rule).expect("rule identity");
        assert_eq!(snapshot.sources[&identity].text, rule_text);
        assert!(!snapshot.overlays.contains_key(&identity));
        assert!(
            snapshot
                .source(rule.to_str().expect("UTF-8 rule path"))
                .is_some(),
            "a finding about the checked rule file must resolve to its saved source"
        );
    }
}
