use std::collections::HashMap;

use super::{
    CallSite, IndexedBinding, IndexedSymbol, NameId, NameTable, Reference, ReferenceKind, SymbolId,
    TextRange, Token, TokenId, TokenKind, compact_groups, enclosing_function,
    qualified_chain_start,
};

pub(super) struct ReferenceInputs<'a> {
    pub(super) source: &'a str,
    pub(super) tokens: &'a [Token],
    pub(super) names: &'a mut NameTable,
    pub(super) symbols: &'a [IndexedSymbol],
    pub(super) bindings: &'a [IndexedBinding],
    pub(super) token_symbols: &'a [Option<SymbolId>],
    pub(super) calls: &'a [CallSite],
}

pub(super) struct ReferenceIndex {
    pub(super) references: Vec<Reference>,
    pub(super) by_symbol_indices: Vec<usize>,
    pub(super) by_symbol_spans: Vec<TextRange>,
    pub(super) by_name_indices: Vec<usize>,
    pub(super) by_name_spans: Vec<TextRange>,
    pub(super) token_references: Vec<Option<usize>>,
    pub(super) local_reference_occurrences: usize,
}

pub(super) fn resolve_symbols(
    source: &str,
    tokens: &[Token],
    names: &NameTable,
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
    symbol_by_name: &[Option<SymbolId>],
) -> Vec<Option<SymbolId>> {
    let mut token_symbols = vec![None; tokens.len()];
    for symbol in symbols {
        let start = tokens.partition_point(|token| token.range.end <= symbol.name_range.start);
        let end = tokens.partition_point(|token| token.range.start < symbol.name_range.end);
        for (offset, token) in tokens[start..end].iter().enumerate() {
            let index = start + offset;
            if token.kind == TokenKind::Identifier {
                token_symbols[index] = Some(symbol.id);
            }
        }
    }
    let mut bindings_by_name = HashMap::<NameId, Vec<usize>>::new();
    for (index, binding) in bindings.iter().enumerate() {
        token_symbols[binding.declaration.0] = Some(binding.id);
        bindings_by_name
            .entry(binding.name)
            .or_default()
            .push(index);
    }
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Identifier || token_symbols[index].is_some() {
            continue;
        }
        let token_id = TokenId(index);
        let symbol = if is_qualified_token(source, tokens, index)
            && qualified_chain_start(source, tokens, index) != index
        {
            qualified_symbol_for_token(source, tokens, index, names, symbol_by_name)
        } else {
            let name = names.for_token(token_id);
            name.and_then(|name| {
                bindings_by_name
                    .get(&name)
                    .into_iter()
                    .flatten()
                    .filter_map(|binding| {
                        let binding = bindings[*binding];
                        binding
                            .scope
                            .contains(token.range.start)
                            .then_some((binding.scope.end - binding.scope.start, binding.id))
                    })
                    .min_by_key(|(scope_size, _)| *scope_size)
                    .map(|(_, id)| id)
                    .or_else(|| symbol_by_name.get(name.0).copied().flatten())
            })
        };
        token_symbols[index] = symbol;
    }
    token_symbols
}

pub(super) fn build_references(
    input: ReferenceInputs<'_>,
    group_counts: &mut Vec<usize>,
) -> ReferenceIndex {
    let ReferenceInputs {
        source,
        tokens,
        names,
        symbols,
        bindings,
        token_symbols,
        calls,
    } = input;
    let mut references = Vec::with_capacity(tokens.len());
    let mut token_references = vec![None; tokens.len()];
    // Calls are emitted in callee-token order by the opening-delimiter scan.
    let mut next_call = 0;
    let mut local_reference_occurrences = append_declaration_references(
        tokens,
        symbols,
        bindings,
        &mut references,
        &mut token_references,
    );
    for (token_index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Identifier || token_references[token_index].is_some() {
            continue;
        }
        let token_id = TokenId(token_index);
        let Some(name) = names.for_token(token_id) else {
            continue;
        };
        let call = calls
            .get(next_call)
            .filter(|call| call.callee_token == token_id);
        if call.is_some() {
            next_call += 1;
        }
        let (qualifier, qualifier_token) =
            if let Some(call) = call {
                (call.qualifier, call.qualifier_token)
            } else {
                let start_index = qualified_chain_start(source, tokens, token_index);
                if start_index + 2 == token_index {
                    (
                        names.for_token(TokenId(start_index)),
                        Some(TokenId(start_index)),
                    )
                } else if start_index < token_index {
                    let end = tokens[token_index - 2].range.end;
                    (
                        Some(names.intern_name(
                            source,
                            TextRange::new(tokens[start_index].range.start, end),
                        )),
                        Some(TokenId(start_index)),
                    )
                } else {
                    (None, None)
                }
            };
        let index = references.len();
        references.push(Reference {
            name,
            qualifier,
            qualifier_token,
            token: token_id,
            range: token.range,
            enclosing: call.map_or_else(
                || enclosing_function(symbols, token.range.start),
                |call| call.caller,
            ),
            kind: if call.is_some() {
                ReferenceKind::Call
            } else {
                ReferenceKind::Read
            },
            resolved: token_symbols[token_index],
        });
        token_references[token_index] = Some(index);
        if token_symbols[token_index].is_some() {
            local_reference_occurrences += 1;
        }
    }
    let (by_symbol_indices, by_symbol_spans) = compact_groups(
        &references,
        symbols.len() + bindings.len(),
        group_counts,
        |reference| reference.resolved.map(|id| id.0),
    );
    let (by_name_indices, by_name_spans) =
        compact_groups(&references, names.ranges.len(), group_counts, |reference| {
            Some(reference.name.0)
        });
    ReferenceIndex {
        references,
        by_symbol_indices,
        by_symbol_spans,
        by_name_indices,
        by_name_spans,
        token_references,
        local_reference_occurrences,
    }
}

fn append_declaration_references(
    tokens: &[Token],
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
    references: &mut Vec<Reference>,
    token_references: &mut [Option<usize>],
) -> usize {
    let mut local_reference_occurrences = 0;
    // Declaration rows are always resolved, so only the first mapping of each
    // token contributes; later declaration or binding rows may overwrite it.
    for symbol in symbols {
        let start = tokens.partition_point(|token| token.range.start < symbol.name_range.start);
        let end = tokens.partition_point(|token| token.range.start < symbol.name_range.end);
        let Some(token_index) = tokens[start..end]
            .iter()
            .rposition(|token| token.kind == TokenKind::Identifier)
            .map(|offset| start + offset)
        else {
            continue;
        };
        let index = references.len();
        references.push(Reference {
            name: symbol.name,
            qualifier: None,
            qualifier_token: None,
            token: TokenId(token_index),
            range: symbol.name_range,
            enclosing: None,
            kind: ReferenceKind::Declaration,
            resolved: Some(symbol.id),
        });
        for (offset, token) in tokens[start..end].iter().enumerate() {
            if token.kind == TokenKind::Identifier {
                let slot = &mut token_references[start + offset];
                local_reference_occurrences += usize::from(slot.is_none());
                *slot = Some(index);
            }
        }
    }
    for binding in bindings {
        let index = references.len();
        references.push(Reference {
            name: binding.name,
            qualifier: None,
            qualifier_token: None,
            token: binding.declaration,
            range: binding.name_range,
            enclosing: Some(binding.owner),
            kind: ReferenceKind::Declaration,
            resolved: Some(binding.id),
        });
        let slot = &mut token_references[binding.declaration.0];
        local_reference_occurrences += usize::from(slot.is_none());
        *slot = Some(index);
    }
    local_reference_occurrences
}

fn is_qualified_token(source: &str, tokens: &[Token], index: usize) -> bool {
    let token = tokens[index];
    let adjacent_dot = |dot: Token, name: Token| {
        dot.kind == TokenKind::Punctuation
            && &source[dot.range.start..dot.range.end] == "."
            && dot.range.end == name.range.start
    };
    index > 0 && adjacent_dot(tokens[index - 1], token)
        || index + 1 < tokens.len() && adjacent_dot(tokens[index + 1], token)
}

fn qualified_symbol_for_token(
    source: &str,
    tokens: &[Token],
    index: usize,
    names: &NameTable,
    symbol_by_name: &[Option<SymbolId>],
) -> Option<SymbolId> {
    let mut start = index;
    while start >= 2
        && tokens[start - 1].kind == TokenKind::Punctuation
        && &source[tokens[start - 1].range.start..tokens[start - 1].range.end] == "."
        && tokens[start - 1].range.end == tokens[start].range.start
        && tokens[start - 2].kind == TokenKind::Identifier
        && tokens[start - 2].range.end == tokens[start - 1].range.start
    {
        start -= 2;
    }
    let mut end = index + 1;
    while end + 1 < tokens.len()
        && tokens[end].kind == TokenKind::Punctuation
        && &source[tokens[end].range.start..tokens[end].range.end] == "."
        && tokens[end - 1].range.end == tokens[end].range.start
        && tokens[end + 1].kind == TokenKind::Identifier
        && tokens[end].range.end == tokens[end + 1].range.start
    {
        end += 2;
    }
    if end - start < 3 {
        return None;
    }
    let range = TextRange::new(tokens[start].range.start, tokens[end - 1].range.end);
    let name = names.find(source, &source[range.start..range.end])?;
    symbol_by_name.get(name.0).copied().flatten()
}
