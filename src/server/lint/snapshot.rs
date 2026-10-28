use crate::{
    analysis::DocumentSnapshot,
    workspace::{Document, normalize_path},
};
#[path = "snapshot/disk.rs"]
mod disk;
#[path = "snapshot/overlay.rs"]
mod overlay;
#[path = "snapshot/paths.rs"]
pub(in crate::server::lint) mod paths;
#[path = "snapshot/rules.rs"]
mod rules;
#[path = "snapshot/sources.rs"]
mod sources;
#[cfg(test)]
#[path = "snapshot/tests.rs"]
mod tests;

use disk::{disk_matches, file_identity, project_files, read_text};
use overlay::overlay_map;
use paths::{input_identity, missing_input_covered, project_root, source_uris, utf8_spelling};
use rules::{ensure_trusted_inputs_saved, rule_closure, trusted_input};
use sha2::{Digest, Sha256};
use sources::{
    CachedSources, build_sources, config_entries, open_snapshots, reject_conflicting_open_sources,
    seeds,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs, io,
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

// Immutable analysis view for one project. `sources` is exactly what the
// analyzer sees: open buffers and new named files replace the saved text, and
// every other captured source is sent as an overlay too so the analyzer cannot
// observe a mix of disk generations.
pub(super) struct ProjectSnapshot {
    pub(super) root: PathBuf,
    canonical_root: Option<PathBuf>,
    pub(super) sources: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) overlays: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    pub(super) uris: BTreeMap<PathBuf, Vec<Url>>,
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
        let (open, open_aliases, conflicting) = open_snapshots(documents);
        ensure_trusted_inputs_saved(
            &config_path,
            &files.rule_paths,
            &rule.inputs.identities,
            &open,
        )?;
        let mut pending = seeds(
            &root,
            canonical_root.as_deref(),
            &files.identities,
            &config_path,
            &files.rule_paths,
            &rule.inputs.identities,
            &open,
        );
        pending.extend(config_entries(&config, &root));
        // Every executed rule input is a checked source (never an overlay), so
        // findings about a rule or helper resolve instead of being rejected.
        pending.extend(rule.inputs.identities.keys().cloned());
        let already_open = BTreeSet::new();
        let mut captured = build_sources(
            &config_path,
            &files.rule_paths,
            &rule.inputs.identities,
            &open,
            pending,
            Some(CachedSources {
                documents: &rule.snapshots,
                previously_open: &already_open,
            }),
        )?;
        reject_conflicting_open_sources(&conflicting, &captured.sources)?;
        for (original, identity) in open_aliases {
            if captured.sources.contains_key(&identity) {
                captured.inputs.insert_alias(&original, &identity)?;
            }
        }
        captured
            .inputs
            .merge_parts(&rule.inputs.aliases, &rule.inputs.missing)?;
        let overlays = overlay_map(
            &captured,
            &config_path,
            &files.rule_paths,
            &rule.inputs.identities,
        )?;
        let uris = source_uris(
            documents,
            &captured.sources,
            &root,
            canonical_root.as_deref(),
            &captured.inputs.aliases,
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
            rule_inputs: rule.inputs.identities,
            rule_aliases: rule.inputs.aliases,
            rule_missing: rule.inputs.missing,
            config_path,
            missing_paths: captured.inputs.missing,
            open_sources,
            source_aliases: captured.inputs.aliases,
        })
    }

    // Rebuild the analysis view for new editor text while reusing captured disk
    // sources. Callers must have verified `unchanged()` first.
    pub(super) fn with_overlays(&self, documents: &[Document]) -> io::Result<Self> {
        let (open, open_aliases, conflicting) = open_snapshots(documents);
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
        reject_conflicting_open_sources(&conflicting, &captured.sources)?;
        for (original, identity) in open_aliases {
            if captured.sources.contains_key(&identity) {
                captured.inputs.insert_alias(&original, &identity)?;
            }
        }
        captured
            .inputs
            .merge_parts(&self.rule_aliases, &self.rule_missing)?;
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
            &captured.inputs.aliases,
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
            missing_paths: captured.inputs.missing,
            open_sources,
            source_aliases: captured.inputs.aliases,
        })
    }

    pub(super) fn refreshable(&self, file: &Path) -> bool {
        // Capture keeps its lexical root for the analyzer's working directory
        // and logical URIs, but the editor may reach the same physical project
        // through another directory spelling (for example a directory symlink).
        // Freshness compares physical identity so a second entry spelling still
        // reuses the frozen view, while a retargeted directory alias resolves to
        // a different identity and stays an input change.
        project_root(file).is_some_and(|root| {
            root == self.root
                || input_identity(&root).as_deref()
                    == Some(self.canonical_root.as_deref().unwrap_or(&self.root))
        }) && self.unchanged()
    }

    pub(super) fn entry_path(&self, path: &Path) -> io::Result<&Path> {
        let identity = self
            .sources
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
            })?;
        if identity.to_str().is_some() {
            return Ok(identity);
        }
        // The analyzer's paths are strings, so a captured native entry whose
        // own bytes are not UTF-8 cannot be named there. Keep the captured
        // identity (findings still resolve through it) but hand the analyzer a
        // retained UTF-8 spelling of the same source, which the overlay map
        // also carries. Never fall back to a lossy rewrite.
        Ok(self
            .source_aliases
            .get_key_value(path)
            .filter(|(alias, target)| target.as_path() == identity && alias.to_str().is_some())
            .map(|(alias, _)| alias.as_path())
            .or_else(|| utf8_spelling(&self.source_aliases, identity))
            .unwrap_or(identity))
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
            || missing_input_covered(path, &normalized, &self.missing_paths)
            || self.aliases.keys().any(|alias| path.starts_with(alias))
            || self.source_aliases.contains_key(path)
            || self.source_aliases.contains_key(&normalized)
            || self.rule_inputs.contains_key(path)
            || self.rule_inputs.contains_key(&normalized)
            // The analyzer compiles its rule program from these saved inputs
            // too, so a watched event that creates a previously missing rule
            // helper, or names a lexical alias of one, must route here.
            || self.rule_aliases.contains_key(path)
            || self.rule_aliases.contains_key(&normalized)
            || self.rule_aliases.keys().any(|alias| path.starts_with(alias))
            || self.rule_aliases.values().any(|identity| identity == path)
            || missing_input_covered(path, &normalized, &self.rule_missing)
    }

    // One canonical read-set coverage predicate. It is pure on its inputs: the
    // routing caller resolves each event path's identity once, outside its
    // per-project and per-result loops, so coverage never repeats filesystem
    // work for every captured project.
    pub(super) fn covers(&self, path: &Path, identity: Option<&Path>) -> bool {
        self.covers_path(path) || identity.is_some_and(|identity| self.covers_path(identity))
    }

    pub(super) fn unchanged(&self) -> bool {
        // A missing input (source or rule helper) that now exists, or whose
        // symlinked ancestors retarget, is an input change.
        if self
            .missing_paths
            .iter()
            .chain(self.rule_missing.iter())
            .any(|(path, identity)| path.exists() || input_identity(path) != *identity)
        {
            return false;
        }
        // Import and open-buffer paths, including symlinked parent
        // directories, must still resolve to the file the analysis captured.
        // A retarget is an input change even when the old target's bytes are
        // unchanged. Resolution goes through input_identity so virtual leaves
        // that do not exist yet are validated through their existing ancestors.
        // Rule-program spellings are checked the same way.
        if !self
            .source_aliases
            .iter()
            .chain(self.rule_aliases.iter())
            .all(|(alias, expected)| {
                input_identity(alias).is_some_and(|actual| actual == *expected)
            })
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
            if !self.covers(&path, identity.as_deref()) {
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
