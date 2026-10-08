use super::{
    BindingContext, ConstructorIndex, IndexedBinding, IndexedConstructor, IndexedImport,
    IndexedSymbol, LineIndex, NameTable, SymbolId, SymbolKind, SyntaxLine, TextRange, Token,
    TokenId, TokenKind,
};

pub(super) fn build_lines(source: &str, index: &LineIndex) -> Vec<SyntaxLine> {
    (0..index.line_starts.len())
        .map(|line| {
            let start = index.line_starts[line];
            let full_end = index
                .line_starts
                .get(line + 1)
                .copied()
                .unwrap_or(source.len());
            let content = index.line_range(source, line).unwrap_or_default();
            let indent = source[content.start..content.end]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            SyntaxLine {
                content: TextRange::new(start, content.end),
                full_end,
                code: TextRange::new(start + indent, content.end),
                indent,
            }
        })
        .collect()
}

pub(super) fn parse_imports(source: &str, lines: &[SyntaxLine]) -> Vec<IndexedImport> {
    let mut imports = Vec::new();
    for line in lines {
        let code = &source[line.code.start..line.code.end];
        if code.trim().is_empty() || code.trim_start().starts_with('#') {
            continue;
        }
        if line.indent != 0 {
            break;
        }
        let Some(rest) = code.strip_prefix("import") else {
            break;
        };
        if !rest.starts_with(char::is_whitespace) {
            break;
        }
        let path_start = code.len() - rest.trim_start().len();
        let path_end = code[path_start..]
            .find(char::is_whitespace)
            .map_or(code.len(), |width| path_start + width);
        if path_start == path_end || code[path_start..].starts_with('#') {
            break;
        }
        let suffix = code[path_end..].trim_start();
        let alias = suffix
            .strip_prefix("as")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map(str::trim_start)
            .and_then(|rest| {
                let end = rest
                    .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .unwrap_or(rest.len());
                (end > 0
                    && (rest.as_bytes()[0].is_ascii_alphabetic() || rest.as_bytes()[0] == b'_'))
                    .then_some((code.len() - rest.len(), end))
            });
        imports.push(IndexedImport {
            path: TextRange::new(line.code.start + path_start, line.code.start + path_end),
            alias: alias.map(|(start, end)| {
                TextRange::new(line.code.start + start, line.code.start + start + end)
            }),
        });
    }
    imports
}

pub(super) fn parse_symbols(
    source: &str,
    lines: &[SyntaxLine],
    names: &mut NameTable,
) -> Vec<IndexedSymbol> {
    let mut symbols: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(line_number, line)| parse_top_level_symbol(source, *line, line_number, names))
        .collect();
    let mut symbol_index = symbols.len();
    let mut next_scope_end = source.len();
    for (line_number, line) in lines.iter().enumerate().rev() {
        let code = source[line.code.start..line.code.end].trim();
        if line.indent != 0 || code.is_empty() || code.starts_with('#') {
            continue;
        }
        if symbol_index > 0 && symbols[symbol_index - 1].start_line == line_number {
            symbols[symbol_index - 1].scope = TextRange::new(line.full_end, next_scope_end);
            symbol_index -= 1;
        }
        next_scope_end = line.content.start;
    }
    symbols
}

// Keep type-body parsing out of the already large snapshot builder.
#[cold]
#[inline(never)]
pub(super) fn parse_constructors(
    source: &str,
    lines: &[SyntaxLine],
    symbols: &[IndexedSymbol],
    names: &mut NameTable,
) -> Option<ConstructorIndex> {
    let mut constructors = Vec::new();
    for (index, parent) in symbols.iter().enumerate() {
        if parent.kind != SymbolKind::Struct {
            continue;
        }
        let end = symbols
            .get(index + 1)
            .map_or(lines.len(), |next| next.start_line);
        let body = &lines[parent.start_line + 1..end];
        let mut body_indent = usize::MAX;
        for line in body {
            if line.indent == 0 || line.indent >= body_indent {
                continue;
            }
            let code = source[line.code.start..line.code.end].trim_start();
            if !code.is_empty() && !code.starts_with('#') {
                body_indent = line.indent;
            }
        }
        if body_indent == usize::MAX {
            continue;
        }
        for line in body {
            if line.indent != body_indent {
                continue;
            }
            let code = &source[line.code.start..line.code.end];
            let Some(first) = code.as_bytes().first() else {
                continue;
            };
            if !first.is_ascii_alphabetic() && *first != b'_' {
                continue;
            }
            let name_len = code
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
                .count();
            let remainder = code[name_len..].trim_start();
            if !remainder.is_empty() && !remainder.starts_with('{') {
                continue;
            }
            let name_range = TextRange::new(line.code.start, line.code.start + name_len);
            constructors.push(IndexedConstructor {
                parent: parent.id,
                name: names.intern_name(source, name_range),
                name_range,
                range: TextRange::new(line.content.start, line.full_end),
            });
        }
    }
    if constructors.is_empty() {
        return None;
    }
    let name_indices = if constructors
        .windows(2)
        .all(|pair| pair[0].name.0 <= pair[1].name.0)
    {
        Vec::new()
    } else {
        let mut indices: Vec<_> = (0..constructors.len()).collect();
        indices.sort_unstable_by_key(|index| (constructors[*index].name.0, *index));
        indices
    };
    Some(ConstructorIndex {
        entries: constructors.into_boxed_slice(),
        name_indices: name_indices.into_boxed_slice(),
    })
}

pub(super) fn parse_bindings(
    context: &BindingContext<'_>,
    symbols: &mut [IndexedSymbol],
) -> Vec<IndexedBinding> {
    let mut bindings = Vec::new();
    for symbol in symbols {
        if symbol.kind != SymbolKind::Function {
            continue;
        }
        let parameter_start = bindings.len();
        parse_function_parameters(context, symbol, &mut bindings);
        symbol.parameter_span = TextRange::new(parameter_start, bindings.len());
        parse_pattern_bindings(context, symbol, &mut bindings);
    }
    bindings
}

fn parse_function_parameters(
    context: &BindingContext<'_>,
    symbol: &IndexedSymbol,
    bindings: &mut Vec<IndexedBinding>,
) {
    let header_end = context.lines[symbol.start_line].full_end;
    let start = context
        .tokens
        .partition_point(|token| token.range.start < symbol.name_range.end);
    let open = context
        .tokens
        .get(start)
        .filter(|token| {
            token.range.start < header_end
                && token.kind == TokenKind::Delimiter
                && &context.source[token.range.start..token.range.end] == "("
        })
        .map(|_| TokenId(start));
    let Some(open) = open else {
        return;
    };
    let close = context
        .delimiter_pairs
        .binary_search_by_key(&open, |pair| pair.open)
        .ok()
        .map_or_else(
            || {
                context
                    .tokens
                    .partition_point(|token| token.range.start < header_end)
            },
            |pair| context.delimiter_pairs[pair].close.0,
        );
    let mut parameter_start = open.0 + 1;
    for comma in open.0 + 1..close {
        if context.tokens[comma].kind == TokenKind::Punctuation
            && context.delimiter_context[comma] == Some(open)
            && &context.source[context.tokens[comma].range.start..context.tokens[comma].range.end]
                == ","
        {
            if let Some(token) =
                parameter_binding_token(context.source, context.tokens, parameter_start, comma)
                && let Some(binding) =
                    make_binding(context, bindings.len(), symbol.id, symbol.scope, token)
            {
                bindings.push(binding);
            }
            parameter_start = comma + 1;
        }
    }
    if let Some(token) =
        parameter_binding_token(context.source, context.tokens, parameter_start, close)
        && let Some(binding) = make_binding(context, bindings.len(), symbol.id, symbol.scope, token)
    {
        bindings.push(binding);
    }
}

fn parse_pattern_bindings(
    context: &BindingContext<'_>,
    symbol: &IndexedSymbol,
    bindings: &mut Vec<IndexedBinding>,
) {
    let start_line = symbol.start_line;
    let end_line = context
        .lines
        .partition_point(|line| line.content.start < symbol.scope.end);
    let mut active_cases = Vec::<(usize, Vec<usize>)>::new();
    for line in &context.lines[start_line + 1..end_line] {
        let code = &context.source[line.code.start..line.code.end];
        if code.trim().is_empty() || code.trim_start().starts_with('#') {
            continue;
        }
        while active_cases
            .last()
            .is_some_and(|(indent, _)| line.indent <= *indent)
        {
            let Some((_, indices)) = active_cases.pop() else {
                break;
            };
            for index in indices {
                bindings[index].scope.end = line.content.start;
            }
        }
        let first = context
            .tokens
            .partition_point(|token| token.range.start < line.code.start);
        let limit = context
            .tokens
            .partition_point(|token| token.range.start < line.content.end);
        if first >= limit
            || context.tokens[first].kind != TokenKind::Identifier
            || &context.source[context.tokens[first].range.start..context.tokens[first].range.end]
                != "case"
        {
            continue;
        }
        let colon = (first + 1..limit).find(|index| {
            context.tokens[*index].kind == TokenKind::Punctuation
                && context.delimiter_context[*index].is_none()
                && &context.source
                    [context.tokens[*index].range.start..context.tokens[*index].range.end]
                    == ":"
        });
        let Some(colon) = colon else {
            continue;
        };
        let scope = TextRange::new(line.full_end, symbol.scope.end);
        let mut indices = Vec::new();
        for token_index in first + 1..colon {
            let token = context.tokens[token_index];
            if token.kind != TokenKind::Identifier {
                continue;
            }
            let text = &context.source[token.range.start..token.range.end];
            if !text.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && !text.starts_with('_')
            {
                continue;
            }
            if let Some(binding) = make_binding(
                context,
                bindings.len(),
                symbol.id,
                scope,
                TokenId(token_index),
            ) {
                indices.push(bindings.len());
                bindings.push(binding);
            }
        }
        if !indices.is_empty() {
            active_cases.push((line.indent, indices));
        }
    }
}

fn parameter_binding_token(
    source: &str,
    tokens: &[Token],
    start: usize,
    end: usize,
) -> Option<TokenId> {
    (start..end)
        .take_while(|index| {
            !(tokens[*index].kind == TokenKind::Punctuation
                && &source[tokens[*index].range.start..tokens[*index].range.end] == ":")
        })
        .find(|index| tokens[*index].kind == TokenKind::Identifier)
        .map(TokenId)
}

fn make_binding(
    context: &BindingContext<'_>,
    binding_index: usize,
    owner: SymbolId,
    scope: TextRange,
    token: TokenId,
) -> Option<IndexedBinding> {
    Some(IndexedBinding {
        id: SymbolId(context.symbol_count + binding_index),
        owner,
        name: context.names.for_token(token)?,
        name_range: context.tokens.get(token.0)?.range,
        scope,
        declaration: token,
    })
}

pub(super) fn declaration_tokens(
    tokens: &[Token],
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
) -> Vec<bool> {
    let mut declarations = vec![false; tokens.len()];
    for symbol in symbols {
        let start = tokens.partition_point(|token| token.range.start < symbol.name_range.start);
        let end = tokens.partition_point(|token| token.range.start < symbol.name_range.end);
        for (offset, token) in tokens[start..end].iter().enumerate() {
            if token.kind == TokenKind::Identifier {
                declarations[start + offset] = true;
            }
        }
    }
    for binding in bindings {
        declarations[binding.declaration.0] = true;
    }
    declarations
}

fn parse_top_level_symbol(
    source: &str,
    line: SyntaxLine,
    line_number: usize,
    names: &mut NameTable,
) -> Option<IndexedSymbol> {
    let code = &source[line.code.start..line.code.end];
    let mut parsed = code;
    if let Some(rest) = parsed.strip_prefix("@unsafe") {
        parsed = rest.trim_start();
    }
    let keyword_end = parsed.find(char::is_whitespace)?;
    let keyword = &parsed[..keyword_end];
    let kind = match keyword {
        "def" | "law" => SymbolKind::Function,
        "type" => SymbolKind::Struct,
        _ => return None,
    };
    let remainder = parsed[keyword_end..].trim_start();
    let name_len = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        .count();
    if name_len == 0
        || !remainder.as_bytes()[0].is_ascii_alphabetic() && remainder.as_bytes()[0] != b'_'
    {
        return None;
    }
    let parse_offset = code.len() - parsed.len();
    let after_keyword = &parsed[keyword_end..];
    let name_offset =
        parse_offset + keyword_end + after_keyword.len() - after_keyword.trim_start().len();
    let name_range = TextRange::new(
        line.code.start + name_offset,
        line.code.start + name_offset + name_len,
    );
    let detail = code.trim();
    let detail_start = line.code.start + code.len() - code.trim_start().len();
    let detail = detail.strip_suffix(':').unwrap_or(detail);
    let detail_range = TextRange::new(detail_start, detail_start + detail.len());
    let name = names.intern_name(source, TextRange::new(name_range.start, name_range.end));
    Some(IndexedSymbol {
        id: SymbolId(usize::MAX),
        name,
        name_range,
        detail_range,
        parameter_span: TextRange::new(0, 0),
        scope: TextRange::new(line.full_end, line.full_end),
        kind,
        start_line: line_number,
    })
}
