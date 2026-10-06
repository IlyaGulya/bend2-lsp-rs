use std::{collections::BTreeMap, fs, path::Path, sync::LazyLock};

use serde_json::{Value, json};
use tower_lsp::lsp_types::{Location, Range};

use super::{
    Must,
    consumers::{self, CallGroup},
    fixtures::{self, Fixture},
    stages,
};

fn relative(fixture: &Fixture, uri: &url::Url) -> String {
    uri.to_file_path()
        .must_be("probe file URI")
        .strip_prefix(fixture.directory.path())
        .must_be("probe URI root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn canonical_range(range: Range) -> Value {
    json!({"start": {"line": range.start.line, "character": range.start.character},
        "end": {"line": range.end.line, "character": range.end.character}})
}

fn locations(fixture: &Fixture, locations: Vec<Location>) -> Value {
    let rows: Vec<_> = locations.into_iter().map(|location| {
        json!({"uri": relative(fixture, &location.uri), "range": canonical_range(location.range)})
    }).collect();
    Value::Array(rows)
}

fn calls(fixture: &Fixture, groups: Vec<CallGroup>) -> Value {
    let mut rows: Vec<_> = groups.into_iter().map(|group| {
        let symbol = group.document.syntax.symbol_by_id(group.symbol).must_be("probe hierarchy symbol");
        let mut ranges = group.ranges;
        ranges.sort_by_key(|range| (range.start.line, range.start.character, range.end.line, range.end.character));
        json!({"uri": relative(fixture, &group.document.uri),
            "name": group.document.syntax.name_text(&group.document.text, symbol.name),
            "range": canonical_range(consumers::range(&group.document, symbol.scope)),
            "selection_range": canonical_range(consumers::range(&group.document, symbol.name_range)),
            "source_uri": relative(fixture, &group.source.uri),
            "from_ranges": ranges.into_iter().map(canonical_range).collect::<Vec<_>>()})
    }).collect();
    rows.sort_by_cached_key(Value::to_string);
    Value::Array(rows)
}

pub(super) fn write(path: &Path) {
    let mut outputs = BTreeMap::new();
    for (label, lazy) in fixtures::cases() {
        let fixture = LazyLock::force(lazy);
        let view = consumers::reference_lookup(fixture);
        let materialized = consumers::reference_materialize(fixture, &view);
        let whole = consumers::references(fixture, "target.bend", "identity", false);
        assert_eq!(
            materialized, whole,
            "split pipeline differs from whole consumer: {label}"
        );
        let mut symbols = BTreeMap::new();
        for (relative, name) in [
            ("target.bend", "identity"),
            ("target.bend", "alternate"),
            ("target.bend", "local"),
            ("client00001.bend", "helper"),
            ("client00001.bend", "client"),
            ("client00001.bend", "second"),
        ] {
            let changes = consumers::rename_symbol(fixture, relative, name)
                .changes
                .must_be("rename edits");
            let edits: BTreeMap<_, _> = changes
                .into_iter()
                .map(|(uri, changes)| {
                    let changes: Vec<_> = changes.into_iter().map(|edit| {
                    json!({"range": canonical_range(edit.range), "new_text": edit.new_text})
                }).collect();
                    (self::relative(fixture, &uri), changes)
                })
                .collect();
            symbols.insert(format!("{relative}::{name}"), json!({
                "references": locations(fixture, consumers::references(fixture, relative, name, false)),
                "references_with_declaration": locations(fixture, consumers::references(fixture, relative, name, true)),
                "rename": edits,
                "incoming": calls(fixture, consumers::incoming_symbol(fixture, relative, name)),
                "outgoing": calls(fixture, consumers::outgoing_symbol(fixture, relative, name)),
            }));
        }
        outputs.insert(label, json!({"reference_materialization": locations(fixture, materialized), "symbols": symbols}));
    }
    // Exercise cold entry points outside Callgrind, retaining their real results.
    let initial = stages::initial_build(fixtures::InitialSources::new());
    assert_eq!(initial.database.indexed_documents().len(), 100);
    assert!(initial.directory.path().is_dir());
    let cold = stages::cold_workspace_build(fixtures::ColdSources::new(100, 3));
    assert_eq!(
        consumers::references(&cold, "target.bend", "identity", false).len(),
        14
    );
    let result = json!({
        "semantic_outputs": outputs,
        "metadata": {
            "diagnostic_identity": cfg!(feature = "decomp-identity"),
            "occurrence_index": cfg!(feature = "decomp-occurrences"),
            "call_index": cfg!(feature = "decomp-calls"),
            "reference_consumer": if cfg!(feature = "decomp-ref-consumer") { "indexed" } else { "actual_A_legacy" },
            "call_consumer": if cfg!(feature = "decomp-call-consumer") { "indexed" } else { "actual_A_legacy" },
            "semantic_only_cold_build": if cfg!(feature = "decomp-identity") { "present" } else { "N/A_actual_A_has_no_semantic_index" },
            "reference_lookup": "target selection and real local/external groups; output preparation inside exact entry",
            "reference_materialization": "prepared selection only; output allocation, URI ownership, UTF16, sort/dedup inside exact entry; no repeated workspace selection",
            "returned_output_drop": "outside exact entry",
        }
    });
    fs::write(
        path,
        serde_json::to_vec_pretty(&result).must_be("serialize semantic probes"),
    )
    .must_be("write semantic probes");
}
