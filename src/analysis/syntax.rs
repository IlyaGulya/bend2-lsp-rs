use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use super::{LineIndex, SymbolKind, TextRange};

#[path = "syntax/completion.rs"]
mod completion;
#[path = "syntax/declarations.rs"]
mod declarations;
#[path = "syntax/scanner.rs"]
mod scanner;

use declarations::{
    build_lines, declaration_tokens, parse_bindings, parse_constructors, parse_imports,
    parse_symbols,
};
use scanner::Scanner;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CasePatternType {
    pub explicit_type: Option<TextRange>,
}

const KEYWORDS: &[&str] = &[
    "def", "law", "type", "is", "match", "case", "do", "return", "for", "exs", "where", "import",
    "as",
];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NameId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SymbolId(pub usize);

// usize::MAX cannot index any of the non-ZST token/symbol/reference/call buffers.
// Keep empty dense slots in one word without narrowing the public ID domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexSlot(usize);

impl IndexSlot {
    const EMPTY: Self = Self(usize::MAX);

    const fn some(index: usize) -> Self {
        Self(index)
    }

    const fn from_option(index: Option<usize>) -> Self {
        match index {
            Some(index) => Self(index),
            None => Self::EMPTY,
        }
    }

    const fn value(self) -> Option<usize> {
        if self.0 == usize::MAX {
            None
        } else {
            Some(self.0)
        }
    }
}

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

struct BindingContext<'a> {
    source: &'a str,
    lines: &'a [SyntaxLine],
    tokens: &'a [Token],
    delimiter_pairs: &'a [DelimiterPair],
    delimiter_context: &'a [IndexSlot],
    symbol_count: usize,
    names: &'a NameTable,
}

struct CallInputs<'a> {
    source: &'a str,
    tokens: &'a [Token],
    delimiter_pairs: &'a [DelimiterPair],
    delimiter_context: &'a [IndexSlot],
    symbols: &'a [IndexedSymbol],
    bindings: &'a [IndexedBinding],
    token_symbols: &'a [IndexSlot],
    names: &'a mut NameTable,
}

struct CallBuild<'a> {
    source: &'a str,
    tokens: &'a [Token],
    delimiter_pairs: &'a [DelimiterPair],
    delimiter_context: &'a [IndexSlot],
    symbols: &'a [IndexedSymbol],
    token_symbols: &'a [IndexSlot],
    declarations: &'a [bool],
    names: &'a mut NameTable,
}

struct CallIndex {
    calls: Vec<CallSite>,
    arguments: Vec<TextRange>,
    separators: Vec<usize>,
    by_open: Vec<IndexSlot>,
    by_name_token: Vec<IndexSlot>,
    from_indices: Vec<usize>,
    from_spans: Vec<TextRange>,
    to_indices: Vec<usize>,
    to_spans: Vec<TextRange>,
}

struct ReferenceInputs<'a> {
    source: &'a str,
    tokens: &'a [Token],
    identifier_count: usize,
    names: &'a mut NameTable,
    symbols: &'a [IndexedSymbol],
    bindings: &'a [IndexedBinding],
    token_symbols: &'a [IndexSlot],
    calls: &'a [CallSite],
}

struct ReferenceIndex {
    references: Vec<Reference>,
    by_symbol_indices: Vec<usize>,
    by_symbol_spans: Vec<TextRange>,
    by_name_indices: Vec<usize>,
    by_name_spans: Vec<TextRange>,
    token_references: Vec<IndexSlot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SyntaxLine {
    content: TextRange,
    full_end: usize,
    code: TextRange,
    indent: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct NameTable {
    ranges: Vec<TextRange>,
    lookup: HashMap<u64, Vec<NameId>>,
    token_names: Vec<IndexSlot>,
}
impl NameTable {
    fn new() -> Self {
        Self {
            ranges: Vec::new(),
            lookup: HashMap::new(),
            token_names: Vec::new(),
        }
    }

    fn intern(&mut self, source: &str, range: TextRange, _token: TokenId) -> NameId {
        let text = &source[range.start..range.end];
        let hash = hash_name(text);
        if let Some(candidates) = self.lookup.get(&hash) {
            for &candidate in candidates {
                let existing = self.ranges[candidate.0];
                if source[existing.start..existing.end] == *text {
                    self.token_names.push(IndexSlot::some(candidate.0));
                    return candidate;
                }
            }
        }
        let id = NameId(self.ranges.len());
        self.ranges.push(range);
        self.lookup.entry(hash).or_default().push(id);
        self.token_names.push(IndexSlot::some(id.0));
        id
    }

    fn intern_name(&mut self, source: &str, range: TextRange) -> NameId {
        if let Some(id) = self.find(source, &source[range.start..range.end]) {
            return id;
        }
        let id = NameId(self.ranges.len());
        let name = &source[range.start..range.end];
        self.ranges.push(range);
        self.lookup.entry(hash_name(name)).or_default().push(id);
        id
    }

    fn append_non_name(&mut self) {
        self.token_names.push(IndexSlot::EMPTY);
    }

    fn find(&self, source: &str, text: &str) -> Option<NameId> {
        let candidates = self.lookup.get(&hash_name(text))?;
        candidates.iter().copied().find(|candidate| {
            let range = self.ranges[candidate.0];
            source[range.start..range.end] == *text
        })
    }

    fn range(&self, id: NameId) -> TextRange {
        self.ranges[id.0]
    }

    fn for_token(&self, id: TokenId) -> Option<NameId> {
        self.token_names
            .get(id.0)
            .copied()
            .and_then(IndexSlot::value)
            .map(NameId)
    }
}

fn keyword_name_flags(source: &str, names: &NameTable) -> Vec<bool> {
    let mut flags = vec![false; names.ranges.len()];
    for keyword in KEYWORDS {
        if let Some(name) = names.find(source, keyword) {
            flags[name.0] = true;
        }
    }
    flags
}

#[derive(Debug, Eq, PartialEq)]
pub struct SyntaxIndex {
    tokens: Box<[Token]>,
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
    token_references: Box<[IndexSlot]>,
    calls: Box<[CallSite]>,
    call_arguments: Box<[TextRange]>,
    call_separators: Box<[usize]>,
    calls_from_indices: Box<[usize]>,
    calls_from_spans: Box<[TextRange]>,
    calls_to_indices: Box<[usize]>,
    calls_to_spans: Box<[TextRange]>,
    call_by_open: Box<[IndexSlot]>,
    call_by_name_token: Box<[IndexSlot]>,
    lines: Box<[SyntaxLine]>,
    auxiliary: AuxiliaryIndex,
    delimiter_pairs: Box<[DelimiterPair]>,
    delimiter_context: Box<[IndexSlot]>,
    symbol_by_name: HashMap<NameId, SymbolId>,
    token_symbols: Box<[IndexSlot]>,
    completion: completion::CompletionIndex,
}

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
            identifier_count,
            names,
            diagnostics,
            delimiter_pairs,
            delimiter_context,
            ..
        } = scan;
        let mut names = names;
        let keyword_names = keyword_name_flags(source, &names);
        let mut symbols = parse_symbols(source, &lines, &mut names);
        for (index, symbol) in symbols.iter_mut().enumerate() {
            symbol.id = SymbolId(index);
        }
        let mut symbol_by_name = HashMap::with_capacity(symbols.len());
        for symbol in &symbols {
            symbol_by_name.entry(symbol.name).or_insert(symbol.id);
        }
        let constructor_index = if symbols
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Struct)
        {
            parse_constructors(source, &lines, &symbols, &mut names)
        } else {
            None
        };
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
        let calls = build_calls(CallInputs {
            source,
            tokens: &tokens,
            delimiter_pairs: &delimiter_pairs,
            delimiter_context: &delimiter_context,
            symbols: &symbols,
            bindings: &bindings,
            token_symbols: &token_symbols,
            names: &mut names,
        });
        let references = build_references(ReferenceInputs {
            source,
            tokens: &tokens,
            identifier_count,
            names: &mut names,
            symbols: &symbols,
            bindings: &bindings,
            token_symbols: &token_symbols,
            calls: &calls.calls,
        });
        let mut index = Self {
            tokens: tokens.into_boxed_slice(),
            names,
            imports: imports.into_boxed_slice(),
            keyword_names: keyword_names.into_boxed_slice(),
            symbols: symbols.into_boxed_slice(),
            bindings: bindings.into_boxed_slice(),
            references: references.references.into_boxed_slice(),
            reference_indices: references.by_symbol_indices.into_boxed_slice(),
            reference_spans: references.by_symbol_spans.into_boxed_slice(),
            name_reference_indices: references.by_name_indices.into_boxed_slice(),
            name_reference_spans: references.by_name_spans.into_boxed_slice(),
            token_references: references.token_references.into_boxed_slice(),
            calls: calls.calls.into_boxed_slice(),
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
            symbol_by_name,
            token_symbols: token_symbols.into_boxed_slice(),
            completion: completion::CompletionIndex::default(),
        };
        index.completion = completion::CompletionIndex::build(source, &index);
        index
    }

    #[must_use]
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
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

    /// Borrow in-scope binding candidates from the current function's dense rows.
    /// No source parsing or workspace traversal is performed by this query.
    pub fn bindings_at(&self, offset: usize) -> impl Iterator<Item = &IndexedBinding> {
        // A caret at EOF belongs to the final indexed scope, whose byte range
        // is half-open. Compare its left-hand byte without slicing UTF-8 text.
        let end = self.lines.last().map_or(0, |line| line.full_end);
        let offset = offset.min(end.saturating_sub(1));
        let symbol = self
            .symbols
            .partition_point(|symbol| symbol.scope.start <= offset)
            .checked_sub(1)
            .and_then(|index| self.symbols.get(index))
            .filter(|symbol| symbol.kind == SymbolKind::Function && symbol.scope.contains(offset));
        let bindings = symbol.map_or(&[][..], |symbol| {
            let start = self
                .bindings
                .partition_point(|binding| binding.owner.0 < symbol.id.0);
            let end = self
                .bindings
                .partition_point(|binding| binding.owner.0 <= symbol.id.0);
            &self.bindings[start..end]
        });
        bindings
            .iter()
            .filter(move |binding| binding.scope.contains(offset))
    }

    #[inline]
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
            .and_then(IndexSlot::value)
            .map(TokenId)?;
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
                    && self.delimiter_context[index].value() == Some(open.0)
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
    pub fn is_keyword_text(name: &str) -> bool {
        KEYWORDS.contains(&name)
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
        self.symbol_by_name.get(&name).map(|id| &self.symbols[id.0])
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
        self.token_symbols
            .get(token.0)
            .copied()
            .and_then(IndexSlot::value)
            .map(SymbolId)
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

    pub fn qualified_references(&self, root: NameId) -> impl Iterator<Item = &Reference> {
        self.references.iter().filter(move |reference| {
            reference
                .qualifier_token
                .and_then(|token| self.name_for_token(token))
                == Some(root)
        })
    }

    #[must_use]
    pub fn reference_for_token(&self, token: TokenId) -> Option<&Reference> {
        let index = self.token_references.get(token.0)?.value()?;
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
        let index = self.call_by_open.get(open.0)?.value()?;
        self.calls.get(index)
    }

    #[must_use]
    pub fn call_for_token(&self, token: TokenId) -> Option<&CallSite> {
        let index = self.call_by_name_token.get(token.0)?.value()?;
        self.calls.get(index)
    }

    #[must_use]
    pub fn call_at(&self, offset: usize) -> Option<(&CallSite, u32)> {
        let previous = self
            .tokens
            .partition_point(|token| token.range.start < offset)
            .checked_sub(1)?;
        let open = self.delimiter_context[previous].value()?;
        let index = self.call_by_open.get(open)?.value()?;
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

    /// An indexed pattern context; an absent explicit type keeps constructor choices unrestricted.
    #[must_use]
    pub fn case_pattern_type(&self, offset: usize) -> Option<CasePatternType> {
        self.completion.pattern_type(offset)
    }

    #[must_use]
    pub fn completion_import_path(&self, offset: usize) -> Option<TextRange> {
        self.completion.import_path(offset)
    }
}

fn hash_name(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

struct ScanOutput {
    tokens: Vec<Token>,
    identifier_count: usize,
    names: NameTable,
    diagnostics: Vec<SyntaxDiagnostic>,
    delimiter_pairs: Vec<DelimiterPair>,
    delimiter_context: Vec<IndexSlot>,
    line_starts: Vec<usize>,
    line_ascii: Vec<u64>,
}

fn resolve_symbols(
    source: &str,
    tokens: &[Token],
    names: &NameTable,
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
    symbol_by_name: &HashMap<NameId, SymbolId>,
) -> Vec<IndexSlot> {
    let mut token_symbols = vec![IndexSlot::EMPTY; tokens.len()];
    for symbol in symbols {
        let start = tokens.partition_point(|token| token.range.end <= symbol.name_range.start);
        let end = tokens.partition_point(|token| token.range.start < symbol.name_range.end);
        for (offset, token) in tokens[start..end].iter().enumerate() {
            let index = start + offset;
            if token.kind == TokenKind::Identifier {
                token_symbols[index] = IndexSlot::some(symbol.id.0);
            }
        }
    }
    let mut bindings_by_name = HashMap::<NameId, Vec<usize>>::new();
    for (index, binding) in bindings.iter().enumerate() {
        token_symbols[binding.declaration.0] = IndexSlot::some(binding.id.0);
        bindings_by_name
            .entry(binding.name)
            .or_default()
            .push(index);
    }
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Identifier || token_symbols[index].value().is_some() {
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
                    .or_else(|| symbol_by_name.get(&name).copied())
            })
        };
        token_symbols[index] = IndexSlot::from_option(symbol.map(|id| id.0));
    }
    token_symbols
}

fn build_calls(input: CallInputs<'_>) -> CallIndex {
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
    let mut by_open = vec![IndexSlot::EMPTY; tokens.len()];
    let mut by_name_token = vec![IndexSlot::EMPTY; tokens.len()];
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
        *by_open_slot = IndexSlot::some(call_index);
        by_name_token[call.callee_token.0] = IndexSlot::some(call_index);
        if let Some(qualifier_token) = call.qualifier_token {
            by_name_token[qualifier_token.0] = IndexSlot::some(call_index);
        }
        calls.push(call);
    }
    let id_count = symbols.len() + bindings.len();
    let (from_indices, from_spans) =
        compact_groups(&calls, id_count, |call| call.caller.map(|id| id.0));
    let (to_indices, to_spans) =
        compact_groups(&calls, id_count, |call| call.callee.map(|id| id.0));
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
    let qualifier = (start_index < callee_index).then(|| {
        let end = context.tokens[callee_index - 2].range.end;
        context.names.intern_name(
            context.source,
            TextRange::new(context.tokens[start_index].range.start, end),
        )
    });
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
            && context.delimiter_context[index].value() == Some(open_index)
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
        callee: context.token_symbols[callee_index].value().map(SymbolId),
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

fn build_references(input: ReferenceInputs<'_>) -> ReferenceIndex {
    let ReferenceInputs {
        source,
        tokens,
        identifier_count,
        names,
        symbols,
        bindings,
        token_symbols,
        calls,
    } = input;
    let mut references = Vec::with_capacity(identifier_count);
    let mut token_references = vec![IndexSlot::EMPTY; tokens.len()];
    let mut call_tokens = vec![false; tokens.len()];
    for call in calls {
        call_tokens[call.callee_token.0] = true;
    }
    append_declaration_references(
        tokens,
        symbols,
        bindings,
        &mut references,
        &mut token_references,
    );
    for (token_index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Identifier || token_references[token_index].value().is_some() {
            continue;
        }
        let token_id = TokenId(token_index);
        let Some(name) = names.for_token(token_id) else {
            continue;
        };
        let start_index = qualified_chain_start(source, tokens, token_index);
        let (qualifier, qualifier_token) = if start_index < token_index {
            let end = tokens[token_index - 2].range.end;
            (
                Some(
                    names.intern_name(source, TextRange::new(tokens[start_index].range.start, end)),
                ),
                Some(TokenId(start_index)),
            )
        } else {
            (None, None)
        };
        let index = references.len();
        references.push(Reference {
            name,
            qualifier,
            qualifier_token,
            token: token_id,
            range: token.range,
            enclosing: enclosing_function(symbols, token.range.start),
            kind: if call_tokens[token_index] {
                ReferenceKind::Call
            } else {
                ReferenceKind::Read
            },
            resolved: token_symbols[token_index].value().map(SymbolId),
        });
        token_references[token_index] = IndexSlot::some(index);
    }
    let (by_symbol_indices, by_symbol_spans) =
        compact_groups(&references, symbols.len() + bindings.len(), |reference| {
            reference.resolved.map(|id| id.0)
        });
    let (by_name_indices, by_name_spans) =
        compact_groups(&references, names.ranges.len(), |reference| {
            Some(reference.name.0)
        });
    ReferenceIndex {
        references,
        by_symbol_indices,
        by_symbol_spans,
        by_name_indices,
        by_name_spans,
        token_references,
    }
}

fn append_declaration_references(
    tokens: &[Token],
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
    references: &mut Vec<Reference>,
    token_references: &mut [IndexSlot],
) {
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
                token_references[start + offset] = IndexSlot::some(index);
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
        token_references[binding.declaration.0] = IndexSlot::some(index);
    }
}

fn compact_groups<T>(
    items: &[T],
    group_count: usize,
    key: impl Fn(&T) -> Option<usize>,
) -> (Vec<usize>, Vec<TextRange>) {
    let mut counts = vec![0; group_count];
    for item in items {
        if let Some(group) = key(item).filter(|group| *group < group_count) {
            counts[group] += 1;
        }
    }
    let mut spans = Vec::with_capacity(group_count);
    let mut total = 0;
    for count in counts {
        let end = total + count;
        spans.push(TextRange::new(total, end));
        total = end;
    }
    let mut indices = vec![0; total];
    let mut next: Vec<_> = spans.iter().map(|span| span.start).collect();
    for (index, item) in items.iter().enumerate() {
        if let Some(group) = key(item).filter(|group| *group < group_count) {
            indices[next[group]] = index;
            next[group] += 1;
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
    symbol_by_name: &HashMap<NameId, SymbolId>,
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
    symbol_by_name.get(&name).copied()
}
