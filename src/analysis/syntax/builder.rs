use super::super::LineIndex;
use super::{
    AuxiliaryIndex, ConstructorIndex, IndexedSymbol, SymbolId, SymbolKind, SyntaxIndex, SyntaxLine,
    Token, TokenKind,
    calls::{CallInputs, build_calls},
    declarations::{
        BindingContext, build_lines, parse_bindings, parse_constructors, parse_imports,
        parse_symbols,
    },
    names::{NameTable, keyword_name_flags},
    references::{ReferenceInputs, build_references, resolve_symbols},
    scanner::{ScanOutput, Scanner},
};

impl SyntaxIndex {
    pub(crate) fn build(source: &str) -> (LineIndex, Self) {
        let mut scan = Scanner::new(source, true).scan();
        let line_index = LineIndex::from_parts(
            source,
            std::mem::take(&mut scan.line_starts),
            std::mem::take(&mut scan.line_ascii),
        );
        let index = Self::from_scan(source, &line_index, scan);
        (line_index, index)
    }

    pub(crate) fn build_with_line_index(source: &str, line_index: &LineIndex) -> Self {
        let scan = Scanner::new(source, false).scan();
        Self::from_scan(source, line_index, scan)
    }

    fn from_scan(source: &str, line_index: &LineIndex, scan: ScanOutput) -> Self {
        let lines = build_lines(source, line_index);
        let imports = parse_imports(source, &lines);
        let ScanOutput {
            tokens,
            names,
            diagnostics,
            delimiter_pairs,
            delimiter_context,
            ..
        } = scan;
        let mut names = names;
        let keyword_names = keyword_name_flags(source, &names);
        let mut symbols = parse_symbols(source, &lines, &mut names);
        let symbol_by_name = assign_symbol_ids(&mut symbols, names.ranges.len());
        let constructor_index = build_constructor_index(source, &lines, &symbols, &mut names);
        let auxiliary = AuxiliaryIndex::new(diagnostics, constructor_index);
        let bindings = parse_bindings(
            &BindingContext {
                source,
                lines: &lines,
                tokens: &tokens,
                delimiter_pairs: &delimiter_pairs,
                delimiter_context: &delimiter_context,
                symbol_count: symbols.len(),
                names: &names,
            },
            &mut symbols,
        );
        let token_symbols = resolve_symbols(
            source,
            &tokens,
            &names,
            &symbols,
            &bindings,
            &symbol_by_name,
        );
        // All four compact indexes share one temporary count/cursor buffer.
        let mut group_counts =
            Vec::with_capacity(names.ranges.len().max(symbols.len() + bindings.len()));
        let calls = build_calls(
            CallInputs {
                source,
                tokens: &tokens,
                delimiter_pairs: &delimiter_pairs,
                delimiter_context: &delimiter_context,
                symbols: &symbols,
                bindings: &bindings,
                token_symbols: &token_symbols,
                names: &mut names,
            },
            &mut group_counts,
        );
        let references = build_references(
            ReferenceInputs {
                source,
                tokens: &tokens,
                names: &mut names,
                symbols: &symbols,
                bindings: &bindings,
                token_symbols: &token_symbols,
                calls: &calls.calls,
            },
            &mut group_counts,
        );
        Self {
            semantic_token_count: count_semantic_tokens(&tokens),
            tokens: tokens.into_boxed_slice(),
            names,
            imports: imports.into_boxed_slice(),
            keyword_names: keyword_names.into_boxed_slice(),
            symbols: symbols.into_boxed_slice(),
            bindings: bindings.into_boxed_slice(),
            references: references.references.into_boxed_slice(),
            external_reference_candidates: references
                .external_reference_candidates
                .into_boxed_slice(),
            reference_indices: references.by_symbol_indices.into_boxed_slice(),
            reference_spans: references.by_symbol_spans.into_boxed_slice(),
            name_reference_indices: references.by_name_indices.into_boxed_slice(),
            name_reference_spans: references.by_name_spans.into_boxed_slice(),
            token_references: references.token_references.into_boxed_slice(),
            local_reference_occurrences: references.local_reference_occurrences,
            calls: calls.calls.into_boxed_slice(),
            local_function_calls: calls.local_function_calls,
            call_arguments: calls.arguments.into_boxed_slice(),
            call_separators: calls.separators.into_boxed_slice(),
            calls_from_indices: calls.from_indices.into_boxed_slice(),
            calls_from_spans: calls.from_spans.into_boxed_slice(),
            calls_to_indices: calls.to_indices.into_boxed_slice(),
            calls_to_spans: calls.to_spans.into_boxed_slice(),
            call_by_open: calls.by_open.into_boxed_slice(),
            call_by_name_token: calls.by_name_token.into_boxed_slice(),
            lines: lines.into_boxed_slice(),
            auxiliary,
            delimiter_pairs: delimiter_pairs.into_boxed_slice(),
            delimiter_context: delimiter_context.into_boxed_slice(),
            symbol_by_name: symbol_by_name.into_boxed_slice(),
            token_symbols: token_symbols.into_boxed_slice(),
        }
    }
}

fn build_constructor_index(
    source: &str,
    lines: &[SyntaxLine],
    symbols: &[IndexedSymbol],
    names: &mut NameTable,
) -> Option<ConstructorIndex> {
    if symbols
        .iter()
        .any(|symbol| symbol.kind == SymbolKind::Struct)
    {
        parse_constructors(source, lines, symbols, names)
    } else {
        None
    }
}

fn assign_symbol_ids(symbols: &mut [IndexedSymbol], name_count: usize) -> Vec<Option<SymbolId>> {
    let mut by_name = vec![None; name_count];
    for (index, symbol) in symbols.iter_mut().enumerate() {
        symbol.id = SymbolId(index);
        by_name[symbol.name.0].get_or_insert(symbol.id);
    }
    by_name
}

fn count_semantic_tokens(tokens: &[Token]) -> usize {
    tokens
        .iter()
        .filter(|token| !matches!(token.kind, TokenKind::Punctuation | TokenKind::Delimiter))
        .count()
}
