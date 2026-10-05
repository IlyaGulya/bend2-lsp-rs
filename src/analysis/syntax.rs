#[path = "syntax/builder.rs"]
mod builder;
#[path = "syntax/calls.rs"]
mod calls;
#[path = "syntax/declarations.rs"]
mod declarations;
#[path = "syntax/names.rs"]
mod names;
#[path = "syntax/references.rs"]
mod references;
#[path = "syntax/scanner.rs"]
mod scanner;

use super::{SymbolKind, TextRange};
use names::NameTable;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NameId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SymbolId(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Identifier,
    Number,
    StringLiteral,
    Comment,
    Operator,
    Delimiter,
    Punctuation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenFlags(u8);

impl TokenFlags {
    const ESCAPED: u8 = 1;
    const UNTERMINATED: u8 = 2;

    #[must_use]
    pub const fn escaped(self) -> bool {
        self.0 & Self::ESCAPED != 0
    }

    #[must_use]
    pub const fn unterminated(self) -> bool {
        self.0 & Self::UNTERMINATED != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    pub range: TextRange,
    pub kind: TokenKind,
    pub flags: TokenFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticKind {
    UnresolvedHole,
    UnterminatedString(char),
    UnmatchedDelimiter(char),
    UnclosedDelimiter(char),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxDiagnostic {
    pub range: TextRange,
    pub kind: DiagnosticKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedImport {
    pub path: TextRange,
    pub alias: Option<TextRange>,
}

impl IndexedImport {
    #[must_use]
    pub fn path_text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.path.start..self.path.end]
    }

    #[must_use]
    pub fn alias_text<'a>(&self, source: &'a str) -> Option<&'a str> {
        let range = self.alias?;
        Some(&source[range.start..range.end])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedSymbol {
    pub id: SymbolId,
    pub name: NameId,
    pub name_range: TextRange,
    pub detail_range: TextRange,
    pub parameter_span: TextRange,
    pub scope: TextRange,
    pub kind: SymbolKind,
    pub start_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedConstructor {
    pub parent: SymbolId,
    pub name: NameId,
    pub name_range: TextRange,
    pub range: TextRange,
}

#[derive(Debug, Eq, PartialEq)]
struct ConstructorIndex {
    entries: Box<[IndexedConstructor]>,
    // Empty when entries are already ordered by NameId; otherwise a sorted permutation.
    name_indices: Box<[usize]>,
}

// Keep rare metadata out of the inline snapshot while retaining cheap empty-diagnostic checks.
#[derive(Debug, Eq, PartialEq)]
struct AuxiliaryIndex {
    data: Option<Box<AuxiliaryData>>,
    diagnostic_count: usize,
}

impl AuxiliaryIndex {
    fn new(
        diagnostics: Vec<SyntaxDiagnostic>,
        constructor_index: Option<ConstructorIndex>,
    ) -> Self {
        let diagnostic_count = diagnostics.len();
        Self {
            diagnostic_count,
            data: (diagnostic_count > 0 || constructor_index.is_some()).then(|| {
                Box::new(AuxiliaryData {
                    diagnostics: diagnostics.into_boxed_slice(),
                    constructors: constructor_index,
                })
            }),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct AuxiliaryData {
    diagnostics: Box<[SyntaxDiagnostic]>,
    constructors: Option<ConstructorIndex>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedBinding {
    pub id: SymbolId,
    pub name: NameId,
    pub name_range: TextRange,
    pub scope: TextRange,
    pub owner: SymbolId,
    pub declaration: TokenId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceKind {
    Declaration,
    Read,
    Call,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reference {
    pub name: NameId,
    pub qualifier: Option<NameId>,
    pub qualifier_token: Option<TokenId>,
    pub token: TokenId,
    pub range: TextRange,
    pub enclosing: Option<SymbolId>,
    pub kind: ReferenceKind,
    pub resolved: Option<SymbolId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallSite {
    pub caller: Option<SymbolId>,
    pub callee: Option<SymbolId>,
    pub name: NameId,
    pub qualifier: Option<NameId>,
    pub qualifier_token: Option<TokenId>,
    pub callee_token: TokenId,
    pub callee_range: TextRange,
    pub call_range: TextRange,
    pub argument_range: TextRange,
    argument_indices: TextRange,
    separator_indices: TextRange,
    pub open: TokenId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DelimiterPair {
    open: TokenId,
    close: TokenId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SyntaxLine {
    content: TextRange,
    full_end: usize,
    code: TextRange,
    indent: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SyntaxIndex {
    tokens: Box<[Token]>,
    semantic_token_count: usize,
    names: NameTable,
    keyword_names: Box<[bool]>,
    imports: Box<[IndexedImport]>,
    symbols: Box<[IndexedSymbol]>,
    bindings: Box<[IndexedBinding]>,
    references: Box<[Reference]>,
    reference_indices: Box<[usize]>,
    reference_spans: Box<[TextRange]>,
    name_reference_indices: Box<[usize]>,
    name_reference_spans: Box<[TextRange]>,
    token_references: Box<[Option<usize>]>,
    calls: Box<[CallSite]>,
    call_arguments: Box<[TextRange]>,
    call_separators: Box<[usize]>,
    calls_from_indices: Box<[usize]>,
    calls_from_spans: Box<[TextRange]>,
    calls_to_indices: Box<[usize]>,
    calls_to_spans: Box<[TextRange]>,
    call_by_open: Box<[Option<usize>]>,
    call_by_name_token: Box<[Option<usize>]>,
    lines: Box<[SyntaxLine]>,
    auxiliary: AuxiliaryIndex,
    delimiter_pairs: Box<[DelimiterPair]>,
    delimiter_context: Box<[Option<TokenId>]>,
    symbol_by_name: Box<[Option<SymbolId>]>,
    token_symbols: Box<[Option<SymbolId>]>,
}

impl SyntaxIndex {
    #[must_use]
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    #[must_use]
    pub(crate) const fn semantic_token_count(&self) -> usize {
        self.semantic_token_count
    }

    #[must_use]
    pub fn imports(&self) -> &[IndexedImport] {
        &self.imports
    }

    #[must_use]
    pub fn symbols(&self) -> &[IndexedSymbol] {
        &self.symbols
    }

    #[must_use]
    pub fn parameters(&self, symbol: &IndexedSymbol) -> &[IndexedBinding] {
        &self.bindings[symbol.parameter_span.start..symbol.parameter_span.end]
    }

    #[must_use]
    pub fn binding_type_range(&self, source: &str, id: SymbolId) -> Option<TextRange> {
        let binding = self.binding_by_id(id)?;
        let owner = self.symbols.get(binding.owner.0)?;
        let binding_index = id.0.checked_sub(self.symbols.len())?;
        if !owner.parameter_span.contains(binding_index)
            || self.token_text(source, TokenId(binding.declaration.0 + 1)) != Some(":")
        {
            return None;
        }
        let open = self
            .delimiter_context
            .get(binding.declaration.0)
            .copied()
            .flatten()?;
        let close = self
            .delimiter_pairs
            .binary_search_by_key(&open, |pair| pair.open)
            .map_or_else(
                |_| {
                    self.tokens
                        .partition_point(|token| token.range.start < owner.detail_range.end)
                },
                |index| self.delimiter_pairs[index].close.0,
            );
        let start = binding.declaration.0 + 2;
        let tokens = self.tokens.get(start..close)?;
        // Parameter identity and nesting are already indexed. Inspect only the
        // local type-token span, never reparse a declaration or the source file.
        let end = tokens
            .iter()
            .enumerate()
            .position(|(offset, token)| {
                let index = start + offset;
                token.kind == TokenKind::Punctuation
                    && self.delimiter_context[index] == Some(open)
                    && self.token_text(source, TokenId(index)) == Some(",")
            })
            .unwrap_or(tokens.len());
        let annotation = &tokens[..end];
        let first = annotation
            .iter()
            .find(|token| token.kind != TokenKind::Comment)?;
        let last = annotation
            .iter()
            .rfind(|token| token.kind != TokenKind::Comment)?;
        Some(TextRange::new(first.range.start, last.range.end))
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        if self.auxiliary.diagnostic_count == 0 {
            return &[];
        }
        self.auxiliary
            .data
            .as_deref()
            .map_or(&[], |data| data.diagnostics.as_ref())
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    #[must_use]
    pub fn line_indent(&self, line: usize) -> Option<usize> {
        self.lines.get(line).map(|line| line.indent)
    }

    #[must_use]
    pub fn line_code_range(&self, line: usize) -> Option<TextRange> {
        self.lines.get(line).map(|line| line.code)
    }

    #[must_use]
    pub fn line_content_range(&self, line: usize) -> Option<TextRange> {
        self.lines.get(line).map(|line| line.content)
    }

    #[must_use]
    pub fn line_full_end(&self, line: usize) -> Option<usize> {
        self.lines.get(line).map(|line| line.full_end)
    }

    #[must_use]
    pub fn name_id(&self, source: &str, name: &str) -> Option<NameId> {
        self.names.find(source, name)
    }

    #[must_use]
    pub fn name_for_token(&self, token: TokenId) -> Option<NameId> {
        self.names.for_token(token)
    }

    #[must_use]
    pub fn is_keyword(&self, name: NameId) -> bool {
        self.keyword_names.get(name.0).copied().unwrap_or(false)
    }

    #[must_use]
    pub fn name_range(&self, name: NameId) -> TextRange {
        self.names.range(name)
    }

    #[must_use]
    pub fn name_text<'a>(&self, source: &'a str, name: NameId) -> &'a str {
        let range = self.name_range(name);
        &source[range.start..range.end]
    }

    #[must_use]
    pub fn symbol_by_name(&self, name: NameId) -> Option<&IndexedSymbol> {
        self.symbol_by_name
            .get(name.0)
            .copied()
            .flatten()
            .map(|id| &self.symbols[id.0])
    }

    #[must_use]
    pub fn constructor_by_name(&self, name: NameId) -> Option<&IndexedConstructor> {
        let index = self.auxiliary.data.as_deref()?.constructors.as_ref()?;
        let constructor = if index.name_indices.is_empty() {
            let position = index.entries.partition_point(|entry| entry.name.0 < name.0);
            index.entries.get(position)?
        } else {
            let position = index
                .name_indices
                .partition_point(|entry| index.entries[*entry].name.0 < name.0);
            &index.entries[*index.name_indices.get(position)?]
        };
        (constructor.name == name).then_some(constructor)
    }

    #[must_use]
    pub fn constructors(&self, parent: SymbolId) -> &[IndexedConstructor] {
        let Some(index) = self
            .auxiliary
            .data
            .as_deref()
            .and_then(|data| data.constructors.as_ref())
        else {
            return &[];
        };
        let start = index
            .entries
            .partition_point(|constructor| constructor.parent.0 < parent.0);
        let end = index
            .entries
            .partition_point(|constructor| constructor.parent.0 <= parent.0);
        &index.entries[start..end]
    }

    #[must_use]
    pub fn symbol_by_id(&self, id: SymbolId) -> Option<&IndexedSymbol> {
        self.symbols.get(id.0)
    }

    #[must_use]
    pub fn binding_by_id(&self, id: SymbolId) -> Option<&IndexedBinding> {
        self.bindings.get(id.0.checked_sub(self.symbols.len())?)
    }

    #[must_use]
    pub fn symbol_for_token(&self, token: TokenId) -> Option<SymbolId> {
        self.token_symbols.get(token.0).copied().flatten()
    }

    #[must_use]
    pub fn symbol_name(&self, id: SymbolId) -> Option<NameId> {
        self.symbol_by_id(id)
            .map(|symbol| symbol.name)
            .or_else(|| self.binding_by_id(id).map(|binding| binding.name))
    }

    #[must_use]
    pub fn symbol_declaration_range(&self, id: SymbolId) -> Option<TextRange> {
        self.symbol_by_id(id)
            .map(|symbol| symbol.name_range)
            .or_else(|| self.binding_by_id(id).map(|binding| binding.name_range))
    }

    pub fn references(&self, id: SymbolId) -> impl Iterator<Item = &Reference> {
        let span = self.reference_spans.get(id.0).copied().unwrap_or_default();
        self.reference_indices[span.start..span.end]
            .iter()
            .map(|index| &self.references[*index])
    }

    pub fn references_named(&self, name: NameId) -> impl Iterator<Item = &Reference> {
        let span = self
            .name_reference_spans
            .get(name.0)
            .copied()
            .unwrap_or_default();
        self.name_reference_indices[span.start..span.end]
            .iter()
            .map(|index| &self.references[*index])
    }

    #[must_use]
    pub fn reference_for_token(&self, token: TokenId) -> Option<&Reference> {
        let index = *self.token_references.get(token.0)?.as_ref()?;
        self.references.get(index)
    }

    #[must_use]
    pub fn calls(&self) -> &[CallSite] {
        &self.calls
    }

    pub fn calls_from(&self, symbol: SymbolId) -> impl Iterator<Item = &CallSite> {
        let span = self.calls_from_spans.get(symbol.0).copied();
        span.into_iter()
            .flat_map(|span| self.calls_from_indices[span.start..span.end].iter())
            .map(|index| &self.calls[*index])
    }

    pub fn calls_to(&self, symbol: SymbolId) -> impl Iterator<Item = &CallSite> {
        let span = self.calls_to_spans.get(symbol.0).copied();
        span.into_iter()
            .flat_map(|span| self.calls_to_indices[span.start..span.end].iter())
            .map(|index| &self.calls[*index])
    }

    #[must_use]
    pub fn call_arguments(&self, call: &CallSite) -> &[TextRange] {
        &self.call_arguments[call.argument_indices.start..call.argument_indices.end]
    }

    #[must_use]
    pub fn call_by_open(&self, open: TokenId) -> Option<&CallSite> {
        let index = self.call_by_open.get(open.0).copied().flatten()?;
        self.calls.get(index)
    }

    #[must_use]
    pub(crate) fn call_index_for_token(&self, token: TokenId) -> Option<usize> {
        self.call_by_name_token.get(token.0).copied().flatten()
    }

    #[must_use]
    pub fn call_for_token(&self, token: TokenId) -> Option<&CallSite> {
        let index = self.call_index_for_token(token)?;
        self.calls.get(index)
    }

    #[must_use]
    pub fn call_at(&self, offset: usize) -> Option<(&CallSite, u32)> {
        let previous = self
            .tokens
            .partition_point(|token| token.range.start < offset)
            .checked_sub(1)?;
        let open = self.delimiter_context[previous]?;
        let index = self.call_by_open.get(open.0).copied().flatten()?;
        let call = &self.calls[index];
        let separators =
            &self.call_separators[call.separator_indices.start..call.separator_indices.end];
        let active = separators.partition_point(|separator| *separator < offset);
        Some((call, u32::try_from(active).unwrap_or(u32::MAX)))
    }

    #[must_use]
    pub fn token(&self, id: TokenId) -> Option<&Token> {
        self.tokens.get(id.0)
    }

    #[must_use]
    pub fn token_text<'a>(&self, source: &'a str, id: TokenId) -> Option<&'a str> {
        let range = self.token(id)?.range;
        source.get(range.start..range.end)
    }

    #[must_use]
    pub fn token_at_or_before(&self, offset: usize) -> Option<TokenId> {
        let index = self
            .tokens
            .partition_point(|token| token.range.start <= offset)
            .checked_sub(1)?;
        Some(TokenId(index))
    }

    #[must_use]
    pub fn is_in_comment_or_string(&self, offset: usize) -> bool {
        let Some(id) = self.token_at_or_before(offset) else {
            return false;
        };
        let token = self.tokens[id.0];
        match token.kind {
            TokenKind::Comment => token.range.start <= offset && offset <= token.range.end,
            TokenKind::StringLiteral => token.range.start <= offset && offset < token.range.end,
            _ => false,
        }
    }

    #[must_use]
    pub fn active_call<'a>(
        &self,
        source: &'a str,
        offset: usize,
    ) -> Option<(&'a str, u32, TokenId, TokenId)> {
        let (call, parameter) = self.call_at(offset)?;
        Some((
            self.name_text(source, call.name),
            parameter,
            call.open,
            call.callee_token,
        ))
    }

    #[must_use]
    pub fn call_argument_ranges(&self, open: TokenId) -> &[TextRange] {
        self.call_by_open(open)
            .map_or(&[], |call| self.call_arguments(call))
    }

    #[must_use]
    pub fn delimiter_close(&self, open: TokenId) -> Option<TokenId> {
        self.delimiter_pairs
            .binary_search_by_key(&open, |pair| pair.open)
            .ok()
            .map(|index| self.delimiter_pairs[index].close)
    }
}

fn compact_groups<T>(
    items: &[T],
    group_count: usize,
    counts: &mut Vec<usize>,
    key: impl Fn(&T) -> Option<usize>,
) -> (Vec<usize>, Vec<TextRange>) {
    counts.clear();
    counts.resize(group_count, 0);
    for item in items {
        if let Some(group) = key(item).filter(|group| *group < group_count) {
            counts[group] += 1;
        }
    }
    let mut spans = Vec::with_capacity(group_count);
    let mut total = 0;
    for count in counts.iter_mut() {
        let end = total + *count;
        spans.push(TextRange::new(total, end));
        *count = total;
        total = end;
    }
    let mut indices = vec![0; total];
    for (index, item) in items.iter().enumerate() {
        if let Some(group) = key(item).filter(|group| *group < group_count) {
            indices[counts[group]] = index;
            counts[group] += 1;
        }
    }
    (indices, spans)
}

fn enclosing_function(symbols: &[IndexedSymbol], offset: usize) -> Option<SymbolId> {
    let index = symbols
        .partition_point(|symbol| symbol.scope.start <= offset)
        .checked_sub(1)?;
    let symbol = symbols[index];
    (symbol.kind == SymbolKind::Function && symbol.scope.contains(offset)).then_some(symbol.id)
}

fn qualified_chain_start(source: &str, tokens: &[Token], mut index: usize) -> usize {
    while index >= 2
        && tokens[index - 1].kind == TokenKind::Punctuation
        && &source[tokens[index - 1].range.start..tokens[index - 1].range.end] == "."
        && tokens[index - 1].range.end == tokens[index].range.start
        && tokens[index - 2].kind == TokenKind::Identifier
        && tokens[index - 2].range.end == tokens[index - 1].range.start
    {
        index -= 2;
    }
    index
}
