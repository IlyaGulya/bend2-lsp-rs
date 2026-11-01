mod support;

use bend2_lsp::analysis::{
    DeclarationKind, DocumentSnapshot, Revision, TextRange, declaration_documentation,
    declaration_hover_by_id, declaration_metadata, declaration_metadata_by_name,
    document_symbol_by_id, document_symbols, inlay_hints, signature_help,
    signature_help_for_symbol,
};
use support::Must;

fn snapshot(source: &str) -> DocumentSnapshot {
    DocumentSnapshot::new(Revision(7), source.to_owned())
}

fn slice(snapshot: &DocumentSnapshot, range: TextRange) -> &str {
    &snapshot.text[range.start..range.end]
}

#[test]
fn multiline_signature_parameters_and_body_are_exact_indexed_slices() {
    let source = concat!(
        "# Choose a dependent function.\n",
        "def choose(\n",
        "  ~A: Type,\n",
        "  value: A,\n",
        "  project: (A -> A),\n",
        ") ->\n",
        "  (A -> A):\n",
        "  project\n",
        "# Next declaration docs.\n",
        "def next() -> Type:\n",
        "  Type\n",
    );
    let snapshot = snapshot(source);
    let metadata = declaration_metadata_by_name(&snapshot, "choose").must_be("choose metadata");
    assert_eq!(metadata.kind, DeclarationKind::Def);
    assert_eq!(slice(&snapshot, metadata.name_range), "choose");
    assert_eq!(
        slice(&snapshot, metadata.signature_range),
        "def choose(\n  ~A: Type,\n  value: A,\n  project: (A -> A),\n) ->\n  (A -> A)"
    );
    assert_eq!(
        slice(&snapshot, metadata.full_range),
        "def choose(\n  ~A: Type,\n  value: A,\n  project: (A -> A),\n) ->\n  (A -> A):\n  project"
    );
    assert_eq!(
        slice(&snapshot, metadata.body_range.must_be("choose body")),
        "project"
    );
    let help =
        signature_help_for_symbol(&snapshot, metadata.symbol, 2).must_be("indexed signature");
    assert_eq!(help.label, slice(&snapshot, metadata.signature_range));
    assert_eq!(
        help.parameters,
        ["~A: Type", "value: A", "project: (A -> A)"]
    );
    assert_eq!(help.active_parameter, 2);
    assert_eq!(
        declaration_documentation(&snapshot, metadata.symbol),
        Some("# Choose a dependent function.")
    );
    let symbol =
        document_symbol_by_id(&snapshot, metadata.symbol).must_be("choose document symbol");
    assert_eq!(symbol.range, metadata.full_range);
    assert!(!slice(&snapshot, symbol.range).contains("Next declaration docs"));
    assert_eq!(document_symbols(&snapshot)[0], symbol);
}

#[test]
fn law_and_def_keep_distinct_written_sites_and_no_executable_law_body() {
    let source = "# Identity law.\nlaw identity:\n  for A: Type\n  for x: A\n  A\n\
                  # Identity implementation.\ndef identity(A, x):\n  x\n";
    let snapshot = snapshot(source);
    let sites: Vec<_> = snapshot
        .syntax
        .declaration_sites()
        .iter()
        .filter(|site| slice(&snapshot, site.name_range) == "identity")
        .collect();
    assert_eq!(sites.len(), 2);
    let law = sites[0];
    let implementation = sites[1];
    assert_ne!(law.symbol, implementation.symbol);
    assert_eq!(law.kind, DeclarationKind::Law);
    assert_eq!(law.body_range, None);
    assert_eq!(
        slice(&snapshot, law.signature_range),
        "law identity:\n  for A: Type\n  for x: A\n  A"
    );
    assert_eq!(law.signature_range, law.full_range);
    assert_eq!(implementation.kind, DeclarationKind::Def);
    assert_eq!(
        slice(&snapshot, implementation.signature_range),
        "def identity(A, x)"
    );
    assert_eq!(
        slice(
            &snapshot,
            implementation.body_range.must_be("implementation body")
        ),
        "x"
    );
    assert_eq!(declaration_metadata(&snapshot, law.symbol), Some(law));
    assert_eq!(
        declaration_documentation(&snapshot, implementation.symbol),
        Some("# Identity implementation.")
    );
    assert_eq!(
        signature_help_for_symbol(&snapshot, implementation.symbol, 1)
            .must_be("written implementation parameters")
            .parameters,
        ["A", "x"]
    );
}

#[test]
fn docs_preserve_unicode_crlf_offsets_and_hover_text() {
    let source = "# café 🦀\r\n# 第二行\r\ndef greet() -> Type:\r\n  Type\r\n";
    let snapshot = snapshot(source);
    let metadata = declaration_metadata_by_name(&snapshot, "greet").must_be("greet metadata");
    let docs = metadata.doc_range.must_be("attached documentation");
    assert_eq!(slice(&snapshot, docs), "# café 🦀\r\n# 第二行");
    assert_eq!(snapshot.line_index.position(source, docs.start), (0, 0));
    assert_eq!(snapshot.line_index.position(source, docs.end), (1, 5));
    assert_eq!(
        snapshot
            .line_index
            .position(source, metadata.name_range.start),
        (2, 4)
    );
    assert_eq!(
        snapshot
            .line_index
            .position(source, metadata.name_range.end),
        (2, 9)
    );
    assert_eq!(
        declaration_hover_by_id(&snapshot, metadata.symbol).must_be("greet hover"),
        "```bend\ndef greet() -> Type\n```\n\ncafé 🦀\n第二行"
    );
}

#[test]
fn doc_attachment_breaks_on_blank_code_or_different_indentation() {
    let source = "# Detached by blank.\n\ndef blank() -> Type:\n  Type\n\
                  # Detached by code.\nimport lib.bend\ndef code() -> Type:\n  Type\n\
                  \x20\x20# Wrong indentation.\ndef indentation() -> Type:\n  Type\n\
                  # Attached first.\n# Attached second.\ndef attached() -> Type:\n  Type # inline body comment\n\
                  # Following docs.\ndef following() -> Type:\n  Type\n";
    let snapshot = snapshot(source);
    for name in ["blank", "code", "indentation"] {
        let metadata =
            declaration_metadata_by_name(&snapshot, name).must_be("declaration metadata");
        assert_eq!(metadata.doc_range, None, "{name}");
    }
    let attached = declaration_metadata_by_name(&snapshot, "attached").must_be("attached metadata");
    assert_eq!(
        declaration_documentation(&snapshot, attached.symbol),
        Some("# Attached first.\n# Attached second.")
    );
    let body = slice(&snapshot, attached.body_range.must_be("attached body"));
    assert!(!body.contains("Following docs"));
    assert!(!slice(&snapshot, attached.full_range).contains("Following docs"));
    let following =
        declaration_metadata_by_name(&snapshot, "following").must_be("following metadata");
    assert_eq!(
        declaration_documentation(&snapshot, following.symbol),
        Some("# Following docs.")
    );
}

#[test]
fn incomplete_definition_does_not_absorb_recovered_next_declaration() {
    let source = "# Broken docs.\ndef broken(\n  value: Type,\n\
                  # Recovered docs.\ndef recovered() -> Type:\n  Type\n";
    let snapshot = snapshot(source);
    let broken = declaration_metadata_by_name(&snapshot, "broken").must_be("broken metadata");
    let recovered =
        declaration_metadata_by_name(&snapshot, "recovered").must_be("recovered metadata");
    assert!(broken.uncertain);
    assert!(broken.full_range.end <= recovered.doc_range.must_be("recovered docs").start);
    assert_eq!(
        slice(&snapshot, recovered.signature_range),
        "def recovered() -> Type"
    );
    assert_eq!(
        declaration_documentation(&snapshot, recovered.symbol),
        Some("# Recovered docs.")
    );
}

#[test]
fn empty_body_foreign_body_and_decorated_header_have_explicit_ranges() {
    let source = "def empty() -> Type:\n\
                  # Foreign docs.\n@unsafe\ndef foreign?() -> Type: import \"effect.js\"\n\
                  def after() -> Type:\n  Type\n";
    let snapshot = snapshot(source);
    let empty = declaration_metadata_by_name(&snapshot, "empty").must_be("empty definition");
    assert_eq!(empty.body_range, None);
    let foreign = declaration_metadata_by_name(&snapshot, "foreign").must_be("foreign definition");
    assert_eq!(
        slice(&snapshot, foreign.signature_range),
        "@unsafe\ndef foreign?() -> Type"
    );
    assert_eq!(
        slice(&snapshot, foreign.body_range.must_be("foreign effect body")),
        "import \"effect.js\""
    );
    assert_eq!(
        declaration_documentation(&snapshot, foreign.symbol),
        Some("# Foreign docs.")
    );
}

#[test]
fn dependent_nested_parameter_annotations_are_not_reparsed_by_signature_help() {
    let source = concat!(
        "def dependent(~A: Type, f: (@x: A -> A), pair: (A, A)) -> (A -> A):\n",
        "  f\n",
    );
    let snapshot = snapshot(source);
    let metadata =
        declaration_metadata_by_name(&snapshot, "dependent").must_be("dependent metadata");
    let declaration = snapshot
        .syntax
        .symbol_by_id(metadata.symbol)
        .must_be("dependent symbol");
    let parameters = snapshot.syntax.parameters(declaration);
    let full: Vec<_> = parameters
        .iter()
        .map(|parameter| slice(&snapshot, parameter.full_range))
        .collect();
    assert_eq!(full, ["~A: Type", "f: (@x: A -> A)", "pair: (A, A)"]);
    let annotations: Vec<_> = parameters
        .iter()
        .map(|parameter| {
            slice(
                &snapshot,
                parameter.annotation.must_be("parameter annotation"),
            )
        })
        .collect();
    assert_eq!(annotations, ["Type", "(@x: A -> A)", "(A, A)"]);
    let help = signature_help_for_symbol(&snapshot, metadata.symbol, usize::MAX)
        .must_be("dependent signature help");
    assert_eq!(help.parameters, full);
    assert_eq!(help.active_parameter, 2);
}

#[test]
fn type_and_constructor_metadata_keep_own_written_ranges() {
    let source = concat!(
        "# Box docs.\n",
        "type Box<A: Type> is Type:\n",
        "  # Constructor docs.\n",
        "  Boxed{value: A}\n",
        "# Next docs.\n",
        "def next() -> Type:\n",
        "  Type\n",
    );
    let snapshot = snapshot(source);
    let parent = declaration_metadata_by_name(&snapshot, "Box").must_be("type metadata");
    assert_eq!(parent.kind, DeclarationKind::Type);
    assert_eq!(
        slice(&snapshot, parent.signature_range),
        "type Box<A: Type> is Type"
    );
    assert!(!slice(&snapshot, parent.full_range).contains("Next docs"));
    let constructor = snapshot
        .syntax
        .declaration_sites()
        .iter()
        .find(|site| site.kind == DeclarationKind::Constructor)
        .must_be("constructor metadata");
    assert_eq!(slice(&snapshot, constructor.name_range), "Boxed");
    assert_eq!(
        slice(&snapshot, constructor.signature_range),
        "Boxed{value: A}"
    );
    assert_eq!(constructor.body_range, None);
    assert_eq!(
        declaration_documentation(&snapshot, constructor.symbol),
        Some("# Constructor docs.")
    );
}

#[test]
fn snapshot_content_hash_is_stable_over_original_utf8_bytes_not_revision() {
    assert_eq!(snapshot("").content_hash(), 0xcbf2_9ce4_8422_2325);
    assert_eq!(snapshot("a").content_hash(), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(snapshot("hello").content_hash(), 0xa430_d846_80aa_bd0b);
    let source = "# café 🦀\r\ndef greet() -> Type:\r\n  Type\r\n";
    let first = snapshot(source);
    let second = DocumentSnapshot::new(Revision(99), source.to_owned());
    assert_eq!(first.content_hash(), second.content_hash());
    assert_ne!(
        first.content_hash(),
        snapshot(&source.replace("\r\n", "\n")).content_hash()
    );
    let indexed = DocumentSnapshot::with_line_index(
        Revision(100),
        source.to_owned(),
        bend2_lsp::analysis::LineIndex::new(source),
    );
    assert_eq!(first.content_hash(), indexed.content_hash());
}

#[test]
fn standalone_signature_help_uses_cold_call_targets_and_exact_parameters() {
    let source = concat!(
        "def transform(f: (Type -> Type), value: Type) -> (Type -> Type):\n",
        "  f\n",
        "def caller() -> Type:\n",
        "  transform(Type, ",
    );
    let snapshot = snapshot(source);
    let help = signature_help(&snapshot, source.len()).must_be("standalone indexed call target");
    assert_eq!(help.parameters, ["f: (Type -> Type)", "value: Type"]);
    assert_eq!(help.active_parameter, 1);
    assert_eq!(
        help.label,
        "def transform(f: (Type -> Type), value: Type) -> (Type -> Type)"
    );
}

#[test]
fn inlay_hints_interleave_nested_calls_in_source_order_and_preserve_range_bounds() {
    let source = concat!(
        "def inner(item: Type) -> Type:\n",
        "  item\n",
        "def outer(first: Type, second: Type) -> Type:\n",
        "  first\n",
        "def caller() -> Type:\n",
        "  outer(inner(Type), Type)\n",
    );
    let snapshot = snapshot(source);
    let start = source.find("outer(inner(Type)").must_be("nested call");
    let hints = inlay_hints(&snapshot, TextRange::new(0, source.len()));
    let positions_and_names: Vec<_> = hints
        .iter()
        .map(|hint| (hint.position, slice(&snapshot, hint.parameter_name)))
        .collect();
    assert_eq!(
        positions_and_names,
        [
            (start + 6, "first"),
            (start + 12, "item"),
            (start + 19, "second"),
        ]
    );
    assert_eq!(
        inlay_hints(&snapshot, TextRange::new(start + 12, start + 12)),
        hints[1..2]
    );
    assert!(inlay_hints(&snapshot, TextRange::new(0, start)).is_empty());
}
