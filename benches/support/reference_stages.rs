use crate::{analysis, reference_locations, workspace};
use tower_lsp::lsp_types::Location;

// Exact non-inlined entry points avoid the default wrapper wildcard also
// matching outlined iterator/drop code. Setup and returned output drop stay out.
#[inline(never)]
pub(super) fn ranges(
    groups: &[workspace::ExternalReferenceGroup<'_>],
) -> Vec<(workspace::FileId, analysis::TextRange)> {
    let mut output = Vec::new();
    for group in groups {
        for row in group.occurrences() {
            output.push((group.source(), row.range));
        }
    }
    output
}

#[inline(never)]
pub(super) fn materialize(
    database: &workspace::WorkspaceDb,
    target: workspace::GlobalSymbolId,
) -> Vec<workspace::WorkspaceOccurrence> {
    database.references_unsorted(target, false)
}

#[inline(never)]
pub(super) fn sort(occurrences: &mut [workspace::WorkspaceOccurrence]) {
    workspace::WorkspaceOccurrence::sort(occurrences);
}

#[inline(never)]
pub(super) fn dedup(occurrences: &mut Vec<workspace::WorkspaceOccurrence>) {
    workspace::WorkspaceOccurrence::dedup(occurrences);
}

#[inline(never)]
pub(super) fn binding(occurrences: Vec<workspace::WorkspaceOccurrence>) -> Vec<Location> {
    reference_locations::binding_reference_locations(occurrences)
}

#[inline(never)]
pub(super) fn owned(occurrences: Vec<workspace::WorkspaceOccurrence>) -> Vec<Location> {
    reference_locations::owned_reference_locations(occurrences)
}

#[inline(never)]
pub(super) fn symbol_pipeline(occurrences: Vec<workspace::WorkspaceOccurrence>) -> Vec<Location> {
    reference_locations::symbol_reference_locations(occurrences)
}
