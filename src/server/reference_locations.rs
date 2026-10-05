use crate::{
    analysis::{DocumentSnapshot, TextRange},
    workspace::WorkspaceOccurrence,
};
use tower_lsp::lsp_types::{Location, Position, Range};

fn reference_range(snapshot: &DocumentSnapshot, range: TextRange) -> Range {
    let (start_line, start_character) = snapshot.line_index.position(&snapshot.text, range.start);
    let (end_line, end_character) = snapshot.line_index.position(&snapshot.text, range.end);
    Range::new(
        Position::new(start_line, start_character),
        Position::new(end_line, end_character),
    )
}

// The binding request clones the URI even though each occurrence is owned.
pub(super) fn binding_reference_locations(occurrences: Vec<WorkspaceOccurrence>) -> Vec<Location> {
    occurrences
        .into_iter()
        .map(|occurrence| Location {
            uri: occurrence.document.uri.clone(),
            range: reference_range(&occurrence.document, occurrence.range),
        })
        .collect()
}

// The owned conversion stage moves each URI after converting its byte range.
pub(super) fn owned_reference_locations(occurrences: Vec<WorkspaceOccurrence>) -> Vec<Location> {
    occurrences
        .into_iter()
        .map(|occurrence| {
            let range = reference_range(&occurrence.document, occurrence.range);
            Location {
                uri: occurrence.document.uri,
                range,
            }
        })
        .collect()
}

// Symbol orchestration retains its additional protocol-level sort and dedup.
pub(super) fn symbol_reference_locations(occurrences: Vec<WorkspaceOccurrence>) -> Vec<Location> {
    let mut locations = owned_reference_locations(occurrences);
    locations.sort_unstable_by(|left, right| {
        left.uri
            .as_str()
            .cmp(right.uri.as_str())
            .then_with(|| left.range.start.line.cmp(&right.range.start.line))
            .then_with(|| left.range.start.character.cmp(&right.range.start.character))
    });
    locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
    locations
}
