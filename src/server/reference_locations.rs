use tower_lsp::lsp_types::Location;

pub(super) fn sort_and_deduplicate(locations: &mut Vec<Location>) {
    locations.sort_by(|left, right| {
        (
            left.uri.as_str(),
            left.range.start.line,
            left.range.start.character,
        )
            .cmp(&(
                right.uri.as_str(),
                right.range.start.line,
                right.range.start.character,
            ))
    });
    locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
}
