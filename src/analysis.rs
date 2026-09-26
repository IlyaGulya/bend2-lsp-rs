use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, DocumentSymbol, FoldingRange, FoldingRangeKind, InlayHint,
    InlayHintKind, InlayHintLabel, Location, ParameterInformation, ParameterLabel, Position, Range,
    SelectionRange, SemanticToken, SignatureHelp, SignatureInformation, SymbolInformation,
    SymbolKind,
};

struct Line<'a> {
    start: usize,
    end: usize,
    indent: usize,
    code: &'a str,
}

struct TopLevelSymbol {
    name: String,
    detail: String,
    kind: SymbolKind,
    start_line: usize,
    name_start: usize,
    name_end: usize,
}

pub struct Import {
    pub path: String,
    pub alias: Option<String>,
    pub path_range: Range,
    pub alias_range: Option<Range>,
}

pub fn imports(source: &str) -> Vec<Import> {
    let mut imports = Vec::new();
    for line in lines(source) {
        let code = line.code;
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
        let path = &code[path_start..path_end];
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
                    .then_some((rest[..end].to_owned(), code.len() - rest.len()))
            });
        let base = line.start + line.indent;
        imports.push(Import {
            path: path.to_owned(),
            alias: alias.as_ref().map(|(name, _)| name.clone()),
            path_range: Range::new(
                position_at(source, base + path_start),
                position_at(source, base + path_end),
            ),
            alias_range: alias.map(|(name, start)| {
                Range::new(
                    position_at(source, base + start),
                    position_at(source, base + start + name.len()),
                )
            }),
        });
    }
    imports
}

pub fn inlay_hints(source: &str, range: Range) -> Vec<InlayHint> {
    let declarations: Vec<TopLevelSymbol> = lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .collect();
    let mut hints = Vec::new();
    for declaration in declarations
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::FUNCTION)
    {
        let Some(open) = declaration.detail.find('(') else {
            continue;
        };
        let Some(close) = declaration.detail.rfind(')') else {
            continue;
        };
        let parameter_names: Vec<String> = split_parameters(&declaration.detail[open + 1..close])
            .into_iter()
            .filter_map(|parameter| {
                parameter
                    .split(':')
                    .next()
                    .and_then(|name| name.split_whitespace().last())
                    .map(str::to_owned)
            })
            .collect();
        if parameter_names.is_empty() {
            continue;
        }
        let declaration_range = Range::new(
            position_at(source, declaration.name_start),
            position_at(source, declaration.name_end),
        );
        for name_range in identifier_ranges(source, &declaration.name) {
            if name_range == declaration_range {
                continue;
            }
            let mut call_open = offset_at(source, name_range.end);
            while call_open < source.len() && source.as_bytes()[call_open].is_ascii_whitespace() {
                call_open += 1;
            }
            if source.as_bytes().get(call_open) != Some(&b'(') {
                continue;
            }
            for (index, argument_start) in call_argument_starts(source, call_open)
                .into_iter()
                .enumerate()
            {
                let Some(parameter) = parameter_names.get(index) else {
                    break;
                };
                let position = position_at(source, argument_start);
                if !position_in_range(position, range) {
                    continue;
                }
                hints.push(InlayHint {
                    position,
                    label: InlayHintLabel::String(format!("{parameter}: ")),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: None,
                    padding_right: Some(true),
                    data: None,
                });
            }
        }
    }
    hints.sort_by_key(|hint| (hint.position.line, hint.position.character));
    hints
}

fn call_argument_starts(source: &str, open: usize) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = Vec::new();
    let mut index = open + 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) == Some(&b')') {
        return starts;
    }
    let mut start = index;
    let mut depth = 0_u32;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(current) = quote {
            index += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == current {
                quote = None;
            }
            continue;
        }
        if byte == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
        } else if matches!(byte, b'(' | b'[' | b'{') {
            depth += 1;
        } else if matches!(byte, b')' | b']' | b'}') {
            if depth == 0 && byte == b')' {
                if start < index {
                    starts.push(start);
                }
                break;
            }
            depth = depth.saturating_sub(1);
        } else if byte == b',' && depth == 0 {
            if start < index {
                starts.push(start);
            }
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            start = index;
            continue;
        }
        index += if byte.is_ascii() {
            1
        } else {
            source[index..].chars().next().unwrap().len_utf8()
        };
    }
    starts
}

fn position_in_range(position: Position, range: Range) -> bool {
    let after_start = position.line > range.start.line
        || position.line == range.start.line && position.character >= range.start.character;
    let before_end = position.line < range.end.line
        || position.line == range.end.line && position.character <= range.end.character;
    after_start && before_end
}

pub fn folding_ranges(source: &str) -> Vec<FoldingRange> {
    let lines = lines(source);
    let mut ranges = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.code.trim().is_empty() {
            continue;
        }
        let mut end = None;
        for (child_index, child) in lines.iter().enumerate().skip(index + 1) {
            if child.code.trim().is_empty() {
                continue;
            }
            if child.indent <= line.indent {
                break;
            }
            end = Some(child_index);
        }
        if let Some(end_line) = end {
            ranges.push(FoldingRange {
                start_line: index as u32,
                start_character: None,
                end_line: end_line as u32,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }
    ranges
}

pub fn selection_range(source: &str, position: Position) -> SelectionRange {
    let offset = offset_at(source, position);
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    let document = SelectionRange {
        range: Range::new(Position::new(0, 0), position_at(source, source.len())),
        parent: None,
    };
    let line = SelectionRange {
        range: Range::new(
            position_at(source, line_start),
            position_at(source, line_end),
        ),
        parent: Some(Box::new(document)),
    };
    let bytes = source.as_bytes();
    let mut cursor = offset.min(source.len());
    while !source.is_char_boundary(cursor) {
        cursor -= 1;
    }
    if cursor == bytes.len() || !is_identifier_byte(bytes[cursor]) {
        if cursor == 0 || !is_identifier_byte(bytes[cursor - 1]) {
            return line;
        }
        cursor -= 1;
    }
    let mut start = cursor;
    let mut end = cursor;
    while start > 0 && is_identifier_byte(bytes[start - 1]) {
        start -= 1;
    }
    while end < bytes.len() && is_identifier_byte(bytes[end]) {
        end += 1;
    }
    SelectionRange {
        range: Range::new(position_at(source, start), position_at(source, end)),
        parent: Some(Box::new(line)),
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn offset_at(source: &str, position: Position) -> usize {
    let mut offset = 0;
    for (line_number, line) in source.split_inclusive('\n').enumerate() {
        if line_number == position.line as usize {
            let line = line.trim_end_matches(['\n', '\r']);
            let mut bytes = 0;
            let mut units = 0;
            for character in line.chars() {
                if units + character.len_utf16() > position.character as usize {
                    break;
                }
                units += character.len_utf16();
                bytes += character.len_utf8();
            }
            return (offset + bytes).min(source.len());
        }
        offset += line.len();
    }
    source.len()
}

pub fn semantic_tokens(source: &str) -> Vec<SemanticToken> {
    let declarations: Vec<(String, u32)> = lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .map(|symbol| {
            (
                symbol.name,
                if symbol.kind == SymbolKind::FUNCTION {
                    12
                } else {
                    1
                },
            )
        })
        .collect();
    let bytes = source.as_bytes();
    let mut raw = Vec::<(usize, usize, u32)>::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        let token_type = if byte == b'#' {
            index += 1;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            17
        } else if byte == b'\'' || byte == b'"' {
            let quote = byte;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() && bytes[index] != b'\n' {
                let current = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if current == b'\\' {
                    escaped = true;
                } else if current == quote {
                    break;
                }
            }
            18
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let word = &source[start..index];
            if [
                "def", "law", "type", "is", "match", "case", "do", "return", "for", "exs", "where",
                "import", "as",
            ]
            .contains(&word)
            {
                15
            } else if let Some((_, token_type)) = declarations.iter().find(|(name, _)| name == word)
            {
                *token_type
            } else if word.as_bytes()[0].is_ascii_uppercase() {
                1
            } else {
                8
            }
        } else if byte.is_ascii_digit() {
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            19
        } else if b"+-*/%=<>!&|^~".contains(&byte) {
            index += 1;
            while index < bytes.len() && b"+-*/%=<>!&|^~".contains(&bytes[index]) {
                index += 1;
            }
            20
        } else {
            index += if byte.is_ascii() {
                1
            } else {
                source[index..].chars().next().unwrap().len_utf8()
            };
            continue;
        };
        raw.push((start, index, token_type));
    }

    let mut tokens = Vec::with_capacity(raw.len());
    let mut previous = Position::new(0, 0);
    for (start, end, token_type) in raw {
        let start = position_at(source, start);
        let end = position_at(source, end);
        if start.line != end.line || start.character == end.character {
            continue;
        }
        tokens.push(SemanticToken {
            delta_line: start.line - previous.line,
            delta_start: if start.line == previous.line {
                start.character - previous.character
            } else {
                start.character
            },
            length: end.character - start.character,
            token_type,
            token_modifiers_bitset: 0,
        });
        previous = start;
    }
    tokens
}

pub fn signature_help(source: &str, offset: usize) -> Option<SignatureHelp> {
    let (name, active_parameter) = active_call(source, offset)?;
    let declaration = lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .find(|symbol| symbol.name == name && symbol.kind == SymbolKind::FUNCTION)?;
    let open = declaration.detail.find('(')?;
    let close = declaration.detail.rfind(')')?;
    let parameters: Vec<String> = split_parameters(&declaration.detail[open + 1..close])
        .into_iter()
        .map(str::to_owned)
        .collect();
    let active_parameter = active_parameter.min(parameters.len().saturating_sub(1) as u32);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label: declaration.detail,
            documentation: None,
            parameters: Some(
                parameters
                    .into_iter()
                    .map(|parameter| ParameterInformation {
                        label: ParameterLabel::Simple(parameter),
                        documentation: None,
                    })
                    .collect(),
            ),
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: Some(active_parameter),
    })
}

fn active_call(source: &str, offset: usize) -> Option<(String, u32)> {
    let bytes = source.as_bytes();
    let mut delimiters = Vec::<(u8, usize, u32)>::new();
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < offset.min(bytes.len()) {
        let byte = bytes[index];
        if let Some(current) = quote {
            index += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == current {
                quote = None;
            }
            continue;
        }
        if byte == b'#' {
            while index < offset && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
        } else if matches!(byte, b'(' | b'[' | b'{') {
            delimiters.push((byte, index, 0));
        } else if matches!(byte, b')' | b']' | b'}') {
            let expected = match byte {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            if delimiters.last().is_some_and(|entry| entry.0 == expected) {
                delimiters.pop();
            }
        } else if byte == b',' {
            if let Some((b'(', _, commas)) = delimiters.last_mut() {
                *commas += 1;
            }
        }
        index += if byte.is_ascii() {
            1
        } else {
            source[index..].chars().next()?.len_utf8()
        };
    }
    let (delimiter, open, commas) = *delimiters.last()?;
    if delimiter != b'(' {
        return None;
    }
    let mut end = open;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || b"_.".contains(&bytes[start - 1]))
    {
        start -= 1;
    }
    let name = source[start..end].rsplit('.').next()?.to_owned();
    (!name.is_empty()).then_some((name, commas))
}

fn split_parameters(parameters: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    for (index, character) in parameters.char_indices() {
        match character {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                let parameter = parameters[start..index].trim();
                if !parameter.is_empty() {
                    result.push(parameter);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let parameter = parameters[start..].trim();
    if !parameter.is_empty() {
        result.push(parameter);
    }
    result
}

pub fn completion_items(source: &str, prefix: &str) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .filter(|symbol| symbol.name.starts_with(prefix))
        .map(|symbol| {
            let mut item = CompletionItem::new_simple(symbol.name, symbol.detail);
            item.kind = Some(if symbol.kind == SymbolKind::STRUCT {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::FUNCTION
            });
            item
        })
        .collect();
    for keyword in [
        "def", "type", "law", "match", "case", "do", "return", "for", "exs", "where", "import",
        "as",
    ] {
        if keyword.starts_with(prefix) && !items.iter().any(|item| item.label == keyword) {
            let mut item = CompletionItem::new_simple(keyword.into(), "Bend keyword".into());
            item.kind = Some(CompletionItemKind::KEYWORD);
            items.push(item);
        }
    }
    items
}

pub fn module_completion_items(source: &str, prefix: &str) -> Vec<CompletionItem> {
    lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .filter(|symbol| symbol.name.starts_with(prefix))
        .map(|symbol| {
            let mut item = CompletionItem::new_simple(symbol.name, symbol.detail);
            item.kind = Some(if symbol.kind == SymbolKind::STRUCT {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::FUNCTION
            });
            item
        })
        .collect()
}

pub fn qualified_completion_items(
    source: &str,
    qualifier: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    let qualifier_prefix = format!("{qualifier}.");
    let requested_prefix = format!("{qualifier_prefix}{prefix}");
    lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .filter(|symbol| symbol.name.starts_with(&requested_prefix))
        .map(|symbol| {
            let label = symbol
                .name
                .strip_prefix(&qualifier_prefix)
                .unwrap_or(&symbol.name)
                .to_owned();
            let mut item = CompletionItem::new_simple(label, symbol.detail);
            item.kind = Some(if symbol.kind == SymbolKind::STRUCT {
                CompletionItemKind::STRUCT
            } else {
                CompletionItemKind::FUNCTION
            });
            item
        })
        .collect()
}

pub fn identifier_ranges(source: &str, name: &str) -> Vec<Range> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'#' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            quote @ (b'\'' | b'"') => {
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == quote {
                        break;
                    }
                }
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                if &source[start..index] == name {
                    ranges.push(Range::new(
                        position_at(source, start),
                        position_at(source, index),
                    ));
                }
            }
            _ => {
                let width = source[index..].chars().next().unwrap().len_utf8();
                index += width;
            }
        }
    }
    ranges
}

#[allow(deprecated)]
pub fn workspace_symbols(
    source: &str,
    uri: &tower_lsp::lsp_types::Url,
    query: &str,
) -> Vec<SymbolInformation> {
    let lines = lines(source);
    let query = query.to_lowercase();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .filter(|symbol| symbol.name.to_lowercase().contains(&query))
        .map(|symbol| SymbolInformation {
            name: symbol.name,
            kind: symbol.kind,
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: Range::new(
                    position_at(source, symbol.name_start),
                    position_at(source, symbol.name_end),
                ),
            },
            container_name: None,
        })
        .collect()
}

pub fn declaration_range(source: &str, name: &str) -> Option<Range> {
    let lines = lines(source);
    let declaration = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .find(|symbol| symbol.name == name)?;
    Some(Range::new(
        position_at(source, declaration.name_start),
        position_at(source, declaration.name_end),
    ))
}

pub fn type_declaration_range(source: &str, name: &str) -> Option<Range> {
    let lines = lines(source);
    let declaration = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .find(|symbol| symbol.kind == SymbolKind::STRUCT && symbol.name == name)?;
    Some(Range::new(
        position_at(source, declaration.name_start),
        position_at(source, declaration.name_end),
    ))
}

pub fn parameter_type(source: &str, parameter_name: &str) -> Option<String> {
    for (index, line) in lines(source).iter().enumerate() {
        if line.indent != 0 {
            continue;
        }
        let Some(declaration) = top_level_symbol(index, line) else {
            continue;
        };
        if declaration.kind != SymbolKind::FUNCTION {
            continue;
        }
        let Some(open) = declaration.detail.find('(') else {
            continue;
        };
        let Some(close) = declaration.detail.rfind(')') else {
            continue;
        };
        for parameter in split_parameters(&declaration.detail[open + 1..close]) {
            let Some((binding, ty)) = parameter.split_once(':') else {
                continue;
            };
            if binding.split_whitespace().last() == Some(parameter_name) {
                return Some(ty.trim().to_owned());
            }
        }
    }
    None
}

pub fn parameter_type_declaration_range(source: &str, parameter_name: &str) -> Option<Range> {
    let type_expression = parameter_type(source, parameter_name)?;
    let lines = lines(source);
    let declarations: Vec<TopLevelSymbol> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .filter(|symbol| symbol.kind == SymbolKind::STRUCT)
        .collect();
    let declaration = declarations.into_iter().find(|symbol| {
        type_expression
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|name| name == symbol.name)
    })?;
    Some(Range::new(
        position_at(source, declaration.name_start),
        position_at(source, declaration.name_end),
    ))
}

pub fn declaration_hover(source: &str, name: &str) -> Option<String> {
    let declaration = lines(source)
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .find(|symbol| symbol.name == name);
    if let Some(declaration) = declaration {
        return Some(format!("```bend\n{}\n```", declaration.detail));
    }
    let ty = parameter_type(source, name)?;
    Some(format!("```bend\n{name}: {ty}\n```"))
}

#[allow(deprecated)]
pub fn document_symbols(source: &str) -> Vec<DocumentSymbol> {
    let lines = lines(source);
    let declarations: Vec<TopLevelSymbol> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.indent == 0)
        .filter_map(|(index, line)| top_level_symbol(index, line))
        .collect();

    declarations
        .iter()
        .enumerate()
        .map(|(index, declaration)| {
            let end_line = declarations
                .get(index + 1)
                .map_or(source.len(), |next| lines[next.start_line].start);
            let children = if declaration.kind == SymbolKind::STRUCT {
                constructor_symbols(source, &lines, declaration, end_line)
            } else {
                Vec::new()
            };
            DocumentSymbol {
                name: declaration.name.clone(),
                detail: Some(declaration.detail.clone()),
                kind: declaration.kind,
                tags: None,
                deprecated: None,
                range: Range::new(
                    position_at(source, lines[declaration.start_line].start),
                    position_at(source, end_line),
                ),
                selection_range: Range::new(
                    position_at(source, declaration.name_start),
                    position_at(source, declaration.name_end),
                ),
                children: (!children.is_empty()).then_some(children),
            }
        })
        .collect()
}

fn lines(source: &str) -> Vec<Line<'_>> {
    let mut start = 0;
    source
        .split_inclusive('\n')
        .map(|raw| {
            let end = start + raw.len();
            let text = raw.trim_end_matches(['\r', '\n']);
            let indent = text
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let line = Line {
                start,
                end,
                indent,
                code: &text[indent..],
            };
            start = end;
            line
        })
        .collect()
}

fn top_level_symbol(index: usize, line: &Line<'_>) -> Option<TopLevelSymbol> {
    let mut code = line.code;
    if let Some(rest) = code.strip_prefix("@unsafe") {
        code = rest.trim_start();
    }
    let (keyword, remainder) = code.split_once(char::is_whitespace)?;
    let kind = match keyword {
        "def" | "law" => SymbolKind::FUNCTION,
        "type" => SymbolKind::STRUCT,
        _ => return None,
    };
    let remainder = remainder.trim_start();
    let name_len = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        .count();
    if name_len == 0
        || !remainder.as_bytes()[0].is_ascii_alphabetic() && remainder.as_bytes()[0] != b'_'
    {
        return None;
    }
    let prefix_len = line.code.len() - code.len();
    let after_keyword = &code[keyword.len()..];
    let name_offset =
        prefix_len + keyword.len() + after_keyword.len() - after_keyword.trim_start().len();
    let name = &remainder[..name_len];
    Some(TopLevelSymbol {
        name: name.to_owned(),
        detail: line
            .code
            .trim()
            .strip_suffix(':')
            .unwrap_or(line.code.trim())
            .to_owned(),
        kind,
        start_line: index,
        name_start: line.start + name_offset,
        name_end: line.start + name_offset + name_len,
    })
}

#[allow(deprecated)]
fn constructor_symbols(
    source: &str,
    lines: &[Line<'_>],
    parent: &TopLevelSymbol,
    end_offset: usize,
) -> Vec<DocumentSymbol> {
    let first_body_line = parent.start_line + 1;
    let body_indent = lines[first_body_line..]
        .iter()
        .take_while(|line| line.start < end_offset)
        .filter(|line| {
            line.indent > 0
                && !line.code.trim().is_empty()
                && !line.code.trim_start().starts_with('#')
        })
        .map(|line| line.indent)
        .min();
    let Some(body_indent) = body_indent else {
        return Vec::new();
    };
    lines[first_body_line..]
        .iter()
        .filter(|line| line.start < end_offset && line.indent == body_indent)
        .filter_map(|line| {
            let (name, name_start, name_end) = constructor_name(line)?;
            let range = Range::new(
                position_at(source, line.start),
                position_at(source, line.end),
            );
            let selection_range = Range::new(
                position_at(source, name_start),
                position_at(source, name_end),
            );
            Some(DocumentSymbol {
                name,
                detail: Some(format!("constructor of {}", parent.name)),
                kind: SymbolKind::ENUM_MEMBER,
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: None,
            })
        })
        .collect()
}

fn constructor_name(line: &Line<'_>) -> Option<(String, usize, usize)> {
    let code = line.code;
    if code.trim_start().starts_with('#') {
        return None;
    }
    let name_len = code
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
        .count();
    if name_len == 0 || !code.as_bytes()[0].is_ascii_alphabetic() && code.as_bytes()[0] != b'_' {
        return None;
    }
    let remainder = code[name_len..].trim_start();
    if !remainder.is_empty() && !remainder.starts_with('{') {
        return None;
    }
    let name_start = line.start + line.indent;
    Some((
        code[..name_len].to_owned(),
        name_start,
        name_start + name_len,
    ))
}

fn position_at(source: &str, offset: usize) -> Position {
    let offset = offset.min(source.len());
    let mut boundary = offset;
    while !source.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let prefix = &source[..boundary];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let character = prefix
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count() as u32;
    Position::new(line, character)
}
