#[path = "analysis/syntax.rs"]
mod syntax;

pub use syntax::{
    CallSite, CasePatternType, DiagnosticKind, IndexedBinding, IndexedConstructor, IndexedImport,
    IndexedSymbol, NameId, Reference, ReferenceKind, SymbolId, SyntaxDiagnostic, SyntaxIndex,
    Token, TokenFlags, TokenId, TokenKind,
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
    Variable,
    Constructor,
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

/// Exact, prefix, and subsequence matching without allocating candidate strings.
/// Lower tuples rank first; ties are resolved by scope and then label by the caller.
#[must_use]
pub fn completion_match(label: &str, query: &str) -> Option<(u8, usize, usize)> {
    if label == query {
        return Some((0, 0, label.len()));
    }
    if label.starts_with(query) {
        return Some((1, 0, label.len()));
    }
    subsequence_completion_match(label, query)
}

fn completion_matches(label: &str, query: &str, ascii: bool) -> bool {
    if label.len() < query.len() {
        return false;
    }
    label.starts_with(query)
        || if ascii {
            ascii_subsequence_matches(label, query)
        } else {
            subsequence_completion_match(label, query).is_some()
        }
}

fn ascii_subsequence_matches(label: &str, query: &str) -> bool {
    let mut wanted = query.bytes();
    let Some(mut next) = wanted.next() else {
        return true;
    };
    for character in label.bytes() {
        if character.eq_ignore_ascii_case(&next) {
            let Some(remaining) = wanted.next() else {
                return true;
            };
            next = remaining;
        }
    }
    false
}

fn subsequence_completion_match(label: &str, query: &str) -> Option<(u8, usize, usize)> {
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut first = 0;
    let mut matched = 0;
    for (index, character) in label.chars().enumerate() {
        if next.is_some_and(|target| character.eq_ignore_ascii_case(&target)) {
            if matched == 0 {
                first = index;
            }
            matched += 1;
            next = wanted.next();
            if next.is_none() {
                return Some((
                    if first == 0 && index + 1 == matched {
                        2
                    } else {
                        3
                    },
                    first + index + 1 - matched,
                    label.len(),
                ));
            }
        }
    }
    None
}

#[must_use]
pub fn completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    completion_items_with_scope(snapshot, prefix, None)
}

/// Complete visible local bindings and declarations at a snapshot-local cursor.
#[must_use]
pub fn scoped_completion_items(
    snapshot: &DocumentSnapshot,
    offset: usize,
    prefix: &str,
) -> Vec<Completion> {
    completion_items_with_scope(snapshot, prefix, Some(offset))
}

fn completion_items_with_scope(
    snapshot: &DocumentSnapshot,
    prefix: &str,
    offset: Option<usize>,
) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let ascii_prefix = prefix.is_ascii();
    let mut items = Vec::new();
    let local_names = offset.map(|offset| {
        let mut local_names = std::collections::HashSet::new();
        for binding in syntax.bindings_at(offset) {
            let name = syntax.name_text(source, binding.name);
            if completion_matches(name, prefix, ascii_prefix) && local_names.insert(binding.name) {
                items.push(Completion {
                    label: name.to_owned(),
                    detail: "Local binding".into(),
                    kind: CompletionKind::Variable,
                });
            }
        }
        items.sort_unstable_by(|left, right| left.label.cmp(&right.label));
        local_names
    });
    for symbol in syntax.symbols() {
        let name = syntax.name_text(source, symbol.name);
        if completion_matches(name, prefix, ascii_prefix)
            && local_names
                .as_ref()
                .is_none_or(|names| !names.contains(&symbol.name))
        {
            items.push(completion(
                name.to_owned(),
                source[symbol.detail_range.start..symbol.detail_range.end].to_owned(),
                symbol.kind,
            ));
        }
    }
    for keyword in [
        "def", "type", "law", "match", "case", "do", "return", "for", "exs", "where", "import",
        "as",
    ] {
        if completion_matches(keyword, prefix, ascii_prefix)
            && !items.iter().any(|item| item.label == keyword)
        {
            items.push(Completion {
                label: keyword.into(),
                detail: "Bend keyword".into(),
                kind: CompletionKind::Keyword,
            });
        }
    }
    items
}

/// Complete indexed ADT constructors, optionally within a qualified namespace.
#[must_use]
pub fn constructor_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: Option<&str>,
    prefix: &str,
) -> Vec<Completion> {
    typed_constructor_completion_items(snapshot, qualifier, prefix, None)
}

/// Restrict constructors only after the caller resolves a known explicit type.
#[must_use]
pub fn typed_constructor_completion_items(
    snapshot: &DocumentSnapshot,
    qualifier: Option<&str>,
    prefix: &str,
    expected_type: Option<&str>,
) -> Vec<Completion> {
    let syntax = &snapshot.syntax;
    let source = &snapshot.text;
    let ascii_prefix = prefix.is_ascii();
    let mut items = Vec::new();
    for parent in syntax.symbols() {
        if parent.kind != SymbolKind::Struct
            || expected_type
                .is_some_and(|expected| syntax.name_text(source, parent.name) != expected)
        {
            continue;
        }
        for constructor in syntax.constructors(parent.id) {
            let name = syntax.name_text(source, constructor.name);
            let label = match qualifier {
                Some(qualifier) => name
                    .strip_prefix(qualifier)
                    .and_then(|name| name.strip_prefix('.')),
                None => Some(name),
            };
            if let Some(label) =
                label.filter(|label| completion_matches(label, prefix, ascii_prefix))
            {
                let parent_name = syntax.name_text(source, parent.name);
                let mut detail = String::with_capacity("constructor of ".len() + parent_name.len());
                detail.push_str("constructor of ");
                detail.push_str(parent_name);
                items.push(Completion {
                    label: label.to_owned(),
                    detail,
                    kind: CompletionKind::Constructor,
                });
            }
        }
    }
    items
}

#[must_use]
pub fn module_completion_items(snapshot: &DocumentSnapshot, prefix: &str) -> Vec<Completion> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let ascii_prefix = prefix.is_ascii();
    syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            completion_matches(name, prefix, ascii_prefix).then(|| {
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
    let ascii_prefix = prefix.is_ascii();
    syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let name = syntax.name_text(source, symbol.name);
            let label = name.strip_prefix(qualifier)?.strip_prefix('.')?;
            completion_matches(label, prefix, ascii_prefix).then(|| {
                let label = label.to_owned();
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
    syntax
        .symbol_by_name(name)
        .map(|symbol| symbol.name_range)
        .or_else(|| {
            syntax
                .constructor_by_name(name)
                .map(|constructor| constructor.name_range)
        })
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
pub fn declaration_hover(snapshot: &DocumentSnapshot, name: &str) -> Option<String> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    if let Some(name_id) = syntax.name_id(source, name)
        && let Some(declaration) = syntax.symbol_by_name(name_id)
    {
        let detail = &source[declaration.detail_range.start..declaration.detail_range.end];
        return Some(format!("```bend\n{detail}\n```"));
    }
    None
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
                constructor_symbols(snapshot, declaration)
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
        constructor_symbols(snapshot, declaration)
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

fn constructor_symbols(snapshot: &DocumentSnapshot, parent: &IndexedSymbol) -> Vec<Symbol> {
    let source = &snapshot.text;
    let syntax = &snapshot.syntax;
    let parent_name = syntax.name_text(source, parent.name);
    syntax
        .constructors(parent.id)
        .iter()
        .map(|constructor| Symbol {
            name: syntax.name_text(source, constructor.name).to_owned(),
            detail: format!("constructor of {parent_name}"),
            kind: SymbolKind::Constructor,
            range: constructor.range,
            selection_range: constructor.name_range,
            children: Vec::new(),
        })
        .collect()
}
