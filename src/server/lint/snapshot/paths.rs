use super::{CONFIG_NAME, ENTRY_LIMIT, FILE_LIMIT, Identities};
use crate::workspace::{Document, normalize_path};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};
use url::Url;

// The three fences shared by every captured read set: content identities by
// canonical path, lexical spellings that resolved to them, and inputs that do
// not exist yet. Source read-set and trusted-rule closure use this mechanism
// independently; the trust and overlay policies stay in their own modules.
#[derive(Default)]
pub(super) struct TrackedInputs {
    pub(super) identities: Identities,
    pub(super) aliases: BTreeMap<PathBuf, PathBuf>,
    pub(super) missing: BTreeMap<PathBuf, Option<PathBuf>>,
}

impl TrackedInputs {
    pub(super) fn insert_alias(&mut self, original: &Path, canonical: &Path) -> io::Result<()> {
        // Resolver paths and document URIs can spell the same lexical path
        // differently (`.`/`..` segments). Store both ends normalized so alias
        // lookup, coverage, and later input_identity validation agree.
        let original = normalize_path(original);
        let canonical = normalize_path(canonical);
        // Distinct lexical aliases can resolve to one canonical source, so the
        // source count budget does not bound this map. Every original path is
        // retained (ordinary, virtual, or symlinked), and the existing entry
        // budget bounds it with a clear failure instead of silent truncation.
        if !self.aliases.contains_key(&original) && self.aliases.len() >= ENTRY_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 32768 source aliases",
            ));
        }
        self.aliases.insert(original, canonical);
        Ok(())
    }

    pub(super) fn insert_missing(
        &mut self,
        path: PathBuf,
        identity: Option<PathBuf>,
    ) -> io::Result<()> {
        if self.missing.contains_key(&path) {
            return Ok(());
        }
        if self.missing.len() >= FILE_LIMIT {
            return Err(io::Error::other(
                "bend-lint snapshot exceeds 4096 missing inputs",
            ));
        }
        self.missing.insert(path, identity);
        Ok(())
    }

    pub(super) fn merge_parts(
        &mut self,
        aliases: &BTreeMap<PathBuf, PathBuf>,
        missing: &BTreeMap<PathBuf, Option<PathBuf>>,
    ) -> io::Result<()> {
        for (original, identity) in aliases {
            self.insert_alias(original, identity)?;
        }
        for (path, identity) in missing {
            self.insert_missing(path.clone(), identity.clone())?;
        }
        Ok(())
    }
}

// A captured native identity whose own path is not UTF-8 still has representable
// spellings when an editor, import, or symlink reached the same source under a
// UTF-8 alias. Map order makes the choice deterministic; this only reports a
// retained lexical spelling and never rewrites the identity.
pub(super) fn utf8_spelling<'a>(
    aliases: &'a BTreeMap<PathBuf, PathBuf>,
    identity: &Path,
) -> Option<&'a Path> {
    aliases.iter().find_map(|(alias, target)| {
        (target.as_path() == identity && alias.to_str().is_some()).then_some(alias.as_path())
    })
}

// Routing coverage for an input that may not exist yet: the event can name the
// missing path, an ancestor directory that was just created, or the resolved
// identity of the missing input. Keys are stored capture spellings, so this
// stays a map lookup instead of filesystem work on every routing query.
pub(super) fn missing_input_covered(
    path: &Path,
    normalized: &Path,
    inputs: &BTreeMap<PathBuf, Option<PathBuf>>,
) -> bool {
    inputs.contains_key(path)
        || inputs.contains_key(normalized)
        || inputs.keys().any(|pending| pending.starts_with(path))
        || inputs.values().any(|identity| {
            identity
                .as_deref()
                .is_some_and(|identity| identity == path || identity.starts_with(path))
        })
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

// Findings must reach every editor spelling that currently holds this source.
// Same-content aliases share the analysis but each URI receives its own
// diagnostic packet with that view's version. Closed sources keep one logical
// URI, exactly as before.
pub(super) fn source_uris(
    documents: &[Document],
    sources: &BTreeMap<PathBuf, std::sync::Arc<crate::analysis::DocumentSnapshot>>,
    root: &Path,
    canonical_root: Option<&Path>,
    aliases: &BTreeMap<PathBuf, PathBuf>,
) -> BTreeMap<PathBuf, Vec<Url>> {
    let mut uris: BTreeMap<PathBuf, Vec<Url>> = sources
        .keys()
        .filter_map(|path| {
            logical_uri(path, root, canonical_root, aliases).map(|uri| (path.clone(), vec![uri]))
        })
        .collect();
    let mut open: BTreeMap<PathBuf, BTreeSet<Url>> = BTreeMap::new();
    for document in documents {
        let Ok(path) = document.uri.to_file_path() else {
            continue;
        };
        let Some(identity) = input_identity(&path) else {
            continue;
        };
        if sources.contains_key(&identity) {
            open.entry(identity)
                .or_default()
                .insert(document.uri.clone());
        }
    }
    for (identity, editor_uris) in open {
        uris.insert(identity, editor_uris.into_iter().collect());
    }
    uris
}

pub(in crate::server::lint) fn project_root(file: &Path) -> Option<PathBuf> {
    file.parent().and_then(|parent| {
        parent
            .ancestors()
            .find(|path| path.join(CONFIG_NAME).exists())
            .map(Path::to_path_buf)
    })
}

// Resolve existing ancestors too: virtual or missing imported files still have
// an identity through a symlinked parent, even before the leaf exists on disk.
pub(in crate::server::lint) fn input_identity(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Some(canonical);
    }
    path.parent()?.ancestors().find_map(|ancestor| {
        let mut canonical = fs::canonicalize(ancestor).ok()?;
        canonical.push(path.strip_prefix(ancestor).ok()?);
        Some(canonical)
    })
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
