use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use super::{InlayHint, LineIndex, SymbolKind, TextRange};

#[path = "syntax/completion.rs"]
mod completion;
#[path = "syntax/scanner.rs"]
mod scanner;

use scanner::Scanner;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CasePatternType {
    pub explicit_type: Option<TextRange>,
}

#[path = "syntax/locals.rs"]
mod locals;
#[path = "syntax/parser.rs"]
mod parser;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NameId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SymbolId(pub usize);

// Empty dense slots use one word without narrowing the public ID domain.
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BindingId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ScopeId(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclarationKind {
    Def,
    Law,
    Type,
    Constructor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclarationBodyKind {
    None,
    Expression,
    Foreign,
    Constructors,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeclarationSite {
    pub symbol: SymbolId,
    pub name: NameId,
    pub kind: DeclarationKind,
    pub name_range: TextRange,
    pub full_range: TextRange,
    pub signature_range: TextRange,
    pub body_range: Option<TextRange>,
    pub doc_range: Option<TextRange>,
    pub template_parameter_count: usize,
    pub law_fill_compatible: bool,
    pub body_kind: DeclarationBodyKind,
    pub linkable: bool,
    pub uncertain: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexedScope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    pub owner: Option<SymbolId>,
    pub range: TextRange,
    pub uncertain: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NameSegment {
    pub token: TokenId,
    pub range: TextRange,
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

#[derive(Clone, Copy, Default)]
pub struct TokenFlags(u8);

impl PartialEq for TokenFlags {
    fn eq(&self, other: &Self) -> bool {
        self.0 & Self::LEXICAL_MASK == other.0 & Self::LEXICAL_MASK
    }
}

impl Eq for TokenFlags {}

impl std::fmt::Debug for TokenFlags {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("TokenFlags")
            .field(&(self.0 & Self::LEXICAL_MASK))
            .finish()
    }
}

impl TokenFlags {
    const ESCAPED: u8 = 1;
    const UNTERMINATED: u8 = 2;
    const CONTEXTUAL_KEYWORD: u8 = 4;
    // Presentation uses the spare bits without widening every Token. Public
    // equality/debug and the lexical flag accessors remain independent of it.
    const LEXICAL_MASK: u8 = 7;
    const SEMANTIC_SHIFT: u32 = 3;

    const fn with_semantic_type(self, token_type: Option<u8>) -> Self {
        let encoded = match token_type {
            Some(token_type) => (token_type + 1) << Self::SEMANTIC_SHIFT,
            None => 0,
        };
        Self((self.0 & Self::LEXICAL_MASK) | encoded)
    }

    pub(super) fn semantic_type(self) -> Option<u32> {
        let encoded = self.0 >> Self::SEMANTIC_SHIFT;
        if encoded == 0 {
            None
        } else {
            Some(u32::from(encoded - 1))
        }
    }

    #[must_use]
    pub const fn escaped(self) -> bool {
        self.0 & Self::ESCAPED != 0
    }

    #[must_use]
    pub const fn unterminated(self) -> bool {
        self.0 & Self::UNTERMINATED != 0
    }

    #[must_use]
    pub const fn contextual_keyword(self) -> bool {
        self.0 & Self::CONTEXTUAL_KEYWORD != 0
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
    /// Complete native loader-header syntax; path/target validation is separate.
    pub valid_syntax: bool,
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
    pub symbol: SymbolId,
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
    pub binding_id: BindingId,
    pub scope_id: ScopeId,
    pub name: NameId,
    pub name_range: TextRange,
    pub full_range: TextRange,
    pub annotation: Option<TextRange>,
    pub synthetic: bool,
    pub anonymous: bool,
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
pub enum ReferenceRole {
    Value,
    Type,
    Constructor,
    Call,
    Offload,
    Binding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reference {
    pub name: NameId,
    pub full_name: NameId,
    pub full_range: TextRange,
    pub segment_span: TextRange,
    pub qualifier: Option<NameId>,
    pub qualifier_token: Option<TokenId>,
    pub token: TokenId,
    pub range: TextRange,
    pub enclosing: Option<SymbolId>,
    pub kind: ReferenceKind,
    pub role: ReferenceRole,
    pub in_type: bool,
    pub uncertain: bool,
    pub synthetic: bool,
    /// Proven lexical identity; `resolved` may additionally cache a standalone file target.
    pub local_binding: Option<BindingId>,
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

#[derive(Debug, Eq, PartialEq)]
struct ParameterHint {
    position: usize,
    binding: usize,
}

struct CallIndex {
    calls: Vec<CallSite>,
    by_open: Vec<IndexSlot>,
    by_name_token: Vec<IndexSlot>,
    from_indices: Vec<usize>,
    from_spans: Vec<TextRange>,
    to_indices: Vec<usize>,
    to_spans: Vec<TextRange>,
}

struct ReferenceIndex {
    references: Vec<Reference>,
    source_indices: Vec<usize>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NameFacts {
    grammar: parser::Tag,
    semantic_type: u8,
    capitalized: bool,
}

impl NameFacts {
    fn new(text: &str) -> Self {
        let grammar = parser::Tag::classify(text.as_bytes());
        let capitalized = text.as_bytes().first().is_some_and(u8::is_ascii_uppercase);
        let semantic_type = if grammar.is_semantic_keyword() {
            15
        } else if capitalized {
            1
        } else {
            8
        };
        Self {
            grammar,
            semantic_type,
            capitalized,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct NameTable {
    ranges: Vec<TextRange>,
    facts: Vec<NameFacts>,
    lookup: HashMap<u64, NameId>,
    collision_next: Vec<IndexSlot>,
    token_names: Vec<IndexSlot>,
}
impl NameTable {
    fn new() -> Self {
        Self {
            ranges: Vec::new(),
            facts: Vec::new(),
            lookup: HashMap::new(),
            collision_next: Vec::new(),
            token_names: Vec::new(),
        }
    }

    fn intern(&mut self, source: &str, range: TextRange, _token: TokenId) -> NameId {
        let id = self.intern_name(source, range);
        self.token_names.push(IndexSlot::some(id.0));
        id
    }

    fn intern_name(&mut self, source: &str, range: TextRange) -> NameId {
        let name = &source[range.start..range.end];
        let hash = hash_name(name);
        if let Some(id) = self.find_hashed(source, name, hash) {
            return id;
        }
        let id = NameId(self.ranges.len());
        self.ranges.push(range);
        self.facts.push(NameFacts::new(name));
        self.collision_next.push(IndexSlot::from_option(
            self.lookup.insert(hash, id).map(|id| id.0),
        ));
        id
    }

    fn append_non_name(&mut self) {
        self.token_names.push(IndexSlot::EMPTY);
    }

    fn find(&self, source: &str, text: &str) -> Option<NameId> {
        self.find_hashed(source, text, hash_name(text))
    }

    fn find_hashed(&self, source: &str, text: &str, hash: u64) -> Option<NameId> {
        let mut candidate = self.lookup.get(&hash).copied();
        while let Some(id) = candidate {
            let range = self.ranges[id.0];
            if source[range.start..range.end] == *text {
                return Some(id);
            }
            candidate = self.collision_next[id.0].value().map(NameId);
        }
        None
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

#[derive(Debug, Eq, PartialEq)]
pub struct SyntaxIndex {
    tokens: Box<[Token]>,
    semantic_token_count: usize,
    parameter_hints: Box<[ParameterHint]>,
    names: NameTable,
    imports: Box<[IndexedImport]>,
    symbols: Box<[IndexedSymbol]>,
    bindings: Box<[IndexedBinding]>,
    local_index: locals::LocalIndex,
    completion_code_end: usize,
    completion_eof_bindings: Box<[BindingId]>,
    sites: Box<[DeclarationSite]>,
    scopes: Box<[IndexedScope]>,
    segments: Box<[NameSegment]>,
    references: Box<[Reference]>,
    reference_source_indices: Box<[usize]>,
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
    symbol_by_name: Box<[IndexSlot]>,
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

    fn from_scan(source: &str, line_index: &LineIndex, mut scan: ScanOutput) -> Self {
        let lines = build_lines(source, line_index);
        let mut facts = parser::parse(
            source,
            &mut scan.tokens,
            &lines,
            line_index,
            &mut scan.names,
        );
        let ScanOutput {
            mut tokens,
            semantic_token_count,
            names,
            diagnostics,
            delimiter_pairs,
            delimiter_context,
            ..
        } = scan;
        let mut symbol_by_name = standalone_symbol_table(&facts.symbols, names.ranges.len());
        let constructor_targets = standalone_constructor_targets(&facts.constructors);
        let constructor_index = constructor_index(std::mem::take(&mut facts.constructors));
        let auxiliary = AuxiliaryIndex::new(diagnostics, constructor_index);
        resolve_standalone_references(&mut facts, &symbol_by_name, &constructor_targets);
        let local_index = locals::LocalIndex::build(&facts.bindings, names.ranges.len());
        let (token_symbols, token_references) =
            reference_token_index(source, &mut tokens, &names, &facts, &local_index);
        let parameter_hint_count = resolve_calls(&mut facts.calls, &token_symbols, &facts.symbols);
        let calls = index_calls(
            facts.calls,
            &facts.references,
            &facts.segments,
            &token_references,
            facts.symbols.len() + facts.bindings.len(),
        );
        let references = index_references(
            facts.references,
            token_references,
            facts.symbols.len() + facts.bindings.len(),
            names.ranges.len(),
        );
        let (completion_code_end, completion_eof_bindings) =
            completion_bindings_at_eof(&tokens, &facts.bindings, &facts.symbols);
        let parameter_hints = build_parameter_hints(
            &calls.calls,
            &facts.arguments,
            &facts.symbols,
            parameter_hint_count,
        );
        for (name, id) in constructor_targets {
            if let Some(id) = id
                && symbol_by_name[name.0].value().is_none()
            {
                symbol_by_name[name.0] = IndexSlot::some(id.0);
            }
        }
        let mut index = Self {
            tokens: tokens.into_boxed_slice(),
            semantic_token_count,
            parameter_hints: parameter_hints.into_boxed_slice(),
            names,
            imports: facts.imports.into_boxed_slice(),
            symbols: facts.symbols.into_boxed_slice(),
            bindings: facts.bindings.into_boxed_slice(),
            local_index,
            completion_code_end,
            completion_eof_bindings,
            sites: facts.sites.into_boxed_slice(),
            scopes: facts.scopes.into_boxed_slice(),
            segments: facts.segments.into_boxed_slice(),
            references: references.references.into_boxed_slice(),
            reference_source_indices: references.source_indices.into_boxed_slice(),
            reference_spans: references.by_symbol_spans.into_boxed_slice(),
            name_reference_indices: references.by_name_indices.into_boxed_slice(),
            name_reference_spans: references.by_name_spans.into_boxed_slice(),
            token_references: references.token_references.into_boxed_slice(),
            calls: calls.calls.into_boxed_slice(),
            call_arguments: facts.arguments.into_boxed_slice(),
            call_separators: facts.separators.into_boxed_slice(),
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
        index.completion = completion::CompletionIndex::build(
            source,
            &index,
            facts.completion_patterns,
            facts.completion_imports,
        );
        index
    }

    #[must_use]
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    pub(crate) const fn semantic_token_count(&self) -> usize {
        self.semantic_token_count
    }

    pub(crate) fn parameter_hints(&self, range: TextRange) -> impl Iterator<Item = InlayHint> {
        let start = self
            .parameter_hints
            .partition_point(|hint| hint.position < range.start);
        let hints = &self.parameter_hints[start..];
        let end = hints.partition_point(|hint| hint.position <= range.end);
        hints[..end].iter().map(|hint| InlayHint {
            position: hint.position,
            parameter_name: self.bindings[hint.binding].name_range,
        })
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
    pub fn declaration_sites(&self) -> &[DeclarationSite] {
        &self.sites
    }

    #[must_use]
    pub fn declaration_site(&self, id: SymbolId) -> Option<&DeclarationSite> {
        self.sites.get(id.0)
    }

    #[must_use]
    pub fn scopes(&self) -> &[IndexedScope] {
        &self.scopes
    }

    #[must_use]
    pub fn bindings(&self) -> &[IndexedBinding] {
        &self.bindings
    }

    #[must_use]
    pub fn binding_named_at(&self, name: NameId, offset: usize) -> Option<&IndexedBinding> {
        self.local_index
            .lookup(name, offset)
            .map(|id| &self.bindings[id.0])
    }

    pub fn visible_bindings(&self, offset: usize) -> impl Iterator<Item = &IndexedBinding> {
        self.local_index.visible(&self.bindings, offset)
    }

    #[must_use]
    pub fn all_references(&self) -> impl ExactSizeIterator<Item = &Reference> {
        self.reference_source_indices
            .iter()
            .map(|index| &self.references[*index])
    }

    #[must_use]
    pub fn reference_segments(&self, reference: &Reference) -> &[NameSegment] {
        &self.segments[reference.segment_span.start..reference.segment_span.end]
    }

    #[must_use]
    pub fn parameters(&self, symbol: &IndexedSymbol) -> &[IndexedBinding] {
        &self.bindings[symbol.parameter_span.start..symbol.parameter_span.end]
    }

    /// Caret visibility includes the final incomplete body and token-end insertion
    /// points without widening any semantic occurrence or lexical scope range.
    pub fn bindings_at(&self, offset: usize) -> impl Iterator<Item = &IndexedBinding> {
        let eof = offset >= self.completion_code_end;
        let previous = self
            .tokens
            .partition_point(|token| token.range.start < offset)
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index));
        let offset = if !eof && previous.is_some_and(|token| token.range.end == offset) {
            offset.saturating_sub(1)
        } else {
            offset
        };
        self.visible_bindings(offset).chain(
            self.completion_eof_bindings
                .iter()
                .filter(move |_| eof)
                .map(|id| &self.bindings[id.0]),
        )
    }

    #[inline]
    #[must_use]
    pub fn binding_type_range(&self, _source: &str, id: SymbolId) -> Option<TextRange> {
        self.binding_by_id(id)?.annotation
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

    pub(super) fn folding_lines(&self) -> impl ExactSizeIterator<Item = (TextRange, usize)> {
        self.lines.iter().map(|line| (line.code, line.indent))
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
        self.names
            .facts
            .get(name.0)
            .is_some_and(|facts| facts.semantic_type == 15)
    }

    #[must_use]
    pub fn is_keyword_text(name: &str) -> bool {
        parser::Tag::classify(name.as_bytes()).is_semantic_keyword()
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
    pub(crate) fn symbol_id_by_name(&self, name: NameId) -> Option<SymbolId> {
        self.symbol_by_name
            .get(name.0)
            .copied()
            .and_then(IndexSlot::value)
            .map(SymbolId)
    }

    #[must_use]
    pub fn symbol_by_name(&self, name: NameId) -> Option<&IndexedSymbol> {
        self.symbol_id_by_name(name).map(|id| &self.symbols[id.0])
    }

    #[must_use]
    pub fn constructor_by_name(&self, name: NameId) -> Option<&IndexedConstructor> {
        let index = self.auxiliary.data.as_deref()?.constructors.as_ref()?;
        let (constructor, duplicate) = if index.name_indices.is_empty() {
            let position = index.entries.partition_point(|entry| entry.name.0 < name.0);
            let constructor = index.entries.get(position)?;
            let duplicate = index
                .entries
                .get(position + 1)
                .is_some_and(|entry| entry.name == name);
            (constructor, duplicate)
        } else {
            let position = index
                .name_indices
                .partition_point(|entry| index.entries[*entry].name.0 < name.0);
            let constructor = &index.entries[*index.name_indices.get(position)?];
            let duplicate = index
                .name_indices
                .get(position + 1)
                .is_some_and(|entry| index.entries[*entry].name == name);
            (constructor, duplicate)
        };
        (!duplicate && constructor.name == name).then_some(constructor)
    }

    /// Look up the constructor's parent by exact declaration identity, including
    /// duplicate written names. Entries retain parser declaration/SymbolId order.
    #[must_use]
    pub fn constructor_by_symbol(&self, symbol: SymbolId) -> Option<&IndexedConstructor> {
        let constructors = &self
            .auxiliary
            .data
            .as_deref()?
            .constructors
            .as_ref()?
            .entries;
        constructors
            .binary_search_by_key(&symbol.0, |constructor| constructor.symbol.0)
            .ok()
            .map(|index| &constructors[index])
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

    pub fn references(&self, id: SymbolId) -> std::slice::Iter<'_, Reference> {
        let span = self.reference_spans.get(id.0).copied().unwrap_or_default();
        self.references[span.start..span.end].iter()
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
        self.all_references().filter(move |reference| {
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
        let span = self
            .calls_from_spans
            .get(symbol.0)
            .copied()
            .unwrap_or_default();
        self.calls_from_indices[span.start..span.end]
            .iter()
            .map(|index| &self.calls[*index])
    }

    pub fn calls_to(&self, symbol: SymbolId) -> impl Iterator<Item = &CallSite> {
        let span = self
            .calls_to_spans
            .get(symbol.0)
            .copied()
            .unwrap_or_default();
        self.calls_to_indices[span.start..span.end]
            .iter()
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
    semantic_token_count: usize,
    names: NameTable,
    diagnostics: Vec<SyntaxDiagnostic>,
    delimiter_pairs: Vec<DelimiterPair>,
    delimiter_context: Vec<IndexSlot>,
    line_starts: Vec<usize>,
    line_ascii: Vec<u64>,
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
    for count in &mut counts {
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

fn constructor_index(entries: Vec<IndexedConstructor>) -> Option<ConstructorIndex> {
    if entries.is_empty() {
        return None;
    }
    let name_indices = if entries
        .windows(2)
        .all(|pair| pair[0].name.0 <= pair[1].name.0)
    {
        Vec::new()
    } else {
        let mut indices: Vec<_> = (0..entries.len()).collect();
        indices.sort_unstable_by_key(|index| (entries[*index].name.0, *index));
        indices
    };
    Some(ConstructorIndex {
        entries: entries.into_boxed_slice(),
        name_indices: name_indices.into_boxed_slice(),
    })
}

fn resolve_calls(
    calls: &mut [CallSite],
    token_symbols: &[IndexSlot],
    symbols: &[IndexedSymbol],
) -> usize {
    let mut hint_count = 0;
    for call in calls {
        call.callee = token_symbols[call.callee_token.0].value().map(SymbolId);
        if let Some(symbol) = call
            .callee
            .and_then(|id| symbols.get(id.0))
            .filter(|symbol| symbol.kind == SymbolKind::Function)
        {
            hint_count += (call.argument_indices.end - call.argument_indices.start)
                .min(symbol.parameter_span.end - symbol.parameter_span.start);
        }
    }
    hint_count
}

fn index_calls(
    calls: Vec<CallSite>,
    references: &[Reference],
    segments: &[NameSegment],
    token_references: &[IndexSlot],
    id_count: usize,
) -> CallIndex {
    let mut by_open = vec![IndexSlot::EMPTY; token_references.len()];
    let mut by_name_token = vec![IndexSlot::EMPTY; token_references.len()];
    for (index, call) in calls.iter().enumerate() {
        by_open[call.open.0] = IndexSlot::some(index);
        if let Some(reference) = token_references[call.callee_token.0]
            .value()
            .map(|index| &references[index])
        {
            for segment in &segments[reference.segment_span.start..reference.segment_span.end] {
                by_name_token[segment.token.0] = IndexSlot::some(index);
            }
        }
    }
    let (from_indices, from_spans) =
        compact_groups(&calls, id_count, |call| call.caller.map(|id| id.0));
    let (to_indices, to_spans) =
        compact_groups(&calls, id_count, |call| call.callee.map(|id| id.0));
    CallIndex {
        calls,
        by_open,
        by_name_token,
        from_indices,
        from_spans,
        to_indices,
        to_spans,
    }
}

fn reference_token_index(
    source: &str,
    tokens: &mut [Token],
    names: &NameTable,
    facts: &parser::FileFacts,
    local_index: &locals::LocalIndex,
) -> (Vec<IndexSlot>, Vec<IndexSlot>) {
    let mut symbols = vec![IndexSlot::EMPTY; tokens.len()];
    let mut occurrences = vec![IndexSlot::EMPTY; tokens.len()];
    for (index, reference) in facts.references.iter().enumerate() {
        if reference.synthetic {
            continue;
        }
        let resolved_type = reference.resolved.and_then(|id| {
            if let Some(symbol) = facts.symbols.get(id.0) {
                Some(if symbol.kind == SymbolKind::Function {
                    12
                } else {
                    1
                })
            } else {
                id.0.checked_sub(facts.symbols.len())
                    .filter(|index| *index < facts.bindings.len())
                    .map(|_| 8)
            }
        });
        // Cache the exact snapshot-local presentation while visiting the same
        // segments needed for identity indexing. Recovery coloring never invents
        // a target and cannot override a grammar-owned lexical binding.
        let name_range = names.range(reference.name);
        let recovery_call = resolved_type.is_none()
            && matches!(reference.role, ReferenceRole::Call | ReferenceRole::Offload)
            && local_index
                .lookup(reference.full_name, tokens[reference.token.0].range.start)
                .is_none()
            && source[name_range.start..name_range.end]
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase);
        for segment in &facts.segments[reference.segment_span.start..reference.segment_span.end] {
            symbols[segment.token.0] = IndexSlot::from_option(reference.resolved.map(|id| id.0));
            occurrences[segment.token.0] = IndexSlot::some(index);
            let token_type = resolved_type.unwrap_or_else(|| {
                if recovery_call && segment.token == reference.token {
                    12
                } else if names
                    .for_token(segment.token)
                    .is_some_and(|name| names.facts[name.0].capitalized)
                {
                    1
                } else {
                    8
                }
            });
            let token = &mut tokens[segment.token.0];
            token.flags = token.flags.with_semantic_type(Some(token_type));
        }
    }
    (symbols, occurrences)
}

fn completion_bindings_at_eof(
    tokens: &[Token],
    bindings: &[IndexedBinding],
    symbols: &[IndexedSymbol],
) -> (usize, Box<[BindingId]>) {
    let code_end = tokens
        .iter()
        .rev()
        .find(|token| token.kind != TokenKind::Comment)
        .map_or(0, |token| token.range.end);
    let mut visible: Vec<_> = bindings
        .iter()
        .filter(|binding| {
            !binding.anonymous
                && binding.scope.end == code_end
                && symbols[binding.owner.0].kind == SymbolKind::Function
        })
        .map(|binding| binding.binding_id)
        .collect();
    visible.sort_unstable_by_key(|id| (bindings[id.0].name.0, std::cmp::Reverse(id.0)));
    visible.dedup_by_key(|id| bindings[id.0].name);
    (code_end, visible.into_boxed_slice())
}

fn build_parameter_hints(
    calls: &[CallSite],
    arguments: &[TextRange],
    symbols: &[IndexedSymbol],
    capacity: usize,
) -> Vec<ParameterHint> {
    let mut hints = Vec::with_capacity(capacity);
    for call in calls {
        let Some(symbol) = call
            .callee
            .and_then(|id| symbols.get(id.0))
            .filter(|symbol| symbol.kind == SymbolKind::Function)
        else {
            continue;
        };
        for (argument, binding) in arguments[call.argument_indices.start..call.argument_indices.end]
            .iter()
            .zip(symbol.parameter_span.start..symbol.parameter_span.end)
        {
            hints.push(ParameterHint {
                position: argument.start,
                binding,
            });
        }
    }
    hints.sort_unstable_by_key(|hint| hint.position);
    hints
}

fn index_references(
    mut references: Vec<Reference>,
    mut token_references: Vec<IndexSlot>,
    id_count: usize,
    name_count: usize,
) -> ReferenceIndex {
    // Store each target's wide records contiguously: callers copying a complete
    // result can use one bulk copy instead of copying every occurrence separately.
    // The final group retains unresolved/invalid targets without exposing them
    // through a valid SymbolId. All other traversals preserve parser source order.
    let (mut order, mut by_symbol_spans) = compact_groups(&references, id_count + 1, |reference| {
        Some(reference.resolved.map_or(id_count, |id| id.0.min(id_count)))
    });
    let _ = by_symbol_spans.pop();
    let (mut by_name_indices, by_name_spans) =
        compact_groups(&references, name_count, |reference| {
            Some(reference.full_name.0)
        });
    let mut source_indices = vec![0; references.len()];
    for (destination, &source) in order.iter().enumerate() {
        source_indices[source] = destination;
    }
    for slot in &mut token_references {
        if let Some(source) = slot.value() {
            *slot = IndexSlot::some(source_indices[source]);
        }
    }
    for index in &mut by_name_indices {
        *index = source_indices[*index];
    }
    // Consume a source -> destination permutation with swaps; do not allocate a
    // second wide-record arena. Keep the immutable inverse for source traversals.
    order.copy_from_slice(&source_indices);
    for source in 0..order.len() {
        while order[source] != source {
            let destination = order[source];
            references.swap(source, destination);
            order.swap(source, destination);
        }
    }
    ReferenceIndex {
        references,
        source_indices,
        by_symbol_spans,
        by_name_indices,
        by_name_spans,
        token_references,
    }
}

fn resolve_standalone_references(
    facts: &mut parser::FileFacts,
    definitions: &[IndexSlot],
    constructors: &HashMap<NameId, Option<SymbolId>>,
) {
    for reference in &mut facts.references {
        if reference.uncertain {
            reference.resolved = None;
            reference.local_binding = None;
            continue;
        }
        if reference.resolved.is_none() && reference.kind != ReferenceKind::Declaration {
            let target = if reference.role == ReferenceRole::Constructor {
                constructors.get(&reference.full_name).copied().flatten()
            } else {
                definitions
                    .get(reference.full_name.0)
                    .copied()
                    .and_then(IndexSlot::value)
                    .map(SymbolId)
            };
            reference.resolved = target.filter(|id| facts.sites[id.0].linkable);
        }
    }
}

fn standalone_symbol_table(symbols: &[IndexedSymbol], name_count: usize) -> Box<[IndexSlot]> {
    if symbols.is_empty() {
        return Box::new([]);
    }
    let mut targets = vec![IndexSlot::EMPTY; name_count].into_boxed_slice();
    for symbol in symbols {
        if symbol.kind != SymbolKind::Constructor && targets[symbol.name.0].value().is_none() {
            targets[symbol.name.0] = IndexSlot::some(symbol.id.0);
        }
    }
    targets
}

fn standalone_constructor_targets(
    constructors: &[IndexedConstructor],
) -> HashMap<NameId, Option<SymbolId>> {
    let mut targets = HashMap::with_capacity(constructors.len());
    for constructor in constructors {
        targets
            .entry(constructor.name)
            .and_modify(|target| *target = None)
            .or_insert(Some(constructor.symbol));
    }
    targets
}
