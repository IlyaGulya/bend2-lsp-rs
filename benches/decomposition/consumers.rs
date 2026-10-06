#[cfg(any(
    not(feature = "decomp-ref-consumer"),
    not(feature = "decomp-call-consumer")
))]
use bend2_lsp::workspace::WorkspaceDb;
use bend2_lsp::{
    analysis::{self, SymbolId, TextRange},
    workspace::Document,
};
use std::collections::HashMap;
use tower_lsp::lsp_types::{Location, Position, Range, TextEdit, WorkspaceEdit};

use super::{Must, fixtures::Fixture};

pub(super) fn range(document: &Document, bytes: TextRange) -> Range {
    let (line, character) = document.line_index.position(&document.text, bytes.start);
    let start = Position::new(line, character);
    let (line, character) = document.line_index.position(&document.text, bytes.end);
    Range::new(start, Position::new(line, character))
}

#[cfg(not(feature = "decomp-call-consumer"))]
pub(super) fn symbol(document: &Document, name: &str) -> SymbolId {
    document
        .syntax
        .name_id(&document.text, name)
        .and_then(|name| document.syntax.symbol_by_name(name))
        .must_be("fixture symbol")
        .id
}

#[cfg(not(feature = "decomp-ref-consumer"))]
pub(super) struct ReferenceGroup {
    document: Document,
    ranges: Vec<TextRange>,
}

#[cfg(not(feature = "decomp-ref-consumer"))]
pub(super) type ReferenceView = Vec<ReferenceGroup>;
#[cfg(feature = "decomp-ref-consumer")]
pub(super) struct ReferenceView {
    local: Document,
    local_ranges: Vec<analysis::Reference>,
    groups: Vec<bend2_lsp::workspace::ExternalReferenceGroup<'static>>,
}

#[inline(never)]
pub(super) fn reference_lookup(fixture: &'static Fixture) -> ReferenceView {
    let uri = fixture.uri("target.bend");
    #[cfg(not(feature = "decomp-ref-consumer"))]
    {
        legacy_selection(&fixture.database, &uri, "identity", false)
    }
    #[cfg(feature = "decomp-ref-consumer")]
    {
        let target = fixture
            .database
            .symbol_by_name(&uri, "identity")
            .must_be("indexed target");
        let local_ranges = target
            .document
            .syntax
            .references(target.id.local_symbol())
            .filter(|row| row.kind != analysis::ReferenceKind::Declaration)
            .copied()
            .collect();
        let groups = fixture
            .database
            .external_reference_groups(target.id)
            .collect();
        ReferenceView {
            local: target.document,
            local_ranges,
            groups,
        }
    }
}

// Exact A symbol_references selection, split before protocol allocation.
// Fixture URIs have unique canonical paths, just like normalized workspace paths.
#[cfg(not(feature = "decomp-ref-consumer"))]
fn legacy_selection(
    database: &WorkspaceDb,
    target_uri: &url::Url,
    name: &str,
    include_declaration: bool,
) -> ReferenceView {
    let target_path = target_uri.to_file_path().ok();
    let Some(target) = database.cached_document(target_uri) else {
        return Vec::new();
    };
    let Some(target_symbol) = target
        .syntax
        .name_id(&target.text, name)
        .and_then(|name| target.syntax.symbol_by_name(name))
    else {
        return Vec::new();
    };
    let target_name = target.syntax.name_text(&target.text, target_symbol.name);
    let mut groups = Vec::new();
    for candidate in database.indexed_documents() {
        if candidate.language_id != "bend" {
            continue;
        }
        let candidate_path = candidate.uri.to_file_path().ok();
        let same_target = candidate.uri == *target_uri
            || target_path
                .as_ref()
                .is_some_and(|path| candidate_path.as_ref() == Some(path));
        let mut ranges = Vec::new();
        if same_target {
            let Some(candidate_name) = candidate
                .syntax
                .name_id(&candidate.text, target_name)
                .and_then(|name| candidate.syntax.symbol_by_name(name))
            else {
                continue;
            };
            for reference in candidate.syntax.references(candidate_name.id) {
                if include_declaration || reference.kind != analysis::ReferenceKind::Declaration {
                    ranges.push(reference.range);
                }
            }
        } else {
            let Some(candidate_name) = candidate.syntax.name_id(&candidate.text, target_name)
            else {
                continue;
            };
            for reference in candidate.syntax.references_named(candidate_name) {
                let Some(qualifier) = reference.qualifier else {
                    continue;
                };
                if reference.resolved.is_some()
                    || reference
                        .qualifier_token
                        .is_some_and(|token| candidate.syntax.symbol_for_token(token).is_some())
                {
                    continue;
                }
                let alias = candidate.syntax.name_text(&candidate.text, qualifier);
                let Some(module) = module_document(database, &candidate, alias) else {
                    continue;
                };
                if module.uri.to_file_path().ok() != target_path {
                    continue;
                }
                ranges.push(reference.range);
            }
        }
        if !ranges.is_empty() {
            groups.push(ReferenceGroup {
                document: candidate,
                ranges,
            });
        }
    }
    groups
}

#[cfg(any(
    not(feature = "decomp-ref-consumer"),
    not(feature = "decomp-call-consumer")
))]
fn module_document(database: &WorkspaceDb, source: &Document, alias: &str) -> Option<Document> {
    let import = analysis::imports(source)
        .iter()
        .find(|import| import.alias_text(&source.text) == Some(alias))?;
    database.import_target(&source.uri, import.path)
}

#[cfg(feature = "decomp-ref-consumer")]
fn indexed_locations(
    mut occurrences: Vec<bend2_lsp::workspace::WorkspaceOccurrence>,
) -> Vec<Location> {
    bend2_lsp::workspace::WorkspaceOccurrence::sort(&mut occurrences);
    bend2_lsp::workspace::WorkspaceOccurrence::dedup(&mut occurrences);
    let mut locations: Vec<_> = occurrences
        .into_iter()
        .map(|occurrence| {
            let converted = range(&occurrence.document, occurrence.range);
            Location {
                uri: occurrence.document.uri,
                range: converted,
            }
        })
        .collect();
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

#[inline(never)]
pub(super) fn reference_materialize(fixture: &Fixture, view: &ReferenceView) -> Vec<Location> {
    #[cfg(not(feature = "decomp-ref-consumer"))]
    {
        std::hint::black_box(fixture);
        legacy_materialize(view)
    }
    #[cfg(feature = "decomp-ref-consumer")]
    {
        let mut occurrences = Vec::new();
        for row in &view.local_ranges {
            occurrences.push(bend2_lsp::workspace::WorkspaceOccurrence {
                document: view.local.clone(),
                range: row.range,
                kind: row.kind,
            });
        }
        for group in &view.groups {
            let document = fixture
                .database
                .document_by_file_id(group.source())
                .must_be("indexed source document");
            for row in group.occurrences() {
                occurrences.push(bend2_lsp::workspace::WorkspaceOccurrence {
                    document: document.clone(),
                    range: row.range,
                    kind: row.kind,
                });
            }
        }
        indexed_locations(occurrences)
    }
}

#[cfg(not(feature = "decomp-ref-consumer"))]
fn legacy_materialize(view: &ReferenceView) -> Vec<Location> {
    let mut locations = Vec::new();
    for group in view {
        for bytes in &group.ranges {
            locations.push(Location {
                uri: group.document.uri.clone(),
                range: range(&group.document, *bytes),
            });
        }
    }
    locations.sort_by_key(|location| {
        (
            location.uri.to_string(),
            location.range.start.line,
            location.range.start.character,
        )
    });
    locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
    locations
}

pub(super) fn references(
    fixture: &Fixture,
    relative: &str,
    name: &str,
    declaration: bool,
) -> Vec<Location> {
    let uri = fixture.uri(relative);
    #[cfg(not(feature = "decomp-ref-consumer"))]
    {
        legacy_materialize(&legacy_selection(
            &fixture.database,
            &uri,
            name,
            declaration,
        ))
    }
    #[cfg(feature = "decomp-ref-consumer")]
    {
        let target = fixture
            .database
            .symbol_by_name(&uri, name)
            .must_be("indexed symbol")
            .id;
        indexed_locations(fixture.database.references_unsorted(target, declaration))
    }
}

#[inline(never)]
pub(super) fn rename(fixture: &Fixture) -> WorkspaceEdit {
    rename_symbol(fixture, "target.bend", "identity")
}

pub(super) fn rename_symbol(fixture: &Fixture, relative: &str, name: &str) -> WorkspaceEdit {
    let mut changes = HashMap::<url::Url, Vec<TextEdit>>::new();
    for location in references(fixture, relative, name, true) {
        changes.entry(location.uri).or_default().push(TextEdit {
            range: location.range,
            new_text: "renamed_identity".into(),
        });
    }
    WorkspaceEdit {
        changes: Some(changes),
        ..WorkspaceEdit::default()
    }
}

pub(super) struct CallGroup {
    pub document: Document,
    pub symbol: SymbolId,
    pub source: Document,
    pub ranges: Vec<Range>,
}

#[cfg(not(feature = "decomp-call-consumer"))]
fn resolve_call(
    database: &WorkspaceDb,
    source: &Document,
    call: &analysis::CallSite,
) -> Option<(Document, SymbolId)> {
    if let Some(callee) = call.callee {
        let symbol = source.syntax.symbol_by_id(callee)?;
        return (symbol.kind == analysis::SymbolKind::Function).then(|| (source.clone(), callee));
    }
    let qualifier = call.qualifier?;
    if call
        .qualifier_token
        .is_some_and(|token| source.syntax.symbol_for_token(token).is_some())
    {
        return None;
    }
    let alias = source.syntax.name_text(&source.text, qualifier);
    let imported = module_document(database, source, alias)?;
    let name = source.syntax.name_text(&source.text, call.name);
    let id = imported
        .syntax
        .name_id(&imported.text, name)
        .and_then(|name| imported.syntax.symbol_by_name(name))
        .filter(|symbol| symbol.kind == analysis::SymbolKind::Function)?
        .id;
    Some((imported, id))
}

#[cfg(not(feature = "decomp-call-consumer"))]
fn add_call(
    groups: &mut Vec<CallGroup>,
    document: Document,
    symbol: SymbolId,
    source: &Document,
    bytes: TextRange,
) {
    let name = document
        .syntax
        .symbol_by_id(symbol)
        .map(|symbol| document.syntax.name_text(&document.text, symbol.name))
        .must_be("call symbol");
    let converted = range(source, bytes);
    if let Some(group) = groups.iter_mut().find(|group| {
        group.document.uri == document.uri
            && group
                .document
                .syntax
                .symbol_by_id(group.symbol)
                .is_some_and(|symbol| {
                    group
                        .document
                        .syntax
                        .name_text(&group.document.text, symbol.name)
                        == name
                })
    }) {
        group.ranges.push(converted);
    } else {
        groups.push(CallGroup {
            document,
            symbol,
            source: source.clone(),
            ranges: vec![converted],
        });
    }
}

#[cfg(feature = "decomp-call-consumer")]
fn indexed_calls(groups: Vec<bend2_lsp::workspace::WorkspaceCallGroup>) -> Vec<CallGroup> {
    groups
        .into_iter()
        .map(|group| {
            let ranges = group
                .ranges
                .into_iter()
                .map(|bytes| range(&group.source, bytes))
                .collect();
            CallGroup {
                document: group.symbol.document,
                symbol: group.symbol.id.local_symbol(),
                source: group.source,
                ranges,
            }
        })
        .collect()
}

#[inline(never)]
pub(super) fn incoming(fixture: &Fixture) -> Vec<CallGroup> {
    incoming_symbol(fixture, "target.bend", "identity")
}

pub(super) fn incoming_symbol(fixture: &Fixture, relative: &str, name: &str) -> Vec<CallGroup> {
    let uri = fixture.uri(relative);
    #[cfg(feature = "decomp-call-consumer")]
    {
        let target = fixture
            .database
            .symbol_by_name(&uri, name)
            .must_be("incoming target")
            .id;
        indexed_calls(fixture.database.incoming_calls(target))
    }
    #[cfg(not(feature = "decomp-call-consumer"))]
    {
        let mut groups = Vec::new();
        for caller in fixture.database.indexed_documents() {
            if caller.language_id != "bend" {
                continue;
            }
            for call in caller.syntax.calls() {
                let Some(caller_id) = call.caller else {
                    continue;
                };
                let Some((target, target_id)) = resolve_call(&fixture.database, &caller, call)
                else {
                    continue;
                };
                if target.uri != uri
                    || target.syntax.symbol_by_id(target_id).is_none_or(|symbol| {
                        target.syntax.name_text(&target.text, symbol.name) != name
                    })
                {
                    continue;
                }
                add_call(
                    &mut groups,
                    caller.clone(),
                    caller_id,
                    &caller,
                    call.callee_range,
                );
            }
        }
        groups
    }
}

#[inline(never)]
pub(super) fn outgoing(fixture: &Fixture) -> Vec<CallGroup> {
    outgoing_symbol(fixture, "client00001.bend", "client")
}

pub(super) fn outgoing_symbol(fixture: &Fixture, relative: &str, name: &str) -> Vec<CallGroup> {
    let uri = fixture.uri(relative);
    #[cfg(feature = "decomp-call-consumer")]
    {
        let caller = fixture
            .database
            .symbol_by_name(&uri, name)
            .must_be("outgoing caller")
            .id;
        indexed_calls(fixture.database.outgoing_calls(caller))
    }
    #[cfg(not(feature = "decomp-call-consumer"))]
    {
        let caller = fixture
            .database
            .cached_document(&uri)
            .must_be("outgoing caller");
        let caller_id = symbol(&caller, name);
        let mut groups = Vec::new();
        for call in caller.syntax.calls_from(caller_id) {
            let Some((target, target_id)) = resolve_call(&fixture.database, &caller, call) else {
                continue;
            };
            add_call(&mut groups, target, target_id, &caller, call.callee_range);
        }
        groups
    }
}
