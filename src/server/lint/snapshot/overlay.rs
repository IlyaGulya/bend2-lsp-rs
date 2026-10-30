use super::{Identities, disk::disk_matches, rules::trusted_input, sources::Captured};
use crate::analysis::DocumentSnapshot;
use std::{
    collections::{BTreeMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

// Source overlays are a distinct policy from trusted saved rule inputs: every
// non-trusted captured source is sent, while rule/config bytes stay on disk.
pub(super) fn overlay_map(
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
        // non-UTF-8 source is represented by its retained UTF-8 aliases below;
        // a dirty one is never silently replaced by its saved text.
        if path.to_str().is_some() {
            overlays.insert(path.clone(), snapshot.clone());
        }
    }
    // A native source whose own bytes are not UTF-8 cannot name itself in the
    // analyzer's string paths. Every retained UTF-8 spelling of that captured
    // identity is a real alias of the same saved source, so it carries the
    // exact captured text as an overlay. The analyzer resolves that overlay
    // instead of folding the native name through a lossy string; no rename,
    // copy, or fallback is introduced.
    for (alias, identity) in &captured.inputs.aliases {
        if alias.to_str().is_none()
            || identity.to_str().is_some()
            || trusted_input(config_path, rule_paths, rule_inputs, identity)
        {
            continue;
        }
        if let Some(snapshot) = captured.sources.get(identity) {
            overlays.insert(alias.clone(), snapshot.clone());
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
