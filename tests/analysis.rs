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
def target(a: Type, b: Type) -> Type:
  ignored = "target(y) \" ( [ #"
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

    let source = "import Base\ntype Maybe is Type:\n  Some{item: U32}\n  None{}\ndef target(value: U32) -> U32:\n  value\ndef shadow(target: (U32 -> U32)) -> U32:\n  target(1)\ndef main() -> U32:\n  target(2)\ndef choose(value: Maybe) -> U32:\n  match value:\n    case Some{item}:\n      item\n    case None{}:\n      0\ndef outside() -> U32:\n  item\n";
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
) {
    use bend2_lsp::analysis;
    use serde_json::Value;

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
    assert_eq!(
        semantic_tokens.len(),
        legacy_tokens.len(),
        "semantic token count"
    );
    for (actual, legacy) in semantic_tokens.iter().zip(&legacy_tokens) {
        if actual == legacy {
            continue;
        }
        // The immutable legacy corpus predates compiler grammar resolution.
        // `case Nil: state` binds a variable: this file defines no Nil
        // constructor. Keep the golden unchanged and prove that sole semantic
        // correction rather than repinning generated token output.
        assert_eq!(
            &actual[..4],
            &legacy[..4],
            "legacy token ranges are unchanged"
        );
        assert_eq!(
            (legacy[4], actual[4]),
            (1, 8),
            "only a type-to-binding correction"
        );
        let start = snapshot.line_index.offset(source, actual[0], actual[1]);
        let end = snapshot.line_index.offset(source, actual[2], actual[3]);
        assert_eq!(&source[start..end], "Nil");
        let token = snapshot
            .syntax
            .token_at_or_before(start)
            .must_be("Nil token");
        let occurrence = snapshot
            .syntax
            .reference_for_token(token)
            .must_be("grammar occurrence");
        assert_eq!(occurrence.role, analysis::ReferenceRole::Binding);
        assert_eq!(
            occurrence.reference.kind,
            analysis::ReferenceKind::Declaration
        );
        let local = occurrence.local_binding.must_be("native pattern binding");
        let binding = &snapshot.syntax.bindings()[local.0];
        assert_eq!(occurrence.reference.resolved, Some(binding.id));
        assert_eq!(snapshot.syntax.constructor_by_name(binding.name), None);
    }

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
    );
}

#[test]
fn medium_and_large_legacy_fixture_outputs_match_indexed_queries_exactly() {
    assert_legacy_fixture_outputs_match_indexed_queries_exactly(
        include_str!("../benches/fixtures/analyzer_medium.bend"),
        include_str!("fixtures/analysis_legacy_medium_outputs.json"),
        "worker_0199",
    );
    assert_legacy_fixture_outputs_match_indexed_queries_exactly(
        include_str!("../benches/fixtures/analyzer_large.bend"),
        include_str!("fixtures/analysis_legacy_large_outputs.json"),
        "worker_0599",
    );
}

#[test]
fn constructor_definitions_preserve_source_order_when_names_were_seen_earlier() {
    for prefix in ["", "def prior() -> FirstType: Second{}\n\n"] {
        let source = format!(
            "{prefix}type FirstType is Data:\n  First{{}}\n  Second{{}}\n\
             type OtherType is Data:\n  Third{{}}\n"
        );
        let snapshot = DocumentSnapshot::new(Revision(1), source.clone());
        for name in ["First", "Second", "Third"] {
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

#[test]
fn inlay_hints_order_nested_calls_and_limit_labels_to_declared_parameters() {
    let source = "def pair(first: U32, second: U32) -> U32:\n  first\n\
                  def unary(only: U32) -> U32:\n  only\n\
                  def main() -> U32:\n  _ = unary(pair(5, 6))\n  _ = pair(1)\n  _ = pair(2, 3, 4)\n  0\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let labels = |range| {
        bend2_lsp::analysis::inlay_hints(&snapshot, range)
            .into_iter()
            .map(|hint| {
                let (line, column) = snapshot.line_index.position(source, hint.position);
                (line, column, hint.label(source))
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(TextRange::new(0, source.len())),
        vec![
            (5, 12, "only: ".to_owned()),
            (5, 17, "first: ".to_owned()),
            (5, 20, "second: ".to_owned()),
            (6, 11, "first: ".to_owned()),
            (7, 11, "first: ".to_owned()),
            (7, 14, "second: ".to_owned()),
        ]
    );
    assert_eq!(
        labels(TextRange::new(
            snapshot.line_index.offset(source, 5, 17),
            snapshot.line_index.offset(source, 5, 20),
        )),
        vec![
            (5, 17, "first: ".to_owned()),
            (5, 20, "second: ".to_owned())
        ]
    );
}

#[test]
fn dense_index_lookups_distinguish_zero_ids_missing_entries_and_nested_calls() {
    use bend2_lsp::analysis::{SymbolId, TokenId};

    let snapshot = DocumentSnapshot::new(
        Revision(1),
        "def first(value: U32) -> U32:\n  value\n\
         def main() -> U32:\n  first(first(1))\n  unknown\n"
            .to_owned(),
    );
    let syntax = &snapshot.syntax;
    let first = snapshot.text.find("first").must_be("first declaration");
    let declaration = syntax
        .token_at_or_before(first)
        .must_be("declaration token");
    assert_eq!(syntax.symbol_for_token(declaration), Some(SymbolId(0)));
    assert_eq!(
        syntax
            .reference_for_token(declaration)
            .must_be("declaration reference")
            .reference
            .resolved,
        Some(SymbolId(0))
    );
    assert_eq!(
        syntax.name_text(
            &snapshot.text,
            syntax.name_for_token(TokenId(0)).must_be("first name")
        ),
        "def"
    );
    assert_eq!(syntax.symbol_for_token(TokenId(0)), None);
    assert!(syntax.is_keyword(syntax.name_for_token(TokenId(0)).must_be("keyword name")));
    assert_eq!(syntax.reference_for_token(TokenId(0)), None);
    let punctuation = syntax
        .token_at_or_before(first + "first".len())
        .must_be("opening delimiter");
    assert_eq!(syntax.name_for_token(punctuation), None);
    assert_eq!(syntax.reference_for_token(punctuation), None);
    assert_eq!(syntax.call_for_token(TokenId(0)), None);
    assert_eq!(syntax.call_by_open(TokenId(0)), None);
    for token in [TokenId(syntax.tokens().len()), TokenId(usize::MAX)] {
        assert_eq!(syntax.name_for_token(token), None);
        assert_eq!(syntax.symbol_for_token(token), None);
        assert!(syntax.reference_for_token(token).is_none());
        assert!(syntax.call_for_token(token).is_none());
        assert!(syntax.call_by_open(token).is_none());
    }

    let outer = snapshot.text.find("first(first").must_be("outer call");
    let inner = outer + "first(".len();
    let outer_token = syntax.token_at_or_before(outer).must_be("outer callee");
    let inner_token = syntax.token_at_or_before(inner).must_be("inner callee");
    let outer_call = syntax
        .call_for_token(outer_token)
        .must_be("first call index");
    let inner_call = syntax.call_for_token(inner_token).must_be("nested call");
    assert_eq!(outer_call.callee, Some(SymbolId(0)));
    assert_eq!(inner_call.callee, Some(SymbolId(0)));
    assert_eq!(syntax.call_by_open(outer_call.open), Some(outer_call));
    assert_eq!(syntax.call_by_open(inner_call.open), Some(inner_call));
    assert_eq!(
        syntax
            .call_at(inner + "first(".len())
            .map(|(call, argument)| (call.callee_token, argument)),
        Some((inner_token, 0))
    );

    let unknown = snapshot.text.find("unknown").must_be("unresolved name");
    let unknown_token = syntax
        .token_at_or_before(unknown)
        .must_be("unresolved token");
    assert_eq!(syntax.symbol_for_token(unknown_token), None);
    assert_eq!(
        syntax
            .reference_for_token(unknown_token)
            .must_be("unresolved reference")
            .reference
            .resolved,
        None
    );
    assert_eq!(syntax.call_for_token(unknown_token), None);
}

#[test]
fn module_completion_preserves_subsequence_order_case_and_repeated_characters() {
    let snapshot = DocumentSnapshot::new(
        Revision(1),
        concat!(
            "def aba() -> U32:\n  0\n",
            "def aab() -> U32:\n  0\n",
            "def AAA() -> U32:\n  0\n",
            "def zaa() -> U32:\n  0\n",
        )
        .into(),
    );
    let cases: &[(&str, &[&str])] = &[
        ("aa", &["aba", "aab", "AAA", "zaa"]),
        ("aBa", &["aba"]),
        ("aaaa", &[]),
        ("λA", &[]),
        ("", &["aba", "aab", "AAA", "zaa"]),
    ];
    for &(query, expected) in cases {
        let items = bend2_lsp::analysis::module_completion_items(&snapshot, query);
        assert!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .eq(expected.iter().copied()),
            "query {query}: {items:?}"
        );
    }
}

#[test]
fn reference_lookup_preserves_first_declaration_shadowing_and_unresolved_names() {
    let source = concat!(
        "def target() -> U32:\n  missing(missing)\n",
        "def target() -> U32:\n  2\n",
        "def local(target: U32) -> U32:\n  target\n",
        "def caller() -> U32:\n  target()\n",
        "def late() -> U32:\n  unresolved(unresolved)\n",
    );
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let declaration = source.find("target").must_be("first declaration");
    let call = source.find("  target()").must_be("global call") + 2;
    assert_eq!(
        bend2_lsp::analysis::identifier_ranges(&snapshot, "target"),
        [
            TextRange::new(declaration, declaration + 6),
            TextRange::new(call, call + 6),
        ]
    );
    for name in ["missing", "unresolved"] {
        let expected = source
            .match_indices(name)
            .map(|(start, text)| TextRange::new(start, start + text.len()));
        assert!(
            bend2_lsp::analysis::identifier_ranges(&snapshot, name)
                .into_iter()
                .eq(expected)
        );
    }
    assert!(bend2_lsp::analysis::identifier_ranges(&snapshot, "absent").is_empty());
}

#[test]
fn parameter_hints_preserve_nested_order_range_boundaries_and_argument_arity() {
    let source = concat!(
        "def add(left: U32, right: U32) -> U32:\n  left\n",
        "def ident(value: U32) -> U32:\n  value\n",
        "def zero() -> U32:\n  0\n",
        "def main() -> U32:\n  _ = add(ident(1), 2)\n  _ = ident(3, 4)\n  _ = unknown(5)\n  zero()\n",
    );
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let nested = source.find("add(ident(1), 2)").must_be("nested call");
    let extra = source.find("ident(3, 4)").must_be("extra argument call");
    let expected = [
        (nested + 4, "left: "),
        (nested + 10, "value: "),
        (nested + 14, "right: "),
        (extra + 6, "value: "),
    ];
    let hints = bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(0, source.len()));
    assert!(
        hints
            .iter()
            .map(|hint| (hint.position, hint.label(source)))
            .eq(expected.map(|(position, label)| (position, label.to_owned())))
    );
    let boundary =
        bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(nested + 10, nested + 10));
    assert!(
        boundary
            .iter()
            .map(|hint| (hint.position, hint.label(source)))
            .eq([(nested + 10, "value: ".to_owned())])
    );
    for range in [
        TextRange::new(nested + 11, nested + 13),
        TextRange::new(nested + 14, nested + 4),
    ] {
        assert!(bend2_lsp::analysis::inlay_hints(&snapshot, range).is_empty());
    }
}

#[test]
fn parameter_hints_preserve_labels_across_nested_multi_argument_interleavings() {
    let source = concat!(
        "def fan(first: U32, second: U32, third: U32) -> U32:\n  first\n",
        "def ident(value: U32) -> U32:\n  value\n",
        "def main() -> U32:\n  fan(ident(1), fan(ident(2), ident(3), ident(4)), ident(5))\n",
    );
    let calls = [1, 2, 3, 4, 5].map(|value| {
        source
            .find(&format!("ident({value})"))
            .must_be("nested identity call")
    });
    let inner = source.find("fan(ident(2)").must_be("nested fan call");
    let expected = [
        (calls[0], "first: "),
        (calls[0] + 6, "value: "),
        (inner, "second: "),
        (calls[1], "first: "),
        (calls[1] + 6, "value: "),
        (calls[2], "second: "),
        (calls[2] + 6, "value: "),
        (calls[3], "third: "),
        (calls[3] + 6, "value: "),
        (calls[4], "third: "),
        (calls[4] + 6, "value: "),
    ];
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let hints = bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(0, source.len()));
    assert!(
        hints
            .iter()
            .map(|hint| (hint.position, hint.label(source)))
            .eq(expected.map(|(position, label)| (position, label.to_owned())))
    );
    let nested = bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(inner, calls[3] + 6));
    assert!(
        nested
            .iter()
            .map(|hint| (hint.position, hint.label(source)))
            .eq(expected[2..9]
                .iter()
                .map(|(position, label)| (*position, (*label).to_owned())))
    );
}

#[test]
fn parameter_hints_distinguish_unresolved_and_zero_arity_calls_from_newly_resolved_calls() {
    let source = concat!(
        "import ./other.bend as library\n",
        "def nop() -> U32:\n  0\n",
        "def main() -> U32:\n  _ = nop()\n  _ = absent(1)\n  library.echo(2)\n",
    );
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    assert!(
        bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(0, source.len())).is_empty()
    );
    let revised = format!(
        "{}\ndef resolved(argument: U32) -> U32:\n  argument\n",
        source.replace("absent(1)", "resolved(1)")
    );
    let argument = revised.find("resolved(1)").must_be("newly resolved call") + 9;
    let snapshot = DocumentSnapshot::new(Revision(2), revised);
    let hints = bend2_lsp::analysis::inlay_hints(&snapshot, TextRange::new(0, snapshot.text.len()));
    assert!(
        hints
            .iter()
            .map(|hint| (hint.position, hint.label(&snapshot.text)))
            .eq([(argument, "argument: ".to_owned())])
    );
}

#[test]
fn module_completion_preserves_ascii_case_underscores_repeated_letters_and_unicode() {
    let source = concat!(
        "def Foo_Bar() -> U32:\n  0\n",
        "def foo__foo() -> U32:\n  0\n",
        "def Ff() -> U32:\n  0\n",
        "def foo() -> U32:\n  0\n",
    );
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let cases: &[(&str, &[&str])] = &[
        ("F_F", &["foo__foo"]),
        ("FF", &["foo__foo", "Ff"]),
        ("πF", &[]),
        ("FOO", &["Foo_Bar", "foo__foo", "foo"]),
        ("_F", &["foo__foo"]),
        ("fff", &[]),
    ];
    for &(query, expected) in cases {
        let mut labels = bend2_lsp::analysis::module_completion_items(&snapshot, query)
            .into_iter()
            .map(|completion| completion.label)
            .collect::<Vec<_>>();
        labels.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(labels, expected, "query {query:?}");
    }
    assert_eq!(
        bend2_lsp::analysis::completion_match("πfoo", "πF"),
        Some((2, 0, 5))
    );
}

#[test]
fn duplicate_constructor_declarations_have_no_arbitrary_standalone_target() {
    let source = "type A is Data:\n  C{}\ntype B is Data:\n  C{}\ndef use() -> A: C{}\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    let name = snapshot
        .syntax
        .name_id(source, "C")
        .must_be("constructor name");
    assert_eq!(snapshot.syntax.constructor_by_name(name), None);
    assert_eq!(bend2_lsp::analysis::declaration_range(&snapshot, "C"), None);
    assert_eq!(
        snapshot
            .syntax
            .declaration_sites()
            .iter()
            .filter(|site| site.name == name)
            .count(),
        2
    );
    for parent_name in ["A", "B"] {
        let parent = snapshot
            .syntax
            .name_id(source, parent_name)
            .and_then(|name| snapshot.syntax.symbol_by_name(name))
            .must_be("parent type");
        let constructor = snapshot
            .syntax
            .constructors(parent.id)
            .first()
            .must_be("parent's exact constructor");
        assert_eq!(
            snapshot.syntax.constructor_by_symbol(constructor.symbol),
            Some(constructor)
        );
        assert_eq!(constructor.parent, parent.id);
        assert_eq!(snapshot.syntax.constructor_by_symbol(parent.id), None);
    }
    assert_eq!(
        snapshot
            .syntax
            .constructor_by_symbol(bend2_lsp::analysis::SymbolId(usize::MAX)),
        None
    );
    let use_offset = source.rfind("C{}").must_be("constructor use");
    let occurrence = snapshot
        .syntax
        .reference_for_token(
            snapshot
                .syntax
                .token_at_or_before(use_offset)
                .must_be("constructor token"),
        )
        .must_be("constructor occurrence");
    assert_eq!(
        occurrence.role,
        bend2_lsp::analysis::ReferenceRole::Constructor
    );
    assert_eq!(occurrence.reference.resolved, None);
}

#[test]
fn workspace_symbol_queries_keep_constructor_facts_nested() {
    use bend2_lsp::analysis::{
        DeclarationKind, SymbolKind, WorkspaceSymbolQuery, document_symbols,
    };

    let source = "type Shape is Data:\n  Circle{radius: U32}\ndef add(x: U32, y: U32) -> U32: U32.add(x, y)\ndef kelvin() -> U32: 273\n";
    let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
    for (query, expected) in [
        ("", vec!["Shape", "add", "kelvin"]),
        ("SH", vec!["Shape"]),
        ("ADD", vec!["add"]),
        ("Circle", vec![]),
    ] {
        let query = WorkspaceSymbolQuery::new(query);
        let names = query
            .symbols(&snapshot)
            .map(|symbol| symbol.name)
            .collect::<Vec<_>>();
        assert_eq!(names, expected);
    }
    let constructor = snapshot
        .syntax
        .symbols()
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Constructor)
        .must_be("constructor symbol fact");
    assert_eq!(
        snapshot.syntax.name_text(source, constructor.name),
        "Circle"
    );
    assert!(
        snapshot
            .syntax
            .declaration_sites()
            .iter()
            .any(|site| site.symbol == constructor.id && site.kind == DeclarationKind::Constructor)
    );
    let outline = document_symbols(&snapshot);
    assert_eq!(outline.len(), 3);
    assert_eq!(outline[0].name, "Shape");
    assert_eq!(outline[0].children.len(), 1);
    assert_eq!(outline[0].children[0].name, "Circle");
    assert_eq!(outline[0].children[0].kind, SymbolKind::Constructor);
}
