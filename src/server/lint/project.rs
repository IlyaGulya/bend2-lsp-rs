use super::{config::LintConfig, snapshot, snapshot::ProjectSnapshot};
use crate::workspace::normalize_path;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
use tokio::task::JoinHandle;
use url::Url;

// Lint work is owned by the project whose inputs it reads, never by one global
// scheduler. A document without a discoverable project still gets an
// independent state keyed by its own URI, so unrelated editors cannot cancel
// each other while bounded analyzer resources stay global.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum ProjectKey {
    Root(PathBuf),
    Document(Url),
}

impl ProjectKey {
    // Whether a filesystem change is routed to this project: its root, a
    // lexical or physical spelling inside it, or the exact document view.
    pub(super) fn involves(&self, paths: &[PathBuf]) -> bool {
        match self {
            Self::Root(root) => paths.iter().any(|path| {
                path.starts_with(root)
                    || normalize_path(path).starts_with(root)
                    || snapshot::input_identity(path)
                        .is_some_and(|identity| identity.starts_with(root))
            }),
            Self::Document(uri) => paths.iter().any(|path| {
                uri.to_file_path().is_ok_and(|opened| {
                    opened == *path
                        || snapshot::input_identity(&opened)
                            .zip(snapshot::input_identity(path))
                            .is_some_and(|(opened, target)| opened == target)
                })
            }),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PreparationKey {
    pub(super) root: PathBuf,
    pub(super) config: LintConfig,
    pub(super) inputs: [u8; 32],
}

pub(super) enum PreparationPhase {
    Running,
    Ready(String),
    Failed(String),
}

// Preparation belongs to the project/analyzer configuration, never to a
// document revision: typing reuses an in-flight or completed preparation and
// only configuration, trust, or project identity changes can restart it.
pub(super) struct Preparation {
    pub(super) key: PreparationKey,
    pub(super) phase: PreparationPhase,
    pub(super) task: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct ProjectState {
    pub(super) revision: u64,
    pub(super) pending: BTreeSet<Url>,
    // Entries already attempted in this revision. A late schedule for the same
    // revision must not run one again; a real input event bumps the revision
    // and clears this set.
    pub(super) completed: BTreeSet<Url>,
    pub(super) active: Option<Url>,
    pub(super) task: Option<(u64, JoinHandle<()>)>,
    pub(super) preparation: Option<Preparation>,
    // One immutable project view per input revision, reused by every entry the
    // revision owns instead of each entry capturing an incompatible view.
    pub(super) view: Option<(u64, Arc<ProjectSnapshot>)>,
}
