use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use super::{LineIndex, SymbolKind, TextRange};

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
    delimiter_context: &'a [Option<TokenId>],
    symbol_count: usize,
    names: &'a NameTable,
}

struct CallInputs<'a> {
    source: &'a str,
    tokens: &'a [Token],
    delimiter_pairs: &'a [DelimiterPair],
    delimiter_context: &'a [Option<TokenId>],
    symbols: &'a [IndexedSymbol],
    bindings: &'a [IndexedBinding],
    token_symbols: &'a [Option<SymbolId>],
    names: &'a mut NameTable,
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

struct CallIndex {
    calls: Vec<CallSite>,
    arguments: Vec<TextRange>,
    separators: Vec<usize>,
    by_open: Vec<Option<usize>>,
    by_name_token: Vec<Option<usize>>,
    from_indices: Vec<usize>,
    from_spans: Vec<TextRange>,
    to_indices: Vec<usize>,
    to_spans: Vec<TextRange>,
}

struct ReferenceInputs<'a> {
    source: &'a str,
    tokens: &'a [Token],
    names: &'a mut NameTable,
    symbols: &'a [IndexedSymbol],
    bindings: &'a [IndexedBinding],
    token_symbols: &'a [Option<SymbolId>],
    calls: &'a [CallSite],
}

struct ReferenceIndex {
    references: Vec<Reference>,
    by_symbol_indices: Vec<usize>,
    by_symbol_spans: Vec<TextRange>,
    by_name_indices: Vec<usize>,
    by_name_spans: Vec<TextRange>,
    token_references: Vec<Option<usize>>,
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
    token_names: Vec<Option<NameId>>,
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
                    self.token_names.push(Some(candidate));
                    return candidate;
                }
            }
        }
        let id = NameId(self.ranges.len());
        self.ranges.push(range);
        self.lookup.entry(hash).or_default().push(id);
        self.token_names.push(Some(id));
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
        self.token_names.push(None);
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
        self.token_names.get(id.0).copied().flatten()
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
    symbol_by_name: HashMap<NameId, SymbolId>,
    token_symbols: Box<[Option<SymbolId>]>,
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
            names: &mut names,
            symbols: &symbols,
            bindings: &bindings,
            token_symbols: &token_symbols,
            calls: &calls.calls,
        });
        Self {
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
        }
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
        let span = self.reference_spans.get(id.0).copied();
        span.into_iter()
            .flat_map(|span| self.reference_indices[span.start..span.end].iter())
            .map(|index| &self.references[*index])
    }

    pub fn references_named(&self, name: NameId) -> impl Iterator<Item = &Reference> {
        let span = self.name_reference_spans.get(name.0).copied();
        span.into_iter()
            .flat_map(|span| self.name_reference_indices[span.start..span.end].iter())
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
    pub fn call_for_token(&self, token: TokenId) -> Option<&CallSite> {
        let index = self.call_by_name_token.get(token.0).copied().flatten()?;
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

fn hash_name(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

struct ScanOutput {
    tokens: Vec<Token>,
    names: NameTable,
    diagnostics: Vec<SyntaxDiagnostic>,
    delimiter_pairs: Vec<DelimiterPair>,
    delimiter_context: Vec<Option<TokenId>>,
    line_starts: Vec<usize>,
    line_ascii: Vec<u64>,
}

struct Scanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    offset: usize,
    tokens: Vec<Token>,
    names: NameTable,
    diagnostics: Vec<SyntaxDiagnostic>,
    delimiters: Vec<(char, TokenId)>,
    delimiter_pairs: Vec<DelimiterPair>,
    delimiter_context: Vec<Option<TokenId>>,
    line_starts: Vec<usize>,
    line_ascii: Vec<u64>,
}

impl<'a> Scanner<'a> {
    fn new(source: &'a str, collect_line_starts: bool) -> Self {
        let (line_starts, line_ascii) = if collect_line_starts {
            (vec![0], vec![u64::MAX])
        } else {
            (Vec::new(), Vec::new())
        };
        Self {
            source,
            bytes: source.as_bytes(),
            offset: 0,
            tokens: Vec::new(),
            names: NameTable::new(),
            diagnostics: Vec::new(),
            delimiters: Vec::new(),
            delimiter_pairs: Vec::new(),
            delimiter_context: Vec::new(),
            line_starts,
            line_ascii,
        }
    }

    fn scan(mut self) -> ScanOutput {
        while self.offset < self.bytes.len() {
            let byte = self.bytes[self.offset];
            if byte == b'\n' {
                if !self.line_starts.is_empty() {
                    self.line_starts.push(self.offset + 1);
                    if (self.line_starts.len() - 1).is_multiple_of(u64::BITS as usize) {
                        self.line_ascii.push(u64::MAX);
                    }
                }
                self.offset += 1;
            } else if byte.is_ascii_whitespace() {
                self.offset += 1;
            } else if byte == b'#' {
                self.comment();
            } else if matches!(byte, b'\'' | b'"') {
                self.string(byte);
            } else if byte.is_ascii_alphabetic() || byte == b'_' {
                self.identifier();
            } else if byte.is_ascii_digit() {
                self.number();
            } else if matches!(byte, b'(' | b'[' | b'{' | b')' | b']' | b'}') {
                self.delimiter();
            } else if b"+-*/%=<>!&|^~".contains(&byte) {
                self.operator();
            } else {
                if !byte.is_ascii() {
                    self.mark_non_ascii();
                }
                self.punctuation();
            }
        }
        for &(delimiter, token) in &self.delimiters {
            let range = self.tokens[token.0].range;
            self.diagnostics.push(SyntaxDiagnostic {
                range,
                kind: DiagnosticKind::UnclosedDelimiter(delimiter),
            });
        }
        self.delimiter_pairs.sort_unstable_by_key(|pair| pair.open);
        ScanOutput {
            tokens: self.tokens,
            names: self.names,
            diagnostics: self.diagnostics,
            delimiter_pairs: self.delimiter_pairs,
            delimiter_context: self.delimiter_context,
            line_starts: self.line_starts,
            line_ascii: self.line_ascii,
        }
    }

    fn mark_non_ascii(&mut self) {
        if let Some(line) = self.line_starts.len().checked_sub(1) {
            self.line_ascii[line / u64::BITS as usize] &= !(1_u64 << (line % u64::BITS as usize));
        }
    }

    fn comment(&mut self) {
        let start = self.offset;
        while self.offset < self.bytes.len() && self.bytes[self.offset] != b'\n' {
            if !self.bytes[self.offset].is_ascii() {
                self.mark_non_ascii();
            }
            self.offset += 1;
        }
        self.push_token(
            start,
            self.offset,
            TokenKind::Comment,
            TokenFlags::default(),
            None,
        );
    }

    fn string(&mut self, quote: u8) {
        let start = self.offset;
        self.offset += 1;
        let mut escaped = false;
        let mut saw_escape = false;
        let mut closed = false;
        while self.offset < self.bytes.len() && self.bytes[self.offset] != b'\n' {
            let byte = self.bytes[self.offset];
            if !byte.is_ascii() {
                self.mark_non_ascii();
            }
            self.offset += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
                saw_escape = true;
            } else if byte == quote {
                closed = true;
                break;
            }
        }
        let mut flags = TokenFlags::default();
        if saw_escape {
            flags.0 |= TokenFlags::ESCAPED;
        }
        if !closed {
            flags.0 |= TokenFlags::UNTERMINATED;
            self.diagnostics.push(SyntaxDiagnostic {
                range: TextRange::new(start, self.offset),
                kind: DiagnosticKind::UnterminatedString(char::from(quote)),
            });
        }
        self.push_token(start, self.offset, TokenKind::StringLiteral, flags, None);
    }

    fn identifier(&mut self) {
        let start = self.offset;
        self.offset += 1;
        while self.offset < self.bytes.len()
            && (self.bytes[self.offset].is_ascii_alphanumeric() || self.bytes[self.offset] == b'_')
        {
            self.offset += 1;
        }
        let range = TextRange::new(start, self.offset);
        let id = TokenId(self.tokens.len());
        let name = self.names.intern(self.source, range, id);
        self.push_token(
            start,
            self.offset,
            TokenKind::Identifier,
            TokenFlags::default(),
            Some(name),
        );
    }

    fn number(&mut self) {
        let start = self.offset;
        self.offset += 1;
        while self.offset < self.bytes.len()
            && (self.bytes[self.offset].is_ascii_alphanumeric() || self.bytes[self.offset] == b'_')
        {
            self.offset += 1;
        }
        self.push_token(
            start,
            self.offset,
            TokenKind::Number,
            TokenFlags::default(),
            None,
        );
    }

    fn delimiter(&mut self) {
        let start = self.offset;
        let delimiter = char::from(self.bytes[self.offset]);
        self.offset += 1;
        let id = TokenId(self.tokens.len());
        self.push_token(
            start,
            self.offset,
            TokenKind::Delimiter,
            TokenFlags::default(),
            None,
        );
        if matches!(delimiter, '(' | '[' | '{') {
            self.delimiters.push((delimiter, id));
            self.delimiter_context[id.0] = Some(id);
            return;
        }
        let expected = match delimiter {
            ')' => '(',
            ']' => '[',
            _ => '{',
        };
        match self.delimiters.last().copied() {
            Some((open, open_token)) if open == expected => {
                self.delimiters.truncate(self.delimiters.len() - 1);
                self.delimiter_pairs.push(DelimiterPair {
                    open: open_token,
                    close: id,
                });
                self.delimiter_context[id.0] = self.delimiters.last().map(|(_, open)| *open);
            }
            _ => {
                self.diagnostics.push(SyntaxDiagnostic {
                    range: TextRange::new(start, self.offset),
                    kind: DiagnosticKind::UnmatchedDelimiter(delimiter),
                });
            }
        }
    }

    fn operator(&mut self) {
        let start = self.offset;
        self.offset += 1;
        while self.offset < self.bytes.len() && b"+-*/%=<>!&|^~".contains(&self.bytes[self.offset])
        {
            self.offset += 1;
        }
        self.push_token(
            start,
            self.offset,
            TokenKind::Operator,
            TokenFlags::default(),
            None,
        );
    }

    fn punctuation(&mut self) {
        let start = self.offset;
        if self.bytes[start] == b'?'
            && self.source[start..].starts_with("?TODO")
            && !self.source[start + 5..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            self.diagnostics.push(SyntaxDiagnostic {
                range: TextRange::new(start, start + 5),
                kind: DiagnosticKind::UnresolvedHole,
            });
        }
        let width = self.source[start..]
            .chars()
            .next()
            .map_or(1, char::len_utf8);
        self.offset += width;
        self.push_token(
            start,
            self.offset,
            TokenKind::Punctuation,
            TokenFlags::default(),
            None,
        );
    }

    fn push_token(
        &mut self,
        start: usize,
        end: usize,
        kind: TokenKind,
        flags: TokenFlags,
        name: Option<NameId>,
    ) {
        self.tokens.push(Token {
            range: TextRange::new(start, end),
            kind,
            flags,
        });
        self.delimiter_context
            .push(self.delimiters.last().map(|(_, id)| *id));
        if name.is_none() {
            self.names.append_non_name();
        }
    }
}

fn build_lines(source: &str, index: &LineIndex) -> Vec<SyntaxLine> {
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

fn parse_imports(source: &str, lines: &[SyntaxLine]) -> Vec<IndexedImport> {
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

fn parse_symbols(source: &str, lines: &[SyntaxLine], names: &mut NameTable) -> Vec<IndexedSymbol> {
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
fn parse_constructors(
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

fn parse_bindings(
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
    let end = context
        .tokens
        .partition_point(|token| token.range.start < header_end);
    let open = (start..end)
        .find(|index| {
            let token = context.tokens[*index];
            token.kind == TokenKind::Delimiter
                && &context.source[token.range.start..token.range.end] == "("
        })
        .map(TokenId);
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

fn resolve_symbols(
    source: &str,
    tokens: &[Token],
    names: &NameTable,
    symbols: &[IndexedSymbol],
    bindings: &[IndexedBinding],
    symbol_by_name: &HashMap<NameId, SymbolId>,
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
                    .or_else(|| symbol_by_name.get(&name).copied())
            })
        };
        token_symbols[index] = symbol;
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

fn declaration_tokens(
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

fn build_references(input: ReferenceInputs<'_>) -> ReferenceIndex {
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
        if token.kind != TokenKind::Identifier || token_references[token_index].is_some() {
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
            resolved: token_symbols[token_index],
        });
        token_references[token_index] = Some(index);
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
    token_references: &mut [Option<usize>],
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
                token_references[start + offset] = Some(index);
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
        token_references[binding.declaration.0] = Some(index);
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
