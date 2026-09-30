#[path = "analysis/syntax.rs"]
mod syntax;

pub use syntax::{
    CallSite, DiagnosticKind, IndexedBinding, IndexedImport, IndexedSymbol, NameId, Reference,
    ReferenceKind, SymbolId, SyntaxDiagnostic, SyntaxIndex, Token, TokenFlags, TokenId, TokenKind,
};

pub type Import = IndexedImport;
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Revision(pub i32);

impl Revision {
    pub const UNVERSIONED: Self = Self(-1);
}

const UTF16_CHECKPOINT_STRIDE: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Utf16Checkpoint {
    byte_offset: usize,
    units: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineIndex {
    line_starts: Vec<usize>,
    ascii_lines: Vec<u64>,
    checkpoints: Vec<Utf16Checkpoint>,
}

impl LineIndex {
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        let mut ascii_lines = vec![u64::MAX];
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset + 1);
                if (line_starts.len() - 1) % u64::BITS as usize == 0 {
                    ascii_lines.push(u64::MAX);
                }
            } else if !byte.is_ascii() {
                let line = line_starts.len() - 1;
                ascii_lines[line / u64::BITS as usize] &= !(1_u64 << (line % u64::BITS as usize));
            }
        }
        Self::from_parts(source, line_starts, ascii_lines)
    }

    fn from_parts(source: &str, line_starts: Vec<usize>, ascii_lines: Vec<u64>) -> Self {
        let mut index = Self {
            line_starts,
            ascii_lines,
            checkpoints: Vec::new(),
        };
        for line in 0..index.line_starts.len() {
            if index.is_ascii_line(line) {
                continue;
            }
            let start = index.line_starts[line];
            let end = index.line_content_end(source, line);
            let mut units = 0;
            for (count, (offset, scalar)) in source[start..end].char_indices().enumerate() {
                units += scalar.len_utf16();
                if (count + 1) % UTF16_CHECKPOINT_STRIDE == 0 {
                    index.checkpoints.push(Utf16Checkpoint {
                        byte_offset: start + offset + scalar.len_utf8(),
                        units,
                    });
                }
            }
        }
        index
    }

    #[must_use]
    pub fn offset(&self, source: &str, line: u32, character: u32) -> usize {
        let Ok(line) = usize::try_from(line) else {
            return source.len();
        };
        let Some(start) = self.line_starts.get(line).copied() else {
            return source.len();
        };
        let end = self.line_content_end(source, line);
        let requested = usize::try_from(character).unwrap_or(usize::MAX);
        let content = &source[start..end];
        let mut units = 0usize;
        for (offset, scalar) in content.char_indices() {
            let scalar_units = scalar.len_utf16();
            if units.saturating_add(scalar_units) > requested {
                return start + offset;
            }
            units += scalar_units;
        }
        end
    }

    #[must_use]
    pub fn position(&self, source: &str, offset: usize) -> (u32, u32) {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let start = self.line_starts[line];
        let end = self.line_content_end(source, line);
        let offset = offset.min(end);
        let character = if self.is_ascii_line(line) {
            offset - start
        } else {
            let before = self
                .checkpoints
                .partition_point(|checkpoint| checkpoint.byte_offset <= offset);
            let checkpoint = before
                .checked_sub(1)
                .and_then(|index| self.checkpoints.get(index))
                .filter(|checkpoint| checkpoint.byte_offset >= start);
            let (checkpoint_offset, units) = checkpoint.map_or((start, 0), |checkpoint| {
                (checkpoint.byte_offset, checkpoint.units)
            });
            units + source[checkpoint_offset..offset].encode_utf16().count()
        };
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(character).unwrap_or(u32::MAX),
        )
    }

    #[must_use]
    pub fn line_range(&self, source: &str, line: usize) -> Option<TextRange> {
        let start = *self.line_starts.get(line)?;
        Some(TextRange::new(start, self.line_content_end(source, line)))
    }

    fn is_ascii_line(&self, line: usize) -> bool {
        self.ascii_lines
            .get(line / u64::BITS as usize)
            .is_some_and(|word| word & (1_u64 << (line % u64::BITS as usize)) != 0)
    }

    fn line_content_end(&self, source: &str, line: usize) -> usize {
        let start = self.line_starts[line];
        let mut end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(source.len());
        if end > start && source.as_bytes()[end - 1] == b'\n' {
            end -= 1;
            if end > start && source.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
        }
        end
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DocumentSnapshot {
    pub revision: Revision,
    pub text: String,
    pub line_index: LineIndex,
    pub syntax: SyntaxIndex,
}

impl DocumentSnapshot {
    #[must_use]
    pub fn new(revision: Revision, text: String) -> Self {
        let (line_index, syntax) = SyntaxIndex::build(&text);
        Self {
            revision,
            text,
            line_index,
            syntax,
        }
    }

    #[must_use]
    pub fn with_line_index(revision: Revision, text: String, line_index: LineIndex) -> Self {
        let syntax = SyntaxIndex::build_with_line_index(&text, &line_index);
        Self {
            revision,
            text,
            line_index,
            syntax,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolKind {
    Function,
    Struct,
    Constructor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub range: TextRange,
    pub selection_range: TextRange,
    pub children: Vec<Self>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionKind {
    Function,
    Struct,
    Keyword,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    pub kind: CompletionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InlayHint {
    pub position: usize,
    pub parameter_name: TextRange,
}

impl InlayHint {
    #[must_use]
    pub fn label(&self, source: &str) -> String {
        let name = &source[self.parameter_name.start..self.parameter_name.end];
        let mut label = String::with_capacity(name.len() + 2);
        label.push_str(name);
        label.push_str(": ");
        label
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldingRange {
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub range: TextRange,
    pub parent: Option<Box<Self>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticToken {
    pub range: TextRange,
    pub token_type: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureHelp {
    pub label: String,
    pub parameters: Vec<String>,
    pub active_parameter: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: TextRange,
}

#[must_use]
pub fn imports(snapshot: &DocumentSnapshot) -> &[Import] {
    snapshot.syntax.imports()
}

#[must_use]
pub fn inlay_hints(snapshot: &DocumentSnapshot, range: TextRange) -> Vec<InlayHint> {
    let syntax = &snapshot.syntax;
    let mut hints = Vec::new();
    for declaration in syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function)
    {
        let parameters = syntax.parameters(declaration);
        if parameters.is_empty() {
            continue;
        }
        for call in syntax.calls_to(declaration.id) {
            for (index, argument) in syntax.call_arguments(call).iter().enumerate() {
                let Some(parameter) = parameters.get(index) else {
                    break;
                };
                if argument.start < range.start || argument.start > range.end {
                    continue;
                }
                hints.push(InlayHint {
                    position: argument.start,
                    parameter_name: parameter.name_range,
                });
            }
        }
    }
    hints.sort_by_key(|hint| hint.position);
    hints
}

#[must_use]
pub fn folding_ranges(snapshot: &DocumentSnapshot) -> Vec<FoldingRange> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let mut ranges = vec![None; syntax.line_count()];
    let mut active = Vec::<(usize, usize)>::new();
    let mut last_code_line = None;
    let mut range_count = 0;

    for index in 0..syntax.line_count() {
        let code_range = syntax.line_code_range(index).unwrap_or_default();
        if source[code_range.start..code_range.end].trim().is_empty() {
            continue;
        }
        let indent = syntax.line_indent(index).unwrap_or_default();
        while active
            .last()
            .is_some_and(|(_, parent_indent)| indent <= *parent_indent)
        {
            let Some((start_line, _)) = active.pop() else {
                break;
            };
            if let Some(end_line) = last_code_line.filter(|end_line| *end_line > start_line) {
                ranges[start_line] = Some(FoldingRange {
                    start_line,
                    end_line,
                });
                range_count += 1;
            }
        }
        active.push((index, indent));
        last_code_line = Some(index);
    }

    while let Some((start_line, _)) = active.pop() {
        if let Some(end_line) = last_code_line.filter(|end_line| *end_line > start_line) {
            ranges[start_line] = Some(FoldingRange {
                start_line,
                end_line,
            });
            range_count += 1;
        }
    }
    let mut output = Vec::with_capacity(range_count);
    output.extend(ranges.into_iter().flatten());
    output
}

#[must_use]
pub fn selection_range(snapshot: &DocumentSnapshot, mut offset: usize) -> SelectionRange {
    let source = &snapshot.text;
    offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let line_number =
        usize::try_from(snapshot.line_index.position(source, offset).0).unwrap_or_default();
    let line = SelectionRange {
        range: snapshot
            .syntax
            .line_content_range(line_number)
            .unwrap_or_default(),
        parent: Some(Box::new(SelectionRange {
            range: TextRange::new(0, source.len()),
            parent: None,
        })),
    };
    let syntax = &snapshot.syntax;
    let Some(token_id) = syntax.token_at_or_before(offset) else {
        return line;
    };
    let Some(mut token) = syntax.token(token_id).copied() else {
        return line;
    };
    if token.range.start == offset
        && !matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
        && token_id.0 > 0
    {
        let previous_id = TokenId(token_id.0 - 1);
        if let Some(previous) = syntax.token(previous_id)
            && previous.range.end == offset
            && matches!(previous.kind, TokenKind::Identifier | TokenKind::Number)
        {
            token = *previous;
        }
    }
    if !matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
        || offset < token.range.start
        || offset > token.range.end
    {
        return line;
    }
    SelectionRange {
        range: token.range,
        parent: Some(Box::new(line)),
    }
}

#[must_use]
pub fn semantic_tokens(snapshot: &DocumentSnapshot) -> Vec<SemanticToken> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let mut result = Vec::new();
    for (index, token) in syntax.tokens().iter().enumerate() {
        let token_type = match token.kind {
            TokenKind::Comment => Some(17),
            TokenKind::StringLiteral => Some(18),
            TokenKind::Number => Some(19),
            TokenKind::Operator => Some(20),
            TokenKind::Identifier => {
                let token_id = TokenId(index);
                let name = syntax.name_for_token(token_id);
                let symbol = syntax.symbol_for_token(token_id);
                if name.is_some_and(|name| syntax.is_keyword(name)) {
                    Some(15)
                } else if let Some(symbol) = symbol.and_then(|id| syntax.symbol_by_id(id)) {
                    Some(if symbol.kind == SymbolKind::Function {
                        12
                    } else {
                        1
                    })
                } else if symbol.is_some_and(|id| syntax.binding_by_id(id).is_some()) {
                    Some(8)
                } else if name.is_some_and(|name| {
                    syntax
                        .name_text(source, name)
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_uppercase)
                }) {
                    Some(1)
                } else {
                    Some(8)
                }
            }
            TokenKind::Punctuation | TokenKind::Delimiter => None,
        };
        if let Some(token_type) = token_type {
            result.push(SemanticToken {
                range: token.range,
                token_type,
            });
        }
    }
    result
}

pub fn signature_help(snapshot: &DocumentSnapshot, offset: usize) -> Option<SignatureHelp> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let (_name, active_parameter, _open, callee) = syntax.active_call(source, offset)?;
    let declaration = syntax
        .symbol_for_token(callee)
        .and_then(|id| syntax.symbol_by_id(id))?;
    if declaration.kind != SymbolKind::Function {
        return None;
    }
    let detail = &source[declaration.detail_range.start..declaration.detail_range.end];
    let open = detail.find('(')?;
    let close = detail.rfind(')')?;
    let parameters: Vec<String> = split_parameters(&detail[open + 1..close])
        .into_iter()
        .map(str::to_owned)
        .collect();
    let active_parameter = usize::try_from(active_parameter)
        .unwrap_or(usize::MAX)
        .min(parameters.len().saturating_sub(1));
    Some(SignatureHelp {
        label: detail.to_owned(),
        parameters,
        active_parameter,
    })
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

#[must_use]
pub fn completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let mut items: Vec<Completion> = syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            name.starts_with(prefix).then(|| {
                completion(
                    name.to_owned(),
                    source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                    symbol.kind,
                )
            })
        })
        .collect();
    for keyword in [
        "def", "type", "law", "match", "case", "do", "return", "for", "exs", "where", "import",
        "as",
    ] {
        if keyword.starts_with(prefix) && !items.iter().any(|item| item.label == keyword) {
            items.push(Completion {
                label: keyword.into(),
                detail: "Bend keyword".into(),
                kind: CompletionKind::Keyword,
            });
        }
    }
    items
}

#[must_use]
pub fn module_completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            name.starts_with(prefix).then(|| {
                completion(
                    name.to_owned(),
                    source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                    symbol.kind,
                )
            })
        })
        .collect()
}

#[must_use]
pub fn qualified_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: &str,
    prefix: &str,
) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let qualifier_prefix = format!("{qualifier}.");
    let requested_prefix = format!("{qualifier_prefix}{prefix}");
    syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            name.starts_with(&requested_prefix).then(|| {
                let label = name
                    .strip_prefix(&qualifier_prefix)
                    .unwrap_or(name)
                    .to_owned();
                completion(
                    label,
                    source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                    symbol.kind,
                )
            })
        })
        .collect()
}

fn completion(label: String, detail: String, symbol_kind: SymbolKind) -> Completion {
    let kind = match symbol_kind {
        SymbolKind::Function | SymbolKind::Constructor => CompletionKind::Function,
        SymbolKind::Struct => CompletionKind::Struct,
    };
    Completion {
        label,
        detail,
        kind,
    }
}

#[must_use]
pub fn identifier_ranges(snapshot: &DocumentSnapshot, name: &str) -> Vec<TextRange> {
    let syntax = &snapshot.syntax;
    let Some(name) = syntax.name_id(&snapshot.text, name) else {
        return Vec::new();
    };
    if let Some(symbol) = syntax.symbol_by_name(name) {
        return syntax
            .references(symbol.id)
            .map(|reference| reference.range)
            .collect();
    }
    syntax
        .references_named(name)
        .map(|reference| reference.range)
        .collect()
}

#[must_use]
pub fn workspace_symbols(snapshot: &DocumentSnapshot, query: &str) -> Vec<WorkspaceSymbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let query = query.to_lowercase();
    syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            name.to_lowercase()
                .contains(&query)
                .then(|| WorkspaceSymbol {
                    name: name.to_owned(),
                    kind: symbol.kind,
                    range: symbol.name_range,
                })
        })
        .collect()
}

#[must_use]
pub fn declaration_range(snapshot: &DocumentSnapshot, name: &str) -> Option<TextRange> {
    let syntax = &snapshot.syntax;
    let name = syntax.name_id(&snapshot.text, name)?;
    Some(syntax.symbol_by_name(name)?.name_range)
}

#[must_use]
pub fn type_declaration_range(snapshot: &DocumentSnapshot, name: &str) -> Option<TextRange> {
    let source = &snapshot.text;
    snapshot
        .syntax
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Struct
                && snapshot.syntax.name_text(source, symbol.name) == name
        })
        .map(|symbol| symbol.name_range)
}

#[must_use]
pub fn parameter_type(snapshot: &DocumentSnapshot, parameter_name: &str) -> Option<String> {
    let source = &snapshot.text;
    for declaration in snapshot
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Function)
    {
        let detail = &source[declaration.detail_range.start..declaration.detail_range.end];
        let Some(open) = detail.find('(') else {
            continue;
        };
        let Some(close) = detail.rfind(')') else {
            continue;
        };
        for parameter in split_parameters(&detail[open + 1..close]) {
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

#[must_use]
pub fn parameter_type_declaration_range(
    snapshot: &DocumentSnapshot,
    parameter_name: &str,
) -> Option<TextRange> {
    let type_expression = parameter_type(snapshot, parameter_name)?;
    let source = &snapshot.text;
    snapshot
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Struct)
        .find(|symbol| {
            type_expression
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|name| name == snapshot.syntax.name_text(source, symbol.name))
        })
        .map(|symbol| symbol.name_range)
}

#[must_use]
pub fn declaration_hover(snapshot: &DocumentSnapshot, name: &str) -> Option<String> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    if let Some(name_id) = syntax.name_id(source, name)
        && let Some(declaration) = syntax.symbol_by_name(name_id)
    {
        let detail = &source[declaration.detail_range.start..declaration.detail_range.end];
        return Some(format!("```bend\n{detail}\n```"));
    }
    let ty = parameter_type(snapshot, name)?;
    Some(format!("```bend\n{name}: {ty}\n```"))
}

#[must_use]
pub fn document_symbols(snapshot: &DocumentSnapshot) -> Vec<Symbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    syntax
        .symbols()
        .iter()
        .enumerate()
        .map(|(index, declaration)| {
            let end = syntax
                .symbols()
                .get(index + 1)
                .and_then(|next| syntax.line_content_range(next.start_line))
                .map_or(source.len(), |line| line.start);
            let start = syntax
                .line_content_range(declaration.start_line)
                .map_or(0, |line| line.start);
            let children = if declaration.kind == SymbolKind::Struct {
                constructor_symbols(snapshot, declaration, end)
            } else {
                Vec::new()
            };
            Symbol {
                name: syntax.name_text(source, declaration.name).to_owned(),
                detail: source[declaration.detail_range.start..declaration.detail_range.end]
                    .to_owned(),
                kind: declaration.kind,
                range: TextRange::new(start, end),
                selection_range: declaration.name_range,
                children,
            }
        })
        .collect()
}

#[must_use]
pub fn document_symbol_by_id(snapshot: &DocumentSnapshot, id: SymbolId) -> Option<Symbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let declaration = syntax.symbol_by_id(id)?;
    let end = syntax
        .symbols()
        .get(id.0 + 1)
        .and_then(|next| syntax.line_content_range(next.start_line))
        .map_or(source.len(), |line| line.start);
    let start = syntax
        .line_content_range(declaration.start_line)
        .map_or(0, |line| line.start);
    let children = if declaration.kind == SymbolKind::Struct {
        constructor_symbols(snapshot, declaration, end)
    } else {
        Vec::new()
    };
    Some(Symbol {
        name: syntax.name_text(source, declaration.name).to_owned(),
        detail: source[declaration.detail_range.start..declaration.detail_range.end].to_owned(),
        kind: declaration.kind,
        range: TextRange::new(start, end),
        selection_range: declaration.name_range,
        children,
    })
}

fn constructor_symbols(
    snapshot: &DocumentSnapshot,
    parent: &IndexedSymbol,
    end_offset: usize,
) -> Vec<Symbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let first_body_line = parent.start_line + 1;
    let body_indent = (first_body_line..syntax.line_count())
        .take_while(|line| {
            syntax
                .line_content_range(*line)
                .is_some_and(|range| range.start < end_offset)
        })
        .filter_map(|line| {
            let indent = syntax.line_indent(line)?;
            let code_range = syntax.line_code_range(line)?;
            let code = &source[code_range.start..code_range.end];
            (indent > 0 && !code.trim().is_empty() && !code.trim_start().starts_with('#'))
                .then_some(indent)
        })
        .min();
    let Some(body_indent) = body_indent else {
        return Vec::new();
    };
    let parent_name = syntax.name_text(source, parent.name);
    (first_body_line..syntax.line_count())
        .take_while(|line| {
            syntax
                .line_content_range(*line)
                .is_some_and(|range| range.start < end_offset)
        })
        .filter(|line| syntax.line_indent(*line) == Some(body_indent))
        .filter_map(|line| {
            let code_range = syntax.line_code_range(line)?;
            let code = &source[code_range.start..code_range.end];
            if code.trim_start().starts_with('#') {
                return None;
            }
            let name_len = code
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
                .count();
            if name_len == 0
                || !code.as_bytes()[0].is_ascii_alphabetic() && code.as_bytes()[0] != b'_'
            {
                return None;
            }
            let remainder = code[name_len..].trim_start();
            if !remainder.is_empty() && !remainder.starts_with('{') {
                return None;
            }
            Some(Symbol {
                name: code[..name_len].to_owned(),
                detail: format!("constructor of {parent_name}"),
                kind: SymbolKind::Constructor,
                range: TextRange::new(
                    syntax.line_content_range(line)?.start,
                    syntax.line_full_end(line)?,
                ),
                selection_range: TextRange::new(code_range.start, code_range.start + name_len),
                children: Vec::new(),
            })
        })
        .collect()
}
