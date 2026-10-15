mod support;

use bend2_lsp::analysis::{CallSite, DocumentSnapshot, ReferenceKind, Revision, TextRange};
use support::Must;

fn snapshot(source: &str) -> DocumentSnapshot {
    DocumentSnapshot::new(Revision(1), source.to_owned())
}

fn call_full_name<'a>(document: &'a DocumentSnapshot, call: &CallSite) -> &'a str {
    let name = document
        .syntax
        .reference_for_token(call.callee_token)
        .must_be("callee occurrence")
        .full_name;
    document.syntax.name_text(&document.text, name)
}

#[test]
fn lambda_call_resolves_to_its_binder_and_stops_at_argument_boundary() {
    let source = "def target(x: U32) -> U32: x\ndef main(target: U32) -> U32: apply(target => target(1), target)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let main = &syntax.symbols()[1];
    let parameter = &syntax.parameters(main)[0];
    let lambda_offset = source.find("target =>").must_be("lambda binder");
    let lambda_token = syntax
        .token_at_or_before(lambda_offset)
        .must_be("lambda token");
    let lambda = syntax
        .symbol_for_token(lambda_token)
        .must_be("lambda binding");
    assert_ne!(lambda, parameter.id);
    let call_offset = source.find("target(1)").must_be("lambda call");
    let call_token = syntax.token_at_or_before(call_offset).must_be("call token");
    assert_eq!(syntax.symbol_for_token(call_token), Some(lambda));
    let final_offset = source.rfind("target)").must_be("outer use");
    let final_token = syntax
        .token_at_or_before(final_offset)
        .must_be("outer token");
    assert_eq!(syntax.symbol_for_token(final_token), Some(parameter.id));
    assert_eq!(syntax.references(lambda).count(), 2);
}

#[test]
fn parallel_right_hand_sides_see_outer_names_and_continuation_sees_new_binders() {
    let source = "def f(x: U32, y: U32) -> U32:\n  x y = y x\n  U32.add(x, y)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let parameters = syntax.parameters(&syntax.symbols()[0]);
    let rhs = source.find("= y x").must_be("parallel rhs") + 2;
    let y = syntax.token_at_or_before(rhs).must_be("rhs y");
    let x = syntax.token_at_or_before(rhs + 2).must_be("rhs x");
    assert_eq!(syntax.symbol_for_token(y), Some(parameters[1].id));
    assert_eq!(syntax.symbol_for_token(x), Some(parameters[0].id));
    let call = source.find("add(x, y)").must_be("continuation");
    let inner_x = syntax.token_at_or_before(call + 4).must_be("inner x");
    let inner_y = syntax.token_at_or_before(call + 7).must_be("inner y");
    assert_ne!(syntax.symbol_for_token(inner_x), Some(parameters[0].id));
    assert_ne!(syntax.symbol_for_token(inner_y), Some(parameters[1].id));
    assert!(syntax.symbol_for_token(inner_x).is_some());
    assert!(syntax.symbol_for_token(inner_y).is_some());
}

#[test]
fn dotted_atoms_have_one_occurrence_and_are_not_prefix_shadowed() {
    let source = "def f(D: U32) -> U32: D.target!(D)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let start = source.find("D.target").must_be("dotted call");
    let prefix = syntax.token_at_or_before(start).must_be("prefix");
    let terminal = syntax.token_at_or_before(start + 2).must_be("terminal");
    let reference = syntax
        .reference_for_token(prefix)
        .must_be("atom occurrence");
    assert_eq!(syntax.reference_for_token(terminal), Some(reference));
    assert_eq!(reference.full_range, TextRange::new(start, start + 8));
    assert_eq!(reference.range, TextRange::new(start + 2, start + 8));
    assert_eq!(syntax.name_text(source, reference.full_name), "D.target");
    assert_eq!(reference.kind, ReferenceKind::Call);
    assert_eq!(reference.resolved, None);
    assert_eq!(syntax.calls().len(), 1);
    assert_eq!(
        syntax.calls()[0].call_range,
        TextRange::new(start, start + 12)
    );
}

#[test]
fn dependent_annotations_do_not_see_their_own_binder() {
    let source = "def f(A: Type, x: A, y: x) -> A: x\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let parameters = syntax.parameters(&syntax.symbols()[0]);
    let a = source.find("x: A").must_be("dependent annotation") + 3;
    let x = source.find("y: x").must_be("second annotation") + 3;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(a).must_be("A token")),
        Some(parameters[0].id)
    );
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(x).must_be("x token")),
        Some(parameters[1].id)
    );
    assert_eq!(
        syntax.binding_type_range(source, parameters[1].id),
        Some(TextRange::new(a, a + 1))
    );
}

#[test]
fn case_binders_are_isolated_between_siblings_and_nested_destructures() {
    let source = "def f(x: Maybe<U32>) -> U32:\n  match x:\n    case Some{v}:\n      Tuple{a, b} = Tuple{v, v}\n      a\n    case None{}:\n      v\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let first = source.find("Some{v}").must_be("pattern") + 5;
    let binder = syntax
        .symbol_for_token(syntax.token_at_or_before(first).must_be("binder token"))
        .must_be("case binding");
    assert_eq!(syntax.references(binder).count(), 3);
    let last = source.rfind('v').must_be("sibling read");
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(last).must_be("sibling token")),
        None
    );
}

#[test]
fn broken_header_recovers_before_the_next_declaration_with_unicode_trivia() {
    let source = "# λ 🦀\r\ndef broken(x: (U32\r\ndef next(y: U32) -> U32:\r\n  y\r\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert_eq!(syntax.symbols().len(), 2);
    assert!(syntax.declaration_sites()[0].uncertain);
    let next = &syntax.symbols()[1];
    assert_eq!(syntax.name_text(source, next.name), "next");
    let parameter = &syntax.parameters(next)[0];
    let read = source.rfind("  y").must_be("body read") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(read).must_be("body token")),
        Some(parameter.id)
    );
    assert_eq!(document.line_index.position(source, read), (3, 2));
}

fn compiler_witness(case: &str, file: &str) -> DocumentSnapshot {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/semantic/corpus.json"))
            .must_be("permanent compiler witness corpus");
    let cases = corpus["cases"].as_array().must_be("witness cases");
    let witness = cases
        .iter()
        .find(|entry| entry["id"] == case)
        .must_be("compiler witness");
    assert_eq!(witness["facet"], "valid");
    snapshot(witness["files"][file].as_str().must_be("original source"))
}

#[test]
fn compiler_proven_lambda_let_and_tuple_witnesses_never_reference_the_global_target() {
    for (case, file) in [
        ("original-lambda", "lambda.bend"),
        ("original-let_valid", "let_valid.bend"),
        ("original-pattern", "pattern.bend"),
    ] {
        let document = compiler_witness(case, file);
        let syntax = &document.syntax;
        let target = syntax
            .name_id(&document.text, "target")
            .and_then(|name| syntax.symbol_by_name(name))
            .must_be("global target");
        assert_eq!(syntax.references(target.id).count(), 1, "{case}");
        let call = syntax
            .calls()
            .iter()
            .find(|call| call_full_name(&document, call) == "target")
            .must_be("lexical local call");
        let local = call
            .callee
            .and_then(|id| syntax.binding_by_id(id))
            .must_be("local target binding");
        assert_eq!(syntax.references(local.id).count(), 2, "{case}");
        assert!(
            syntax
                .reference_for_token(call.callee_token)
                .must_be("local call occurrence")
                .local_binding
                .is_some()
        );
    }
}

#[test]
fn compiler_proven_offload_is_a_structural_call_with_exact_callee_and_arguments() {
    let document = compiler_witness("original-gpu", "gpu.bend");
    let syntax = &document.syntax;
    let call = &syntax.calls()[0];
    assert_eq!(call_full_name(&document, call), "target");
    assert_eq!(
        &document.text[call.call_range.start..call.call_range.end],
        "target!(1)"
    );
    assert_eq!(syntax.call_arguments(call).len(), 1);
    let argument = syntax.call_arguments(call)[0];
    assert_eq!(&document.text[argument.start..argument.end], "1");
    assert_eq!(
        syntax
            .reference_for_token(call.callee_token)
            .must_be("offload occurrence")
            .role,
        bend2_lsp::analysis::ReferenceRole::Offload
    );
}

#[test]
fn repeated_sequential_shadowing_has_indexed_lookup_and_one_visible_name() {
    let mut source = "def f(x: U32) -> U32:\n".to_owned();
    for _ in 0..2048 {
        source.push_str("  x = x\n");
    }
    source.push_str("  x\n");
    let document = snapshot(&source);
    let syntax = &document.syntax;
    let name = syntax.name_id(&source, "x").must_be("x name");
    let offset = source.rfind("  x").must_be("final use") + 2;
    let binding = syntax
        .binding_named_at(name, offset)
        .must_be("indexed visible binding");
    let reference = syntax
        .reference_for_token(syntax.token_at_or_before(offset).must_be("final token"))
        .must_be("final occurrence");
    assert_eq!(reference.resolved, Some(binding.id));
    assert_eq!(syntax.references(binding.id).count(), 2);
    let visible: Vec<_> = syntax.visible_bindings(offset).collect();
    assert_eq!(visible, [binding]);
    assert_eq!(syntax.bindings().len(), 2049);
    assert!(!syntax.declaration_sites()[0].uncertain);
}

#[test]
fn adversarial_depth_is_bounded_and_does_not_consume_the_next_definition() {
    let source = format!(
        "def deep() -> U32: {}1{}\ndef next(x: U32) -> U32: x\n",
        "(".repeat(2048),
        ")".repeat(2048),
    );
    let document = snapshot(&source);
    assert_eq!(document.syntax.symbols().len(), 2);
    assert!(document.syntax.declaration_sites()[0].uncertain);
    assert_eq!(
        document
            .syntax
            .name_text(&source, document.syntax.symbols()[1].name),
        "next"
    );
    assert!(!document.syntax.declaration_sites()[1].uncertain);
}

#[test]
fn recoverable_missing_telescope_keeps_definite_body_ownership_and_queries() {
    let source = "def target(x):\n  x\ndef main: U32\n  target(1)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert!(syntax.declaration_sites()[1].uncertain);
    assert!(syntax.declaration_sites()[1].linkable);
    let target = syntax.symbols()[0].id;
    let call = syntax.calls_to(target).next().must_be("recovered call");
    assert_eq!(call.caller, Some(syntax.symbols()[1].id));
    assert!(
        !syntax
            .reference_for_token(call.callee_token)
            .must_be("recovered use")
            .uncertain
    );
}

#[test]
fn implicit_array_write_rebinding_retains_both_same_span_origins() {
    let source = "def f(a: Array<U32>, i: U32, value: U32) -> Array<U32>:\n  a[i] <- value\n  a\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let start = source.find("a[i]").must_be("array write");
    let occurrences: Vec<_> = syntax
        .all_references()
        .iter()
        .filter(|reference| reference.full_range == TextRange::new(start, start + 1))
        .collect();
    assert_eq!(occurrences.len(), 2);
    let read = occurrences
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Read)
        .must_be("outer read");
    let declaration = occurrences
        .iter()
        .find(|reference| reference.kind == ReferenceKind::Declaration)
        .must_be("implicit declaration");
    assert_ne!(read.local_binding, declaration.local_binding);
    assert!(declaration.synthetic);
    assert!(!read.synthetic);
    let final_offset = source.rfind("  a").must_be("continuation") + 2;
    let final_use = syntax
        .reference_for_token(
            syntax
                .token_at_or_before(final_offset)
                .must_be("final token"),
        )
        .must_be("final use");
    assert_eq!(final_use.local_binding, declaration.local_binding);
}

#[test]
fn dependent_quantifier_domain_uses_outer_binding_and_codomain_uses_inner_binding() {
    let source = "def f(x: Type, g: (@x: x -> x)) -> Type: x\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let outer = &syntax.parameters(&syntax.symbols()[0])[0];
    let binder_offset = source.find("@x").must_be("quantifier binder") + 1;
    let inner = syntax
        .symbol_for_token(
            syntax
                .token_at_or_before(binder_offset)
                .must_be("binder token"),
        )
        .must_be("quantified binding");
    assert_ne!(inner, outer.id);
    let domain = source.find("x: x").must_be("domain") + 3;
    let codomain = source.find("-> x").must_be("codomain") + 3;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(domain).must_be("domain token")),
        Some(outer.id)
    );
    assert_eq!(
        syntax.symbol_for_token(
            syntax
                .token_at_or_before(codomain)
                .must_be("codomain token")
        ),
        Some(inner)
    );
    let body = source.rfind(": x").must_be("body") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(body).must_be("body token")),
        Some(outer.id)
    );
}

#[test]
fn do_typed_bindings_start_after_their_rhs_and_namespace_is_not_a_phantom_type() {
    let source = "def f(x: U32) -> U32:\n  do M<>:\n    x: U32 = x\n    return x\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let outer = &syntax.parameters(&syntax.symbols()[0])[0];
    let rhs = source.find("= x").must_be("do rhs") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(rhs).must_be("rhs token")),
        Some(outer.id)
    );
    let inner_offset = source.find("    x:").must_be("do binder") + 4;
    let inner = syntax
        .symbol_for_token(
            syntax
                .token_at_or_before(inner_offset)
                .must_be("binder token"),
        )
        .must_be("do local");
    assert_ne!(inner, outer.id);
    let continuation = source.find("return x").must_be("return") + 7;
    assert_eq!(
        syntax.symbol_for_token(
            syntax
                .token_at_or_before(continuation)
                .must_be("return token")
        ),
        Some(inner)
    );
    let namespace = source.find("M<>").must_be("namespace");
    assert_eq!(
        syntax.reference_for_token(
            syntax
                .token_at_or_before(namespace)
                .must_be("namespace token")
        ),
        None
    );
}

#[test]
fn law_where_and_existential_clauses_have_compiler_visibility_order() {
    let source = "law claim:\n  for x: T where P(x)\n  exs y: F(x)\n  Pair<x, y>\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let parameters = syntax.parameters(&syntax.symbols()[0]);
    assert_eq!(
        parameters.len(),
        1,
        "existential clauses are not callable parameters"
    );
    let x = parameters[0].id;
    for offset in [
        source.find("P(x)").must_be("where") + 2,
        source.find("F(x)").must_be("dependent clause") + 2,
        source.find("<x,").must_be("conclusion") + 1,
    ] {
        assert_eq!(
            syntax.symbol_for_token(syntax.token_at_or_before(offset).must_be("x use")),
            Some(x)
        );
    }
    let y_declaration = source.find("y: F").must_be("existential");
    let y = syntax
        .symbol_for_token(
            syntax
                .token_at_or_before(y_declaration)
                .must_be("existential token"),
        )
        .must_be("existential binder");
    let y_use = source.find(", y>").must_be("existential conclusion") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(y_use).must_be("y use")),
        Some(y)
    );
    assert_eq!(syntax.declaration_sites()[0].body_range, None);
}

#[test]
fn rewrite_binder_is_local_only_to_the_motive_not_the_tail() {
    let source = "def f(e: U32) -> U32: %e@eq : P(e); e\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let outer = &syntax.parameters(&syntax.symbols()[0])[0];
    let binder = source.find("%e@").must_be("rewrite") + 1;
    let inner = syntax
        .symbol_for_token(syntax.token_at_or_before(binder).must_be("rewrite token"))
        .must_be("rewrite binder");
    assert_ne!(inner, outer.id);
    let motive = source.find("P(e)").must_be("motive") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(motive).must_be("motive token")),
        Some(inner)
    );
    let tail = source.rfind("; e").must_be("tail") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(tail).must_be("tail token")),
        Some(outer.id)
    );
    assert_eq!(syntax.references(inner).count(), 2);
}

#[test]
fn dotted_binders_match_only_the_same_complete_semantic_atom() {
    let source = "def f(D: U32, D.target: U32) -> U32: D.target\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let parameters = syntax.parameters(&syntax.symbols()[0]);
    let use_offset = source.rfind("D.target").must_be("exact dotted use");
    let occurrence = syntax
        .reference_for_token(
            syntax
                .token_at_or_before(use_offset)
                .must_be("prefix token"),
        )
        .must_be("one atom");
    assert_eq!(occurrence.resolved, Some(parameters[1].id));
    assert_eq!(syntax.references(parameters[0].id).count(), 1);
    assert_eq!(syntax.references(parameters[1].id).count(), 2);
    assert_eq!(syntax.name_text(source, occurrence.full_name), "D.target");
}

#[test]
fn contextual_as_and_unsafe_words_remain_real_names() {
    let source = "def as(unsafe: Type) -> Type: unsafe\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    for name in ["as", "unsafe"] {
        assert!(!syntax.is_keyword(syntax.name_id(source, name).must_be("free name")));
    }
    let parameter = &syntax.parameters(&syntax.symbols()[0])[0];
    assert_eq!(syntax.references(parameter.id).count(), 2);
}

#[test]
fn contextual_keyword_coloring_distinguishes_loader_and_decorator_from_names() {
    use bend2_lsp::analysis::{SyntaxIndex, semantic_tokens};

    let source = "import ./dep.bend as as\n@unsafe\ndef as(unsafe: Type) -> Type: unsafe\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let import_keyword = source.find("as as").must_be("alias clause");
    let decorator = source.find("@unsafe").must_be("decorator") + 1;
    let colors = semantic_tokens(&document);
    for name in ["as", "unsafe"] {
        assert!(!SyntaxIndex::is_keyword_text(name));
        assert!(!syntax.is_keyword(syntax.name_id(source, name).must_be("contextual name")));
    }
    for token in syntax.tokens() {
        let text = &source[token.range.start..token.range.end];
        if !matches!(text, "as" | "unsafe") {
            continue;
        }
        let keyword = token.range.start == import_keyword || token.range.start == decorator;
        assert_eq!(
            token.flags.contextual_keyword(),
            keyword,
            "{:?}",
            token.range
        );
        let color = colors
            .iter()
            .find(|color| color.range == token.range)
            .must_be("contextual name color");
        assert_eq!(color.token_type == 15, keyword, "{:?}", token.range);
    }
    let parameter = &syntax.parameters(&syntax.symbols()[0])[0];
    assert_eq!(syntax.references(parameter.id).count(), 2);
    assert!(!syntax.declaration_sites()[0].uncertain);
}

#[test]
fn call_expressions_are_not_destructuring_patterns_and_do_not_fabricate_binders() {
    let source = "def f(x: U32) -> U32:\n  Some(item) = x\n  item\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert!(syntax.declaration_sites()[0].uncertain);
    assert_eq!(
        syntax.bindings().len(),
        1,
        "a call pattern never declares item"
    );
    let last = source.rfind("  item").must_be("recovered read") + 2;
    let reference = syntax
        .reference_for_token(syntax.token_at_or_before(last).must_be("item token"))
        .must_be("uncertain recovered occurrence");
    assert!(reference.uncertain);
    assert_eq!(reference.resolved, None);
}

#[test]
fn reserved_words_inside_full_dotted_names_never_split_declarations_or_uses() {
    let source = "def Foo.type(x: U32) -> U32: x\ndef Type.foo(x: U32) -> U32: x\ndef caller() -> U32: Foo.type(Type.foo(1))\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert_eq!(syntax.symbols().len(), 3);
    assert_eq!(syntax.declaration_sites().len(), 3);
    assert!(
        syntax
            .declaration_sites()
            .iter()
            .all(|site| !site.uncertain)
    );
    for (name, declaration) in [("Foo.type", 0), ("Type.foo", 1)] {
        let call = syntax
            .calls()
            .iter()
            .find(|call| call_full_name(&document, call) == name)
            .must_be("complete dotted call");
        assert_eq!(call.callee, Some(syntax.symbols()[declaration].id));
        let segments = syntax.reference_segments(
            syntax
                .reference_for_token(call.callee_token)
                .must_be("dotted use"),
        );
        assert_eq!(segments.len(), 2);
    }
    let recovered = snapshot("def broken(x: Foo.type\n\ndef next(y: U32) -> U32: y\n");
    assert_eq!(recovered.syntax.symbols().len(), 2);
    assert_eq!(
        recovered
            .syntax
            .name_text(&recovered.text, recovered.syntax.symbols()[1].name),
        "next"
    );
    assert!(recovered.syntax.declaration_sites()[0].uncertain);
}

#[test]
fn root_match_cases_may_start_in_the_first_compiler_column() {
    let source = "def f(x: U32) -> U32:\nmatch x:\ncase y: y\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let binder = source.find("case y").must_be("unindented case") + 5;
    let local = syntax
        .symbol_for_token(syntax.token_at_or_before(binder).must_be("case binder"))
        .must_be("case binding");
    assert!(syntax.binding_by_id(local).is_some());
    assert_eq!(syntax.references(local).count(), 2);
    let body = source.rfind(": y").must_be("arm body") + 2;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(body).must_be("body token")),
        Some(local)
    );
    assert!(!syntax.declaration_sites()[0].uncertain);
}

#[test]
fn binding_visibility_is_half_open_and_restores_outer_at_lambda_end() {
    let source = "def f(x: U32) -> U32: apply(x => x, x)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let outer = &syntax.parameters(&syntax.symbols()[0])[0];
    let declaration = source.find("x =>").must_be("lambda declaration");
    let inner_id = syntax
        .symbol_for_token(
            syntax
                .token_at_or_before(declaration)
                .must_be("lambda token"),
        )
        .must_be("inner binder");
    let inner = syntax.binding_by_id(inner_id).must_be("inner metadata");
    assert_eq!(
        syntax.binding_named_at(inner.name, inner.scope.start),
        Some(inner)
    );
    assert_eq!(
        syntax.binding_named_at(inner.name, inner.scope.end),
        Some(outer)
    );
    assert_eq!(
        syntax
            .visible_bindings(inner.scope.end)
            .filter(|binding| binding.name == inner.name)
            .collect::<Vec<_>>(),
        [outer]
    );
    assert_eq!(syntax.binding_named_at(outer.name, outer.scope.end), None);
    assert!(syntax.visible_bindings(outer.scope.end).next().is_none());
    assert_eq!(
        syntax.symbol_for_token(
            syntax
                .token_at_or_before(declaration)
                .must_be("binder site")
        ),
        Some(inner.id)
    );
}

#[test]
fn rewrite_implicit_proof_has_real_percent_origin_and_no_fake_declaration_use() {
    let source = "def f(e: U32) -> U32: %proof@e : P(_, proof); e\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let use_offset = source.find("P(_").must_be("implicit proof use") + 2;
    let reference = syntax
        .reference_for_token(
            syntax
                .token_at_or_before(use_offset)
                .must_be("underscore token"),
        )
        .must_be("proof read");
    let target = reference
        .resolved
        .and_then(|id| syntax.binding_by_id(id))
        .must_be("implicit local identity");
    assert!(target.synthetic);
    assert!(!target.anonymous);
    let origin = source.find('%').must_be("rewrite origin");
    assert_eq!(target.name_range, TextRange::new(origin, origin + 1));
    assert_eq!(
        syntax.references(target.id).count(),
        1,
        "no fabricated declaration occurrence"
    );
    assert_eq!(reference.kind, ReferenceKind::Read);
    assert!(!reference.synthetic);
    assert_eq!(syntax.binding_named_at(target.name, target.scope.end), None);
}

#[test]
fn anonymous_parameters_preserve_signature_positions_without_fake_local_names() {
    let source = "def f(_: U32, x: U32) -> U32: x\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let parameters = syntax.parameters(&syntax.symbols()[0]);
    assert_eq!(parameters.len(), 2);
    assert!(parameters[0].anonymous);
    assert!(!parameters[1].anonymous);
    assert_eq!(syntax.references(parameters[0].id).count(), 0);
    let placeholder = source.find('_').must_be("placeholder token");
    assert_eq!(
        syntax.reference_for_token(
            syntax
                .token_at_or_before(placeholder)
                .must_be("placeholder")
        ),
        None
    );
    let body = source.rfind(": x").must_be("body") + 2;
    assert_eq!(syntax.binding_named_at(parameters[0].name, body), None);
    assert_eq!(
        syntax.visible_bindings(body).collect::<Vec<_>>(),
        [&parameters[1]]
    );
}

#[test]
fn bare_known_constructors_are_not_variable_patterns_but_lambda_names_can_shadow_them() {
    let source = "type Choice is Data:\n  None{}\ndef f(x: Choice) -> Choice:\n  match x:\n    case None: None\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let pattern = source.find("case None").must_be("bare constructor pattern") + 5;
    let occurrence = syntax
        .reference_for_token(syntax.token_at_or_before(pattern).must_be("pattern token"))
        .must_be("invalid pattern occurrence");
    assert!(occurrence.uncertain);
    assert_eq!(occurrence.local_binding, None);
    assert_eq!(occurrence.resolved, None);
    assert_eq!(
        syntax.bindings().len(),
        1,
        "only the function parameter is a binding"
    );

    let lambda_source =
        "type Choice is Data:\n  None{}\ndef f() -> (Choice -> Choice): None => None\n";
    let lambda = snapshot(lambda_source);
    let name = lambda_source.find("None =>").must_be("lambda binder");
    let identity = lambda
        .syntax
        .symbol_for_token(
            lambda
                .syntax
                .token_at_or_before(name)
                .must_be("lambda token"),
        )
        .must_be("lambda binding may use constructor spelling");
    assert!(lambda.syntax.binding_by_id(identity).is_some());
    assert_eq!(lambda.syntax.references(identity).count(), 2);
}

#[test]
fn dotted_local_rename_ranges_cover_the_complete_binder_at_every_use() {
    let source = "def f(Foo.target: U32) -> U32: Foo.target\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let binding = &syntax.parameters(&syntax.symbols()[0])[0];
    let occurrences: Vec<_> = syntax.references(binding.id).collect();
    assert_eq!(occurrences.len(), 2);
    for occurrence in &occurrences {
        assert_eq!(
            &source[occurrence.range.start..occurrence.range.end],
            "Foo.target"
        );
        assert_eq!(occurrence.range, occurrence.full_range);
        assert_eq!(syntax.name_text(source, occurrence.name), "Foo.target");
    }
    let mut ranges: Vec<_> = occurrences
        .iter()
        .map(|occurrence| occurrence.range)
        .collect();
    ranges.sort_unstable_by_key(|range| std::cmp::Reverse(range.start));
    let mut renamed = source.to_owned();
    for range in ranges {
        renamed.replace_range(range.start..range.end, "other");
    }
    assert_eq!(renamed, "def f(other: U32) -> U32: other\n");
    let updated = snapshot(&renamed);
    let updated_binding = &updated.syntax.parameters(&updated.syntax.symbols()[0])[0];
    assert_eq!(updated.syntax.references(updated_binding.id).count(), 2);
}

#[test]
fn dotted_local_rename_capture_is_visible_in_the_index_at_the_complete_use() {
    let source = "def f(Foo.target: U32) -> (U32 -> U32): other => Foo.target\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let binding = &syntax.parameters(&syntax.symbols()[0])[0];
    let use_offset = source.rfind("Foo.target").must_be("dotted local use");
    let use_reference = syntax
        .reference_for_token(syntax.token_at_or_before(use_offset).must_be("use token"))
        .must_be("local occurrence");
    assert_eq!(use_reference.resolved, Some(binding.id));
    assert_eq!(use_reference.range, use_reference.full_range);
    let replacement = syntax.name_id(source, "other").must_be("capture name");
    let capture = syntax
        .binding_named_at(replacement, use_offset)
        .must_be("capturing lambda binding");
    assert_ne!(capture.id, binding.id);
    let lambda_offset = source.find("other =>").must_be("lambda declaration");
    assert_eq!(capture.name_range.start, lambda_offset);
    assert!(capture.scope.contains(use_offset));

    let mut ranges: Vec<_> = syntax
        .references(binding.id)
        .map(|reference| reference.range)
        .collect();
    ranges.sort_unstable_by_key(|range| std::cmp::Reverse(range.start));
    let mut captured = source.to_owned();
    for range in ranges {
        captured.replace_range(range.start..range.end, "other");
    }
    let updated = snapshot(&captured);
    let updated_use = captured.rfind("other").must_be("captured use");
    let resolved = updated
        .syntax
        .symbol_for_token(
            updated
                .syntax
                .token_at_or_before(updated_use)
                .must_be("captured token"),
        )
        .must_be("capturing binding identity");
    assert_ne!(
        resolved,
        updated.syntax.parameters(&updated.syntax.symbols()[0])[0].id
    );
}

#[test]
fn constructor_role_restores_terminal_edit_range_after_ignoring_an_exact_local_name() {
    let source = "type Foo is Data:\n  Foo.target{}\ndef f(Foo.target: U32) -> Foo: Foo.target{}\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let use_offset = source.rfind("Foo.target").must_be("constructor use");
    let reference = syntax
        .reference_for_token(
            syntax
                .token_at_or_before(use_offset)
                .must_be("constructor prefix"),
        )
        .must_be("constructor occurrence");
    assert_eq!(
        reference.role,
        bend2_lsp::analysis::ReferenceRole::Constructor
    );
    assert_eq!(reference.local_binding, None);
    assert_eq!(
        &source[reference.range.start..reference.range.end],
        "target"
    );
    assert_eq!(syntax.name_text(source, reference.name), "target");
    assert_eq!(
        &source[reference.full_range.start..reference.full_range.end],
        "Foo.target"
    );
}

#[test]
fn unknown_uppercase_pattern_names_are_local_binders_not_types() {
    // The same no-Base witness is retained in the pinned compiler corpus:
    // there is a constructor Mk, but no declared constructor named Nil.
    let source = "type Value is Data:\n  Mk{}\ndef f(x: Value) -> Value:\n  match x:\n    case Nil:\n      Nil\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let declaration = source.find("case Nil").must_be("uppercase pattern") + 5;
    let token = syntax
        .token_at_or_before(declaration)
        .must_be("pattern token");
    let identity = syntax
        .symbol_for_token(token)
        .must_be("uppercase binder identity");
    let binding = syntax
        .binding_by_id(identity)
        .must_be("local pattern binding");
    assert_eq!(syntax.name_text(source, binding.name), "Nil");
    assert_eq!(syntax.references(identity).count(), 2);
    let body = source.rfind("      Nil").must_be("uppercase use") + 6;
    assert_eq!(
        syntax.symbol_for_token(syntax.token_at_or_before(body).must_be("use token")),
        Some(identity)
    );
    let tokens: Vec<_> = bend2_lsp::analysis::semantic_tokens(&document)
        .into_iter()
        .filter(|token| &source[token.range.start..token.range.end] == "Nil")
        .collect();
    assert_eq!(tokens.len(), 2);
    assert!(tokens.iter().all(|token| token.token_type == 8));
    assert!(
        !syntax
            .declaration_sites()
            .last()
            .must_be("function site")
            .uncertain
    );
}

#[test]
fn pinned_base_fused_erased_type_telescopes_keep_exact_parameters_and_local_uses() {
    let source = include_str!("fixtures/semantic/compiler/base.bend");
    let document = snapshot(source);
    let syntax = &document.syntax;
    for (name, parameter, full_parameter) in [
        ("Word.Con", "p", "-p: Nat"),
        ("Array", "T", "-T: Type"),
        ("IO.OP", "R", "-R: Type"),
        ("App", "S", "-S: Type"),
    ] {
        let declaration = syntax
            .name_id(source, name)
            .and_then(|name| syntax.symbol_by_name(name))
            .must_be("pinned Base type");
        let metadata = syntax
            .declaration_site(declaration.id)
            .must_be("type metadata");
        assert!(metadata.linkable, "{name}");
        assert!(!metadata.uncertain, "{name}");
        let parameters = syntax.parameters(declaration);
        assert_eq!(parameters.len(), 1, "{name}");
        assert_eq!(syntax.name_text(source, parameters[0].name), parameter);
        let range = parameters[0].full_range;
        assert_eq!(&source[range.start..range.end], full_parameter);
        let opening = source[metadata.name_range.end..]
            .find("<-")
            .must_be("fused telescope opening")
            + metadata.name_range.end;
        let token = syntax
            .token_at_or_before(opening)
            .must_be("original operator token");
        assert_eq!(
            syntax.token_text(source, token),
            Some("<-"),
            "lexer still preserves write operator"
        );
        assert!(
            syntax
                .references(parameters[0].id)
                .any(|reference| reference.kind == ReferenceKind::Read),
            "constructor annotations retain the type's lexical {parameter} binding"
        );
    }
}

#[test]
fn empty_type_telescopes_accept_fused_and_separated_angle_tokens() {
    let source = "type LeftEmpty<> is Data:\ntype RightEmpty< > is Data:\n";
    let document = snapshot(source);
    assert_eq!(document.syntax.symbols().len(), 2);
    for declaration in document.syntax.symbols() {
        let metadata = document
            .syntax
            .declaration_site(declaration.id)
            .must_be("empty type metadata");
        assert!(metadata.linkable);
        assert!(!metadata.uncertain);
        assert!(document.syntax.parameters(declaration).is_empty());
    }
}

#[test]
fn calls_are_in_source_preorder_with_finalized_nested_argument_spans() {
    let source = "def main() -> U32: outer(first(1), second(inner(2)), third(3))\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let names: Vec<_> = syntax
        .calls()
        .iter()
        .map(|call| call_full_name(&document, call))
        .collect();
    assert_eq!(names, ["outer", "first", "second", "inner", "third"]);
    assert!(
        syntax
            .calls()
            .windows(2)
            .all(|pair| pair[0].callee_range.start < pair[1].callee_range.start)
    );
    let caller = syntax.symbols()[0].id;
    assert_eq!(
        syntax
            .calls_from(caller)
            .map(|call| call_full_name(&document, call))
            .collect::<Vec<_>>(),
        names
    );
    let outer = &syntax.calls()[0];
    assert_eq!(
        syntax
            .call_arguments(outer)
            .iter()
            .map(|range| &source[range.start..range.end])
            .collect::<Vec<_>>(),
        ["first(1)", "second(inner(2))", "third(3)"]
    );
    let second = &syntax.calls()[2];
    assert_eq!(
        syntax
            .call_arguments(second)
            .iter()
            .map(|range| &source[range.start..range.end])
            .collect::<Vec<_>>(),
        ["inner(2)"]
    );
    for call in syntax.calls() {
        assert_eq!(syntax.call_by_open(call.open), Some(call));
        assert_eq!(syntax.call_for_token(call.callee_token), Some(call));
        assert!(call.argument_range.start <= call.argument_range.end);
        assert!(call.call_range.start < call.call_range.end);
    }
}

#[test]
fn incomplete_nested_calls_finalize_preorder_slots_without_absorbing_the_next_definition() {
    let source = "def first() -> U32: outer(inner(1), pending(\ndef next() -> U32: 0\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert_eq!(
        syntax
            .calls()
            .iter()
            .map(|call| call_full_name(&document, call))
            .collect::<Vec<_>>(),
        ["outer", "inner", "pending"]
    );
    let next = syntax.declaration_sites()[1].full_range.start;
    for call in syntax.calls() {
        assert!(call.call_range.end <= next);
        assert!(call.argument_range.start <= call.argument_range.end);
        assert_eq!(syntax.call_by_open(call.open), Some(call));
    }
    assert!(!syntax.declaration_sites()[1].uncertain);
}

#[test]
fn source_preorder_call_slots_preserve_retroactive_type_argument_phase() {
    let source = "def f() -> Type:\n  T = F<maker(inner(1))>\n  run(T)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    assert_eq!(
        syntax
            .calls()
            .iter()
            .map(|call| call_full_name(&document, call))
            .collect::<Vec<_>>(),
        ["maker", "inner", "run"]
    );
    assert_eq!(syntax.calls()[0].caller, None);
    assert_eq!(syntax.calls()[1].caller, None);
    assert_eq!(syntax.calls()[2].caller, Some(syntax.symbols()[0].id));
    assert_eq!(
        syntax
            .calls_from(syntax.symbols()[0].id)
            .map(|call| call_full_name(&document, call))
            .collect::<Vec<_>>(),
        ["run"]
    );
    for call in &syntax.calls()[..2] {
        assert!(
            syntax
                .reference_for_token(call.callee_token)
                .must_be("type call occurrence")
                .in_type
        );
    }
}

#[test]
fn cached_parser_columns_keep_utf16_layout_after_non_ascii_literals() {
    let source = concat!(
        "def f(x: U32) -> U32:\n",
        "  text = \"🦀\"; (match x:\n",
        "                case y: y)\n",
    );
    let document = snapshot(source);
    let offset = source.find("case y").must_be("Unicode-layout case") + 5;
    let token = document
        .syntax
        .token_at_or_before(offset)
        .must_be("case binder");
    let binding = document
        .syntax
        .symbol_for_token(token)
        .must_be("case parsed at UTF-16 column");
    assert!(document.syntax.binding_by_id(binding).is_some());
    assert_eq!(document.syntax.references(binding).count(), 2);
    let match_offset = source.find("match x").must_be("match header");
    assert_eq!(document.line_index.position(source, match_offset), (1, 16));
    let indexed = DocumentSnapshot::with_line_index(
        Revision(1),
        source.to_owned(),
        bend2_lsp::analysis::LineIndex::new(source),
    );
    assert_eq!(document.syntax, indexed.syntax);
}

#[test]
fn law_existential_domains_do_not_consume_the_following_braced_type_as_a_constructor() {
    use bend2_lsp::analysis::{DeclarationKind, ReferenceRole};

    let source = "type Word is Data:\n  Word{}\nlaw witness:\n  for x: Word\n  exs y: Word\n  {x == y: Word}\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let word = syntax.name_id(source, "Word").must_be("Word name");
    let declaration = syntax
        .declaration_sites()
        .iter()
        .find(|site| site.kind == DeclarationKind::Type)
        .must_be("Word type")
        .symbol;
    let uses: Vec<_> = syntax
        .references_named(word)
        .filter(|reference| reference.kind != ReferenceKind::Declaration)
        .map(|reference| (reference.role, reference.resolved, reference.uncertain))
        .collect();
    assert_eq!(
        uses,
        vec![(ReferenceRole::Type, Some(declaration), false); 3]
    );
    let proof_start = source.find("{x == y").must_be("written equality");
    let locals: Vec<_> = syntax
        .all_references()
        .iter()
        .filter(|reference| reference.full_range.start >= proof_start)
        .filter_map(|reference| {
            let name = syntax.name_text(source, reference.full_name);
            matches!(name, "x" | "y").then_some((
                name,
                reference.role,
                reference.local_binding.is_some(),
                reference.uncertain,
            ))
        })
        .collect();
    assert_eq!(
        locals,
        vec![
            ("x", ReferenceRole::Type, true, false),
            ("y", ReferenceRole::Type, true, false),
        ]
    );
}

#[test]
fn offload_may_follow_a_newline_but_bang_and_open_parenthesis_are_adjacent() {
    use bend2_lsp::analysis::ReferenceRole;

    let source = "def target(x: U32) -> U32:\n  x\ndef main() -> U32:\n  target\n  !(1)\n";
    let document = snapshot(source);
    let call = document.syntax.calls().first().must_be("multiline offload");
    assert_eq!(call_full_name(&document, call), "target");
    assert_eq!(document.syntax.calls().len(), 1);
    assert_eq!(
        &source[call.call_range.start..call.call_range.end],
        "target\n  !(1)",
    );
    let reference = document
        .syntax
        .reference_for_token(call.callee_token)
        .must_be("offload origin");
    assert_eq!(reference.role, ReferenceRole::Offload);
    let target = document
        .syntax
        .name_id(source, "target")
        .and_then(|name| document.syntax.symbol_by_name(name))
        .must_be("target declaration");
    assert_eq!(call.callee, Some(target.id));
    for suffix in ["! (1)", "!\n  (1)"] {
        let broken = snapshot(&format!(
            "def target(x):\n  x\ndef main:\n  target{suffix}\n"
        ));
        assert!(
            broken
                .syntax
                .calls()
                .iter()
                .all(|call| call_full_name(&broken, call) != "target"),
            "a separated bang must not fabricate a compiler offload call: {suffix:?}",
        );
    }
}

#[test]
fn grammar_completion_contexts_use_native_telescope_types_and_incomplete_pattern_boundaries() {
    let declarations = "type Shape is Data:\n  Circle{}\ntype Other is Data:\n  Square{}\ntype Wrapped is Data:\n  Box{value: Shape}\n";
    for suffix in ["Box{Ci", "Box{\n      Ci"] {
        let source =
            format!("{declarations}def choose(input: Wrapped):\n  match input:\n    case {suffix}");
        let document = snapshot(&source);
        let context = document
            .syntax
            .case_pattern_type(source.len())
            .must_be("nested pattern");
        let expected = context
            .explicit_type
            .must_be("constructor telescope annotation");
        assert_eq!(&source[expected.start..expected.end], "Shape");
        let root = source.find("case ").must_be("case head") + "case ".len();
        let expected = document
            .syntax
            .case_pattern_type(root)
            .and_then(|context| context.explicit_type)
            .must_be("scrutinee annotation");
        assert_eq!(&source[expected.start..expected.end], "Wrapped");
    }
    let source = format!(
        "{declarations}def choose(input: Wrapped):\n  match input:\n    case Bo\n      Bo\n"
    );
    let document = snapshot(&source);
    let header = source.find("case Bo").must_be("incomplete head") + "case Bo".len();
    let body = source.rfind("Bo").must_be("expression body") + 2;
    assert!(document.syntax.case_pattern_type(header).is_some());
    assert!(document.syntax.case_pattern_type(body).is_none());
}

#[test]
fn completion_eof_visibility_does_not_widen_native_binding_scopes() {
    let source =
        "def inspect(updated_outer: U32):\n  match updated_outer:\n    case Pair{latest}:\n      ";
    let document = snapshot(source);
    let mut names: Vec<_> = document
        .syntax
        .bindings_at(source.len())
        .map(|binding| document.syntax.name_text(source, binding.name))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["latest", "updated_outer"]);
    assert_eq!(document.syntax.visible_bindings(source.len()).count(), 0);
    let latest = document
        .syntax
        .name_id(source, "latest")
        .must_be("pattern name");
    assert!(
        document
            .syntax
            .binding_named_at(latest, source.len())
            .is_none()
    );
}

#[test]
fn import_path_completion_is_header_owned_and_retains_empty_unsaved_paths() {
    for source in ["import ", "import Base.List as List\ndef main: 0\n"] {
        let document = snapshot(source);
        let offset = source.find("import ").must_be("header") + "import ".len();
        let range = document
            .syntax
            .completion_import_path(offset)
            .must_be("indexed import path");
        assert_eq!(
            &source[range.start..range.end],
            if source == "import " { "" } else { "Base.List" }
        );
        if let Some(alias) = source.find("as List") {
            assert!(document.syntax.completion_import_path(alias).is_none());
        }
    }
    let source = "def main: import ";
    assert!(
        snapshot(source)
            .syntax
            .completion_import_path(source.len())
            .is_none()
    );
}

#[test]
fn keyword_coloring_never_turns_dotted_semantic_atoms_into_keywords() {
    use bend2_lsp::analysis::{ReferenceRole, SyntaxIndex, semantic_tokens};

    let source = "def D.if(value): value\ndef D.match: D.if(0)\n";
    let document = snapshot(source);
    let syntax = &document.syntax;
    let keyword = syntax.token_at_or_before(0).must_be("def token");
    assert!(syntax.is_keyword(syntax.name_for_token(keyword).must_be("keyword name")));
    assert!(syntax.reference_for_token(keyword).is_none());
    assert!(!SyntaxIndex::is_keyword_text("if"));
    assert!(SyntaxIndex::is_keyword_text("match"));
    let offset = source.find("D.match").must_be("dotted declaration") + 2;
    let token = syntax
        .token_at_or_before(offset)
        .must_be("match atom segment");
    let occurrence = syntax
        .reference_for_token(token)
        .must_be("dotted semantic atom");
    assert_eq!(occurrence.role, ReferenceRole::Binding);
    assert_eq!(syntax.name_text(source, occurrence.full_name), "D.match");
    let colors = semantic_tokens(&document);
    assert_eq!(
        colors
            .iter()
            .find(|color| color.range.start == 0)
            .map(|color| color.token_type),
        Some(15)
    );
    assert_eq!(
        colors
            .iter()
            .find(|color| color.range.start == offset)
            .map(|color| color.token_type),
        Some(12)
    );
}

#[test]
fn native_import_headers_accept_reserved_aliases_without_term_occurrences() {
    for (header, alias) in [
        ("import Base", None),
        ("import ./dep.bend as def#comment", Some("def")),
        ("import ./dep.bend as if", Some("if")),
        ("import ./dep.bend as Type", Some("Type")),
        ("import ./dep.bend as as", Some("as")),
        ("import ./dep.bend as unsafe", Some("unsafe")),
        ("\u{FEFF}import\u{00A0}./dep.bend as Name", Some("Name")),
    ] {
        let body = alias.map_or_else(|| "0".to_owned(), |alias| format!("{alias}.target(0)"));
        let source = format!("{header}\ndef main: {body}\n");
        let document = snapshot(&source);
        let import = document
            .syntax
            .imports()
            .first()
            .must_be("native import metadata");
        assert!(import.valid_syntax, "{header:?}");
        assert_eq!(import.alias_text(&source), alias);
        assert!(
            document
                .syntax
                .all_references()
                .iter()
                .all(|reference| reference.full_range.start > header.len())
        );
        assert!(
            document
                .syntax
                .name_id(&source, "main")
                .and_then(|name| document.syntax.symbol_by_name(name))
                .is_some()
        );
        if let Some(alias) = alias {
            let call = document
                .syntax
                .calls()
                .first()
                .must_be("qualified reserved-alias call");
            let occurrence = document
                .syntax
                .reference_for_token(call.callee_token)
                .must_be("opaque dotted semantic atom");
            assert_eq!(
                document.syntax.name_text(&source, occurrence.full_name),
                format!("{alias}.target")
            );
            assert_eq!(occurrence.role, bend2_lsp::analysis::ReferenceRole::Call);
            assert_eq!(
                occurrence
                    .qualifier
                    .map(|name| document.syntax.name_text(&source, name)),
                Some(alias)
            );
            assert_eq!(document.syntax.reference_segments(occurrence).len(), 2);
        }
    }
    for header in [
        "import ./dep.bend",
        "import Base as #comment",
        "import Base as Alias trailing",
        "import ./dep.bend as D.foo",
        "import ./dep.bend as élse",
        "import ./dep.bend as Aliasé",
        "import ./dep.bend as",
        "import Base\u{0085}",
        "import",
    ] {
        let source = format!("{header}\ndef main: 0\n");
        let document = snapshot(&source);
        let import = document
            .syntax
            .imports()
            .first()
            .must_be("invalid header metadata");
        assert!(!import.valid_syntax, "{header:?}");
        assert!(
            document
                .syntax
                .name_id(&source, "main")
                .and_then(|name| document.syntax.symbol_by_name(name))
                .is_some(),
            "an incomplete alias must not absorb the next declaration: {header:?}"
        );
    }
}
