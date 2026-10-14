use tower_lsp::lsp_types::Location;

pub(super) fn sort_and_deduplicate(locations: &mut Vec<Location>) {
    locations.sort_by_key(|location| {
        (
            location.uri.to_string(),
            location.range.start.line,
            location.range.start.character,
        )
    });
    locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
}
