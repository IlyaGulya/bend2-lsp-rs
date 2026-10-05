use super::{
    CallSite, DelimiterPair, IndexedBinding, IndexedSymbol, NameTable, SymbolId, TextRange, Token,
    TokenId, TokenKind, compact_groups, declarations::declaration_tokens, enclosing_function,
    qualified_chain_start,
};

pub(super) struct CallInputs<'a> {
    pub(super) source: &'a str,
    pub(super) tokens: &'a [Token],
    pub(super) delimiter_pairs: &'a [DelimiterPair],
    pub(super) delimiter_context: &'a [Option<TokenId>],
    pub(super) symbols: &'a [IndexedSymbol],
    pub(super) bindings: &'a [IndexedBinding],
    pub(super) token_symbols: &'a [Option<SymbolId>],
    pub(super) names: &'a mut NameTable,
}

struct CallBuild<'a> {
    source: &'a str,
    tokens: &'a [Token],
    delimiter_pairs: &'a [DelimiterPair],
    delimiter_context: &'a [Option<TokenId>],
    symbols: &'a [IndexedSymbol],
    token_symbols: &'a [Option<SymbolId>],
    declarations: &'a [bool],
    names: &'a mut NameTable,
}

pub(super) struct CallIndex {
    pub(super) calls: Vec<CallSite>,
    pub(super) arguments: Vec<TextRange>,
    pub(super) separators: Vec<usize>,
    pub(super) by_open: Vec<Option<usize>>,
    pub(super) by_name_token: Vec<Option<usize>>,
    pub(super) from_indices: Vec<usize>,
    pub(super) from_spans: Vec<TextRange>,
    pub(super) to_indices: Vec<usize>,
    pub(super) to_spans: Vec<TextRange>,
}

pub(super) fn build_calls(input: CallInputs<'_>, group_counts: &mut Vec<usize>) -> CallIndex {
    let CallInputs {
        source,
        tokens,
        delimiter_pairs,
        delimiter_context,
        symbols,
        bindings,
        token_symbols,
        names,
    } = input;
    let declarations = declaration_tokens(tokens, symbols, bindings);
    let mut context = CallBuild {
        source,
        tokens,
        delimiter_pairs,
        delimiter_context,
        symbols,
        token_symbols,
        declarations: &declarations,
        names,
    };
    let mut calls = Vec::new();
    let mut arguments = Vec::new();
    let mut separators = Vec::new();
    let mut by_open = vec![None; tokens.len()];
    let mut by_name_token = vec![None; tokens.len()];
    for (open_index, (open_token, by_open_slot)) in
        context.tokens.iter().copied().zip(&mut by_open).enumerate()
    {
        let Some(call) = parse_call(
            &mut context,
            open_index,
            open_token,
            &mut arguments,
            &mut separators,
        ) else {
            continue;
        };
        let call_index = calls.len();
        *by_open_slot = Some(call_index);
        by_name_token[call.callee_token.0] = Some(call_index);
        if let Some(qualifier_token) = call.qualifier_token {
            by_name_token[qualifier_token.0] = Some(call_index);
        }
        calls.push(call);
    }
    let id_count = symbols.len() + bindings.len();
    let (from_indices, from_spans) = compact_groups(&calls, id_count, group_counts, |call| {
        call.caller.map(|id| id.0)
    });
    let (to_indices, to_spans) = compact_groups(&calls, id_count, group_counts, |call| {
        call.callee.map(|id| id.0)
    });
    CallIndex {
        calls,
        arguments,
        separators,
        by_open,
        by_name_token,
        from_indices,
        from_spans,
        to_indices,
        to_spans,
    }
}

fn parse_call(
    context: &mut CallBuild<'_>,
    open_index: usize,
    open_token: Token,
    arguments: &mut Vec<TextRange>,
    separators: &mut Vec<usize>,
) -> Option<CallSite> {
    if open_token.kind != TokenKind::Delimiter
        || &context.source[open_token.range.start..open_token.range.end] != "("
    {
        return None;
    }
    let callee_index = open_index.checked_sub(1)?;
    let callee_token = context.tokens[callee_index];
    if callee_token.kind != TokenKind::Identifier
        || context.declarations[callee_index]
        || !context.source[callee_token.range.end..open_token.range.start]
            .bytes()
            .all(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    let name = context.names.for_token(TokenId(callee_index))?;
    let start_index = qualified_chain_start(context.source, context.tokens, callee_index);
    let qualifier = if start_index + 2 == callee_index {
        context.names.for_token(TokenId(start_index))
    } else if start_index < callee_index {
        let end = context.tokens[callee_index - 2].range.end;
        Some(context.names.intern_name(
            context.source,
            TextRange::new(context.tokens[start_index].range.start, end),
        ))
    } else {
        None
    };
    let qualifier_token = (start_index < callee_index).then_some(TokenId(start_index));
    let close = context
        .delimiter_pairs
        .binary_search_by_key(&TokenId(open_index), |pair| pair.open)
        .ok()
        .map(|index| context.delimiter_pairs[index].close);
    let argument_end = close.map_or(context.source.len(), |close| {
        context.tokens[close.0].range.start
    });
    let call_end = close.map_or(context.source.len(), |close| {
        context.tokens[close.0].range.end
    });
    let limit = close.map_or(context.tokens.len(), |close| close.0);
    let argument_start_index = arguments.len();
    let separator_start_index = separators.len();
    let mut segment_start = open_token.range.end;
    for index in open_index + 1..limit {
        let token = context.tokens[index];
        if token.kind == TokenKind::Punctuation
            && context.delimiter_context[index] == Some(TokenId(open_index))
            && &context.source[token.range.start..token.range.end] == ","
        {
            if let Some(range) = trimmed_range(context.source, segment_start, token.range.start) {
                arguments.push(range);
            }
            separators.push(token.range.start);
            segment_start = token.range.end;
        }
    }
    if let Some(range) = trimmed_range(context.source, segment_start, argument_end) {
        arguments.push(range);
    }
    Some(CallSite {
        caller: enclosing_function(context.symbols, callee_token.range.start),
        callee: context.token_symbols[callee_index],
        name,
        qualifier,
        qualifier_token,
        callee_token: TokenId(callee_index),
        callee_range: callee_token.range,
        call_range: TextRange::new(context.tokens[start_index].range.start, call_end),
        argument_range: TextRange::new(open_token.range.end, argument_end),
        argument_indices: TextRange::new(argument_start_index, arguments.len()),
        separator_indices: TextRange::new(separator_start_index, separators.len()),
        open: TokenId(open_index),
    })
}

fn trimmed_range(source: &str, start: usize, end: usize) -> Option<TextRange> {
    let text = source.get(start..end)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let prefix = text.len() - text.trim_start().len();
    Some(TextRange::new(
        start + prefix,
        start + prefix + trimmed.len(),
    ))
}
