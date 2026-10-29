use super::{LintResult, ProjectKey, snapshot};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use url::Url;

// One current entry that indexes a document view. `entry` is the run's root
// URI; it is the deterministic tie-break when epochs are equal.
struct Contributor {
    entry: Url,
    result: Arc<LintResult>,
}

#[derive(Default)]
pub(super) struct LintResults {
    entries: HashMap<Url, Arc<LintResult>>,
    // Every document a run contributes to: its own root and each reported
    // target. Keeping all contributors is what lets a newer empty own report
    // coexist with a still-current imported finding from another run.
    contributors: HashMap<Url, Vec<Contributor>>,
}

impl LintResults {
    // Newest first, then entry identity. This is the only ordering a union
    // consumer sees, so output never depends on HashMap iteration order.
    pub(super) fn contributors(&self, uri: &Url) -> Vec<Arc<LintResult>> {
        self.contributors
            .get(uri)
            .map_or_else(Vec::new, |contributors| {
                contributors
                    .iter()
                    .map(|contributor| contributor.result.clone())
                    .collect()
            })
    }

    pub(super) fn snapshot_for(&self, path: &Path) -> Option<Arc<snapshot::ProjectSnapshot>> {
        self.entries
            .values()
            .filter(|result| path.starts_with(&result.snapshot.root))
            .max_by_key(|result| result.snapshot.root.as_os_str().len())
            .map(|result| result.snapshot.clone())
    }

    pub(super) fn entries_of(&self, project: &ProjectKey) -> Vec<Url> {
        self.entries
            .iter()
            .filter(|(_, result)| &result.project == project)
            .map(|(uri, _)| uri.clone())
            .collect()
    }

    // The project that owns this entry's retained result. Project membership is
    // discovered from disk, so a retained result proves the ownership an entry
    // had before a configuration appeared or disappeared.
    pub(super) fn project_of(&self, uri: &Url) -> Option<&ProjectKey> {
        self.entries.get(uri).map(|result| &result.project)
    }

    // Retained coverage for one event, using the identities the caller already
    // resolved for this event instead of resolving them per retained entry.
    pub(super) fn projects_covering(
        &self,
        spellings: &[(PathBuf, Option<PathBuf>)],
    ) -> Vec<ProjectKey> {
        let mut projects: BTreeSet<ProjectKey> = BTreeSet::new();
        for result in self.entries.values() {
            if super::covered_by(&result.snapshot, spellings) {
                projects.insert(result.project.clone());
            }
        }
        projects.into_iter().collect()
    }

    pub(super) fn affected_of(&self, project: &ProjectKey) -> HashSet<Url> {
        self.entries
            .iter()
            .filter(|(_, result)| &result.project == project)
            .flat_map(|(uri, result)| {
                std::iter::once(uri.clone()).chain(result.findings.by_uri.keys().cloned())
            })
            .collect()
    }

    pub(super) fn affected_keys(&self) -> HashSet<Url> {
        self.contributors.keys().cloned().collect()
    }

    pub(super) fn clear(&mut self) -> HashSet<Url> {
        let affected = self.contributors.keys().cloned().collect();
        self.entries.clear();
        self.contributors.clear();
        affected
    }

    pub(super) fn insert(&mut self, uri: Url, result: Arc<LintResult>) -> HashSet<Url> {
        let mut affected: HashSet<_> = result.findings.by_uri.keys().cloned().collect();
        affected.insert(uri.clone());
        if let Some(previous) = self.entries.insert(uri, result) {
            affected.extend(previous.findings.by_uri.keys().cloned());
        }
        self.rebuild();
        affected
    }

    // Drop only entries a plan's retired revision owned. A result produced
    // under a newer project revision belongs to a newer run and must survive
    // the older plan's late cleanup.
    pub(super) fn remove_project_revision(
        &mut self,
        project: &ProjectKey,
        retired_revision: u64,
    ) -> HashSet<Url> {
        let mut changed = HashSet::new();
        let mut removed = false;
        self.entries.retain(|uri, result| {
            if &result.project == project && result.revision <= retired_revision {
                removed = true;
                changed.insert(uri.clone());
                changed.extend(result.findings.by_uri.keys().cloned());
                false
            } else {
                true
            }
        });
        if removed {
            self.rebuild();
        }
        changed
    }

    // Drop entries produced under a configuration other than the current one.
    // A newer run already using the new configuration therefore survives a
    // late settings event instead of being cleared and repeated.
    pub(super) fn remove_stale(&mut self, stale: impl Fn(&LintResult) -> bool) -> HashSet<Url> {
        let mut changed = HashSet::new();
        let mut removed = false;
        self.entries.retain(|uri, result| {
            if stale(result) {
                removed = true;
                changed.insert(uri.clone());
                changed.extend(result.findings.by_uri.keys().cloned());
                false
            } else {
                true
            }
        });
        if removed {
            self.rebuild();
        }
        changed
    }

    fn rebuild(&mut self) {
        self.contributors.clear();
        for (entry, result) in &self.entries {
            for uri in std::iter::once(entry).chain(result.findings.by_uri.keys()) {
                self.contributors
                    .entry(uri.clone())
                    .or_default()
                    .push(Contributor {
                        entry: entry.clone(),
                        result: result.clone(),
                    });
            }
        }
        for contributors in self.contributors.values_mut() {
            contributors.sort_by(|left, right| {
                right
                    .result
                    .epoch
                    .cmp(&left.result.epoch)
                    .then_with(|| left.entry.cmp(&right.entry))
            });
        }
    }
}
