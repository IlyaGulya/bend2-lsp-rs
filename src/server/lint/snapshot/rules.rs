use super::{
    FILE_LIMIT, Identities,
    disk::{disk_matches, is_bend, read_source},
    paths::{TrackedInputs, input_identity},
};
use crate::{
    analysis::{self, DocumentSnapshot, Revision},
    workspace::normalize_path,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

// The analyzer compiles the rule program from disk: every file reachable from
// a rule file by a local import is executed code. Keep its bytes, the lexical
// spellings that reached it, and the targets that do not exist yet, so the
// same alias/creation fences as captured sources apply to rule inputs too.
#[derive(Default)]
pub(super) struct RuleClosure {
    pub(super) inputs: TrackedInputs,
    pub(super) snapshots: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
}

pub(super) fn trusted_input(
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
pub(super) fn rule_closure(rule_paths: &HashSet<PathBuf>) -> io::Result<RuleClosure> {
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
        closure.inputs.insert_alias(&path, &key)?;
        if !visited.insert(key) {
            continue;
        }
        if !path.exists() {
            closure.inputs.insert_missing(path, identity)?;
            continue;
        }
        if closure.inputs.identities.len() >= FILE_LIMIT {
            return Err(io::Error::other("bend-lint rule inputs exceed 4096 files"));
        }
        let (canonical, text) = read_source(&path, &mut bytes)?;
        let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
        let digest: [u8; 32] = Sha256::digest(snapshot.text.as_bytes()).into();
        closure.inputs.identities.insert(canonical.clone(), digest);
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

pub(super) fn ensure_trusted_inputs_saved(
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
