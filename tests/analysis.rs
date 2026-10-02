mod support;

use support::Must;

use std::sync::Arc;

use bend2_lsp::analysis::{
    DocumentSnapshot, FoldingRange, LineIndex, Revision, TextRange, folding_ranges,
};

#[test]
fn line_index_clamps_empty_eof_and_out_of_range_coordinates() {
    let empty = LineIndex::new("");
    assert_eq!(empty.offset("", 0, 0), 0);
    assert_eq!(empty.offset("", u32::MAX, u32::MAX), 0);
    assert_eq!(empty.position("", usize::MAX), (0, 0));
    assert_eq!(empty.line_range("", 0), Some(TextRange::new(0, 0)));
    assert_eq!(empty.line_range("", 1), None);

    let trailing_newline = "x\n";
    let index = LineIndex::new(trailing_newline);
    assert_eq!(index.position(trailing_newline, 2), (1, 0));
    assert_eq!(index.offset(trailing_newline, 1, 0), 2);
    assert_eq!(index.offset(trailing_newline, 2, 0), trailing_newline.len());
}

#[test]
fn line_index_handles_lf_and_crlf_line_ends() {
    let lf = "ab\ncd";
    let lf_index = LineIndex::new(lf);
    assert_eq!(lf_index.position(lf, 2), (0, 2));
    assert_eq!(lf_index.position(lf, 3), (1, 0));
    assert_eq!(lf_index.offset(lf, 0, u32::MAX), 2);
    assert_eq!(lf_index.line_range(lf, 0), Some(TextRange::new(0, 2)));
    assert_eq!(lf_index.line_range(lf, 1), Some(TextRange::new(3, 5)));

    let crlf = "ab\r\ncd\r\n";
    let crlf_index = LineIndex::new(crlf);
    assert_eq!(crlf_index.position(crlf, 2), (0, 2));
    assert_eq!(crlf_index.position(crlf, 3), (0, 2));
    assert_eq!(crlf_index.position(crlf, 4), (1, 0));
    assert_eq!(crlf_index.line_range(crlf, 0), Some(TextRange::new(0, 2)));
    assert_eq!(crlf_index.line_range(crlf, 1), Some(TextRange::new(4, 6)));
    assert_eq!(crlf_index.line_range(crlf, 2), Some(TextRange::new(8, 8)));
}

#[test]
fn line_index_converts_utf16_and_floors_non_boundary_byte_offsets() {
    let source = "Aé🦀Z";
    let index = LineIndex::new(source);
    assert_eq!(index.offset(source, 0, 0), 0);
    assert_eq!(index.offset(source, 0, 1), 1);
    assert_eq!(index.offset(source, 0, 2), 3);
    assert_eq!(index.offset(source, 0, 3), 3);
    assert_eq!(index.offset(source, 0, 4), 7);
    assert_eq!(index.offset(source, 0, 5), 8);
    assert_eq!(index.offset(source, 0, u32::MAX), 8);
    assert_eq!(index.position(source, 2), (0, 1));
    assert_eq!(index.position(source, 6), (0, 2));
    assert_eq!(index.position(source, 7), (0, 4));
    assert_eq!(index.position(source, 8), (0, 5));
    assert_eq!(index.position(source, usize::MAX), (0, 5));
}

#[test]
fn long_unicode_lines_use_sparse_utf16_position_checkpoints() {
    let source = format!("{}🦀{}é{}", "a".repeat(31), "b".repeat(32), "c".repeat(40));
    let line_index = LineIndex::new(&source);
    let snapshot = DocumentSnapshot::new(Revision(1), source.clone());
    for (offset, expected) in [
        (31, 31),
        (32, 31),
        (34, 31),
        (35, 33),
        (66, 64),
        (67, 65),
        (68, 65),
        (69, 66),
        (109, 106),
    ] {
        assert_eq!(line_index.position(&source, offset), (0, expected));
        assert_eq!(
            snapshot.line_index.position(&snapshot.text, offset),
            (0, expected)
        );
    }

    let string_and_comment = DocumentSnapshot::new(Revision(1), "\"🦀\" #é\n".to_owned());
    assert_eq!(
        string_and_comment
            .line_index
            .position(&string_and_comment.text, 10),
        (0, 7)
    );

    let lines: Vec<_> = (0..70)
        .map(|line| if line == 64 { "é" } else { "x" })
        .collect();
    let source = lines.join("\n");
    let offset = source.find('é').must_be("find Unicode line");
    let snapshot = DocumentSnapshot::new(Revision(1), source);
    assert_eq!(
        snapshot.line_index.position(&snapshot.text, offset + 2),
        (64, 1)
    );
}

#[test]
fn document_revision_keeps_previous_snapshot_immutable() {
    let previous = Arc::new(DocumentSnapshot::new(Revision(1), "old\n".to_owned()));
    let mut new_text = previous.text.clone();
    new_text.replace_range(0..3, "newer");
    let new_index = LineIndex::new(&new_text);
    let current = Arc::new(DocumentSnapshot::with_line_index(
        Revision(2),
        new_text,
        new_index,
    ));

    assert_eq!(previous.revision, Revision(1));
    assert_eq!(previous.text, "old\n");
    assert_eq!(previous.line_index.position(&previous.text, 4), (1, 0));
    assert_eq!(current.revision, Revision(2));
    assert_eq!(current.text, "newer\n");
    assert_eq!(current.line_index.position(&current.text, 6), (1, 0));
}

#[test]
fn folding_ranges_track_nested_indented_blocks_in_source_order() {
    let source = "outer\n  child\n    grandchild\n  sibling\n\nnext\n  leaf\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());

    assert_eq!(
        folding_ranges(&snapshot),
        vec![
            FoldingRange {
                start_line: 0,
                end_line: 3,
            },
            FoldingRange {
                start_line: 1,
                end_line: 2,
            },
            FoldingRange {
                start_line: 5,
                end_line: 6,
            },
        ]
    );
}

#[test]
fn folding_preserves_blank_and_comment_line_boundaries() {
    let source = "outer\n  middle\n    inner\n      body\n\n    # comment\nnext\n  leaf\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());

    assert_eq!(
        folding_ranges(&snapshot),
        vec![
            FoldingRange {
                start_line: 0,
                end_line: 5,
            },
            FoldingRange {
                start_line: 1,
                end_line: 5,
            },
            FoldingRange {
                start_line: 2,
                end_line: 3,
            },
            FoldingRange {
                start_line: 6,
                end_line: 7,
            },
        ]
    );
}

#[test]
fn folding_handles_empty_documents_and_unclosed_syntax_at_eof() {
    let empty = DocumentSnapshot::new(Revision(1), String::new());
    assert!(folding_ranges(&empty).is_empty());

    let source = "broken(\n  nested\n    child";
    let malformed = DocumentSnapshot::new(Revision(2), source.to_owned());
    assert_eq!(
        folding_ranges(&malformed),
        vec![
            FoldingRange {
                start_line: 0,
                end_line: 2,
            },
            FoldingRange {
                start_line: 1,
                end_line: 2,
            },
        ]
    );
}
#[test]
fn syntax_index_lexes_once_and_reuses_names_imports_calls_and_context() {
    use bend2_lsp::analysis::{TokenKind, completion_items, identifier_ranges, signature_help};

    let source = r#"import lib.bend as Lib
def target(a: Type, b: Type):
  "target(y) \" ( [ #"
  # target(x, [)
  target(1, nested(2, 3),
"#;
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let imports = bend2_lsp::analysis::imports(&snapshot);
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].path_text(source), "lib.bend");
    assert_eq!(imports[0].alias_text(source), Some("Lib"));

    let targets = identifier_ranges(&snapshot, "target");
    assert_eq!(targets.len(), 2, "comments and strings are not identifiers");
    let target_id = snapshot
        .syntax
        .name_id(source, "target")
        .and_then(|name| snapshot.syntax.symbol_by_name(name))
        .must_be("indexed target declaration")
        .id;
    let target_references: Vec<_> = snapshot.syntax.references(target_id).collect();
    assert_eq!(target_references.len(), 2);
    assert!(
        target_references
            .iter()
            .any(|reference| reference.kind == bend2_lsp::analysis::ReferenceKind::Declaration)
    );
    let recursive_call = snapshot
        .syntax
        .calls_to(target_id)
        .next()
        .must_be("indexed recursive call");
    assert_eq!(recursive_call.caller, Some(target_id));
    assert_eq!(
        snapshot
            .syntax
            .call_arguments(recursive_call)
            .iter()
            .map(|range| &source[range.start..range.end])
            .collect::<Vec<_>>(),
        ["1", "nested(2, 3)"]
    );
    assert_eq!(
        snapshot.syntax.call_at(source.len()).map(|(_, n)| n),
        Some(2)
    );
    assert_eq!(completion_items(&snapshot, "tar")[0].label, "target");
    let string = snapshot
        .syntax
        .tokens()
        .iter()
        .find(|token| token.kind == TokenKind::StringLiteral)
        .must_be("quoted source must have one string token");
    assert!(string.flags.escaped());
    assert!(
        snapshot
            .syntax
            .is_in_comment_or_string(source.find("target(y)").must_be("string text") + 2)
    );
    assert!(
        snapshot
            .syntax
            .is_in_comment_or_string(source.find("# target").must_be("comment text") + 3)
    );

    let help = signature_help(&snapshot, source.len()).must_be("incomplete call remains indexed");
    assert_eq!(help.parameters, ["a: Type", "b: Type"]);
    assert_eq!(help.active_parameter, 1);
}

#[test]
fn syntax_index_keeps_malformed_literal_hole_and_delimiter_diagnostics() {
    use bend2_lsp::analysis::DiagnosticKind;

    let snapshot = DocumentSnapshot::new(
        Revision(1),
        "def main: Type\n  ?TODO\n  value[)\n  \"unfinished\n".to_owned(),
    );
    let diagnostics = snapshot.syntax.diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::UnresolvedHole)
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::UnterminatedString('"'))
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.kind == DiagnosticKind::UnmatchedDelimiter(')') })
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::UnclosedDelimiter('['))
    );
}
#[test]
fn resolved_names_keep_parameter_and_pattern_shadowing_local() {
    use bend2_lsp::analysis::identifier_ranges;

    let source = "def target(value):\n  value\ndef shadow(target):\n  target(1)\ndef main:\n  target(2)\ndef choose(value):\n  match value:\n    case Some(item):\n      item\n    case None:\n      0\n  item\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let target_name = snapshot
        .syntax
        .name_id(source, "target")
        .must_be("target name");
    let target = snapshot
        .syntax
        .symbol_by_name(target_name)
        .must_be("top-level target");
    assert_eq!(identifier_ranges(&snapshot, "target").len(), 2);

    let shadow_call = source.find("target(1)").must_be("shadowed call");
    let local_token = snapshot
        .syntax
        .token_at_or_before(shadow_call)
        .must_be("local target token");
    let local = snapshot
        .syntax
        .symbol_for_token(local_token)
        .must_be("resolved local binding");
    assert_ne!(local, target.id);
    let binding = snapshot
        .syntax
        .binding_by_id(local)
        .must_be("parameter binding");
    assert_eq!(snapshot.syntax.name_text(source, binding.name), "target");
    assert_eq!(snapshot.syntax.references(local).count(), 2);

    let branch_use = source.find("      item").must_be("pattern branch binding") + 6;
    let branch_token = snapshot
        .syntax
        .token_at_or_before(branch_use)
        .must_be("pattern variable token");
    let branch_id = snapshot
        .syntax
        .symbol_for_token(branch_token)
        .must_be("resolved pattern variable");
    let branch = snapshot
        .syntax
        .binding_by_id(branch_id)
        .must_be("pattern binding");
    assert!(branch.scope.contains(branch_use));

    let outside_use = source.rfind("  item").must_be("out-of-scope variable") + 2;
    let outside_token = snapshot
        .syntax
        .token_at_or_before(outside_use)
        .must_be("out-of-scope token");
    assert_eq!(snapshot.syntax.symbol_for_token(outside_token), None);
}

fn assert_exact_output<T: std::fmt::Debug + PartialEq>(name: &str, candidate: &[T], legacy: &[T]) {
    assert_eq!(candidate.len(), legacy.len(), "{name} count");
    for (index, (actual, expected)) in candidate.iter().zip(legacy).enumerate() {
        assert_eq!(actual, expected, "{name}[{index}] differs");
    }
}

fn assert_legacy_fixture_outputs_match_indexed_queries_exactly(
    source: &str,
    golden_json: &str,
    identifier_name: &str,
    completion_prefix: &str,
) {
    use bend2_lsp::analysis::{self, CompletionKind};
    use serde_json::Value;
    use tower_lsp::lsp_types::CompletionItemKind;

    let golden: Value =
        serde_json::from_str(golden_json).must_be("legacy output fixture must parse");
    assert_eq!(
        golden["source_bytes"]
            .as_u64()
            .must_be("legacy fixture size must be an integer"),
        u64::try_from(source.len()).must_be("fixture size fits u64")
    );

    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let semantic_tokens: Vec<[u32; 5]> = analysis::semantic_tokens(&snapshot)
        .into_iter()
        .map(|token| {
            let start = snapshot
                .line_index
                .position(&snapshot.text, token.range.start);
            let end = snapshot
                .line_index
                .position(&snapshot.text, token.range.end);
            [start.0, start.1, end.0, end.1, token.token_type]
        })
        .collect();
    let legacy_tokens: Vec<[u32; 5]> = serde_json::from_value(golden["semantic_tokens"].clone())
        .must_be("legacy semantic tokens must match the canonical shape");
    assert_exact_output(
        "semantic token (line, range, kind)",
        &semantic_tokens,
        &legacy_tokens,
    );

    let identifier_ranges: Vec<[u32; 4]> = analysis::identifier_ranges(&snapshot, identifier_name)
        .into_iter()
        .map(|range| {
            let start = snapshot.line_index.position(&snapshot.text, range.start);
            let end = snapshot.line_index.position(&snapshot.text, range.end);
            [start.0, start.1, end.0, end.1]
        })
        .collect();
    let legacy_ranges: Vec<[u32; 4]> = serde_json::from_value(golden["identifier_ranges"].clone())
        .must_be("legacy identifier ranges must match the canonical shape");
    assert_exact_output(
        "identifier range (line and range)",
        &identifier_ranges,
        &legacy_ranges,
    );

    let completion_items: Vec<(String, String, u32)> =
        analysis::completion_items(&snapshot, completion_prefix)
            .into_iter()
            .map(|item| {
                let kind = match item.kind {
                    CompletionKind::Function => CompletionItemKind::FUNCTION,
                    CompletionKind::Struct => CompletionItemKind::STRUCT,
                    CompletionKind::Keyword => CompletionItemKind::KEYWORD,
                };
                let kind = serde_json::to_value(kind)
                    .must_be("completion kind must serialize")
                    .as_u64()
                    .must_be("completion kind must be a number");
                (
                    item.label,
                    item.detail,
                    u32::try_from(kind).must_be("completion kind fits u32"),
                )
            })
            .collect();
    let legacy_completions: Vec<(String, String, u32)> =
        serde_json::from_value(golden["completion_items"].clone())
            .must_be("legacy completions must match the canonical shape");
    assert_exact_output(
        "completion (label, detail, kind)",
        &completion_items,
        &legacy_completions,
    );
}

fn assert_legacy_inlay_hints_match_exactly(source: &str, golden_json: &str, size: &str) {
    use bend2_lsp::analysis;

    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let actual: Vec<(u32, u32, String)> =
        analysis::inlay_hints(&snapshot, TextRange::new(0, snapshot.text.len()))
            .into_iter()
            .map(|hint| {
                let position = snapshot.line_index.position(&snapshot.text, hint.position);
                (position.0, position.1, hint.label(&snapshot.text))
            })
            .collect();
    let expected: Vec<(u32, u32, String)> =
        serde_json::from_str(golden_json).must_be("legacy inlay hints must parse");
    assert_exact_output(
        &format!("{size} inlay hint (position, parameter label)"),
        &actual,
        &expected,
    );
}

#[test]
fn medium_and_large_inlay_hints_match_legacy_exactly() {
    assert_legacy_inlay_hints_match_exactly(
        include_str!("../benches/fixtures/analyzer_medium.bend"),
        include_str!("fixtures/analysis_legacy_inlay_medium.json"),
        "medium",
    );
    assert_legacy_inlay_hints_match_exactly(
        include_str!("../benches/fixtures/analyzer_large.bend"),
        include_str!("fixtures/analysis_legacy_inlay_large.json"),
        "large",
    );
}

#[test]
fn small_legacy_fixture_outputs_match_indexed_queries_exactly() {
    assert_legacy_fixture_outputs_match_indexed_queries_exactly(
        include_str!("../benches/fixtures/analyzer_input.bend"),
        include_str!("fixtures/analysis_legacy_outputs.json"),
        "transform_31",
        "transform_",
    );
}

#[test]
fn medium_and_large_legacy_fixture_outputs_match_indexed_queries_exactly() {
    assert_legacy_fixture_outputs_match_indexed_queries_exactly(
        include_str!("../benches/fixtures/analyzer_medium.bend"),
        include_str!("fixtures/analysis_legacy_medium_outputs.json"),
        "worker_0199",
        "worker_",
    );
    assert_legacy_fixture_outputs_match_indexed_queries_exactly(
        include_str!("../benches/fixtures/analyzer_large.bend"),
        include_str!("fixtures/analysis_legacy_large_outputs.json"),
        "worker_0599",
        "worker_",
    );
}

#[test]
fn constructor_definitions_preserve_source_order_when_names_were_seen_earlier() {
    for prefix in ["", "def prior(value: Second) -> Nat:\n  0n\n\n"] {
        let source = format!(
            "{prefix}type FirstType is Data:\n  First{{}}\n  Second{{}}\n\
             type OtherType is Data:\n  First{{}}\n"
        );
        let snapshot = DocumentSnapshot::new(Revision(1), source.clone());
        for name in ["First", "Second"] {
            let start = source
                .find(&format!("  {name}"))
                .must_be("constructor declaration")
                + 2;
            assert_eq!(
                bend2_lsp::analysis::declaration_range(&snapshot, name),
                Some(TextRange::new(start, start + name.len()))
            );
        }
    }
}
