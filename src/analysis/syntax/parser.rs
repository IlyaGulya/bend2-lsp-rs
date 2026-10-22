use super::{
    BindingId, CallSite, DeclarationKind, DeclarationSite, IndexSlot, IndexedBinding,
    IndexedConstructor, IndexedImport, IndexedReference, IndexedScope, IndexedSymbol, LineIndex,
    NameId, NameSegment, NameTable, Reference, ReferenceKind, ReferenceRole, ScopeId, SymbolId,
    SymbolKind, SyntaxLine, TextRange, Token, TokenId, TokenKind,
};
use std::{cell::Cell, collections::HashSet};

pub(super) struct FileFacts {
    pub imports: Vec<IndexedImport>,
    pub symbols: Vec<IndexedSymbol>,
    pub sites: Vec<DeclarationSite>,
    pub constructors: Vec<IndexedConstructor>,
    pub scopes: Vec<IndexedScope>,
    pub bindings: Vec<IndexedBinding>,
    pub references: Vec<IndexedReference>,
    pub segments: Vec<NameSegment>,
    pub calls: Vec<CallSite>,
    pub arguments: Vec<TextRange>,
    pub separators: Vec<usize>,
    pub completion_imports: Vec<TextRange>,
    pub completion_patterns: Vec<super::completion::PatternSite>,
}

// ECMAScript's \\s / String.trim set, used by the native line-based import loader.
fn import_whitespace(character: char) -> bool {
    matches!(character,
        '\u{0009}'..='\u{000D}' | ' ' | '\u{00A0}' | '\u{1680}'
        | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}'
        | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

#[derive(Clone, Copy)]
struct Expression {
    range: TextRange,
    reference: Option<usize>,
    references: TextRange,
    write: Option<usize>,
    pattern: bool,
}

enum Head {
    Name(usize),
    Pattern,
    Other,
}

#[derive(Clone, Copy)]
struct ScopeFrame {
    scope: ScopeId,
    environment_start: usize,
}

struct EnvironmentEntry {
    binding: usize,
    previous: IndexSlot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tag {
    Other,
    Import,
    As,
    Unsafe,
    Def,
    Law,
    Type,
    Universe,
    Data,
    Kind,
    Quant,
    Do,
    Match,
    Case,
    Return,
    For,
    Exs,
    Where,
    Is,
    At,
    Question,
    Colon,
    Comma,
    Semicolon,
    Dot,
    Backslash,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,
    OpenBrace,
    CloseBrace,
    Arrow,
    Bind,
    Lambda,
    Equal,
    EqualEqual,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Pair,
    Concat,
    Parallel,
    Or,
    And,
    LogicalOr,
    LogicalAnd,
    BitOr,
    BitXor,
    BitAnd,
    ShiftLeft,
    ShiftRight,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Tilde,
    Bang,
}

impl Tag {
    pub(super) fn classify(text: &[u8]) -> Self {
        match text {
            b"import" => Self::Import,
            b"as" => Self::As,
            b"unsafe" => Self::Unsafe,
            b"def" => Self::Def,
            b"law" => Self::Law,
            b"type" => Self::Type,
            b"Type" => Self::Universe,
            b"Data" => Self::Data,
            b"Kind" => Self::Kind,
            b"Quant" => Self::Quant,
            b"do" => Self::Do,
            b"match" => Self::Match,
            b"case" => Self::Case,
            b"return" => Self::Return,
            b"for" => Self::For,
            b"exs" => Self::Exs,
            b"where" => Self::Where,
            b"is" => Self::Is,
            b"@" => Self::At,
            b"?" => Self::Question,
            b":" => Self::Colon,
            b"," => Self::Comma,
            b";" => Self::Semicolon,
            b"." => Self::Dot,
            b"\\" => Self::Backslash,
            b"(" => Self::OpenParen,
            b")" => Self::CloseParen,
            b"[" => Self::OpenBracket,
            b"]" => Self::CloseBracket,
            b"{" => Self::OpenBrace,
            b"}" => Self::CloseBrace,
            b"->" => Self::Arrow,
            b"<-" => Self::Bind,
            b"=>" => Self::Lambda,
            b"=" => Self::Equal,
            b"==" => Self::EqualEqual,
            b"!=" => Self::NotEqual,
            b"<" => Self::Less,
            b"<=" => Self::LessEqual,
            b">" => Self::Greater,
            b">=" => Self::GreaterEqual,
            b"<>" => Self::Pair,
            b"++" => Self::Concat,
            b"<&>" => Self::Parallel,
            b"|" => Self::Or,
            b"&" => Self::And,
            b"||" => Self::LogicalOr,
            b"&&" => Self::LogicalAnd,
            b".|." => Self::BitOr,
            b".^." => Self::BitXor,
            b".&." => Self::BitAnd,
            b"<<" => Self::ShiftLeft,
            b">>" => Self::ShiftRight,
            b"+" => Self::Plus,
            b"-" => Self::Minus,
            b"*" => Self::Star,
            b"/" => Self::Slash,
            b"%" => Self::Percent,
            b"^" => Self::Caret,
            b"~" => Self::Tilde,
            b"!" => Self::Bang,
            _ => Self::Other,
        }
    }

    pub(super) fn is_semantic_keyword(self) -> bool {
        matches!(
            self,
            Self::Def
                | Self::Law
                | Self::Type
                | Self::Is
                | Self::Match
                | Self::Case
                | Self::Do
                | Self::Return
                | Self::For
                | Self::Exs
                | Self::Where
                | Self::Import
                | Self::Universe
                | Self::Data
                | Self::Kind
                | Self::Quant
        )
    }
}

#[derive(Clone, Copy)]
struct ParserToken {
    index: usize,
    tag: Tag,
}

fn significant_tokens(
    source: &str,
    tokens: &[Token],
    names: &NameTable,
) -> (Vec<ParserToken>, usize) {
    let bytes = source.as_bytes();
    let mut significant = Vec::with_capacity(tokens.len());
    let mut identifier_count = 0;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::Comment {
            continue;
        }
        let range = token.range;
        let tag = match token.kind {
            TokenKind::Identifier => {
                identifier_count += 1;
                names
                    .for_token(TokenId(index))
                    .map_or(Tag::Other, |name| names.facts[name.0].grammar)
            }
            TokenKind::Operator | TokenKind::Delimiter | TokenKind::Punctuation => {
                Tag::classify(&bytes[range.start..range.end])
            }
            _ => Tag::Other,
        };
        // Only a standalone, adjacent dot joins identifier segments. Dotted
        // operators (.|., .^., .&.) must not hide a neighboring keyword.
        let dotted = tag != Tag::Other
            && token.kind == TokenKind::Identifier
            && (index
                .checked_sub(1)
                .and_then(|index| tokens.get(index))
                .is_some_and(|previous| {
                    previous.range.end == range.start
                        && previous.range.end - previous.range.start == 1
                        && bytes[previous.range.start] == b'.'
                })
                || tokens.get(index + 1).is_some_and(|next| {
                    next.range.start == range.end
                        && next.range.end - next.range.start == 1
                        && bytes[next.range.start] == b'.'
                }));
        significant.push(ParserToken {
            index,
            tag: if dotted { Tag::Other } else { tag },
        });
    }
    (significant, identifier_count)
}

struct Parser<'a> {
    source: &'a str,
    tokens: &'a mut [Token],
    lines: &'a [SyntaxLine],
    line_index: &'a LineIndex,
    names: &'a mut NameTable,
    significant: Vec<ParserToken>,
    position: usize,
    limit: usize,
    owner: Option<SymbolId>,
    scope: Option<ScopeId>,
    environment: Vec<IndexSlot>,
    environment_stack: Vec<EnvironmentEntry>,
    parameter_bindings: Vec<bool>,
    constructor_names: HashSet<NameId>,
    scope_bindings: Vec<Option<usize>>,
    binding_next: Vec<Option<usize>>,
    pattern_candidates: Vec<bool>,
    declaration_references: Vec<(usize, SymbolId)>,
    argument_work: Vec<TextRange>,
    separator_work: Vec<usize>,
    in_type: bool,
    uncertain: bool,
    depth: usize,
    line_cache: Cell<usize>,
    newline_cache: Cell<(usize, bool)>,
    facts: FileFacts,
}

pub(super) fn parse(
    source: &str,
    tokens: &mut [Token],
    lines: &[SyntaxLine],
    line_index: &LineIndex,
    names: &mut NameTable,
) -> FileFacts {
    let (significant, identifier_count) = significant_tokens(source, tokens, names);
    let limit = significant.len();
    let environment = vec![IndexSlot::EMPTY; names.ranges.len()];
    let mut parser = Parser {
        source,
        tokens,
        lines,
        line_index,
        names,
        significant,
        position: 0,
        limit,
        owner: None,
        scope: None,
        environment,
        environment_stack: Vec::new(),
        parameter_bindings: Vec::new(),
        constructor_names: HashSet::new(),
        scope_bindings: Vec::new(),
        binding_next: Vec::new(),
        pattern_candidates: Vec::with_capacity(identifier_count),
        declaration_references: Vec::new(),
        argument_work: Vec::new(),
        separator_work: Vec::new(),
        in_type: false,
        uncertain: false,
        depth: 0,
        line_cache: Cell::new(0),
        newline_cache: Cell::new((usize::MAX, false)),
        facts: FileFacts {
            imports: Vec::new(),
            symbols: Vec::new(),
            sites: Vec::new(),
            constructors: Vec::new(),
            scopes: Vec::new(),
            bindings: Vec::new(),
            references: Vec::with_capacity(identifier_count),
            segments: Vec::with_capacity(identifier_count),
            calls: Vec::new(),
            arguments: Vec::new(),
            separators: Vec::new(),
            completion_imports: Vec::new(),
            completion_patterns: Vec::new(),
        },
    };
    parser.imports();
    parser.declarations();
    parser.finish()
}

impl Parser<'_> {
    fn token(&self) -> Option<TokenId> {
        (self.position < self.limit).then(|| TokenId(self.significant[self.position].index))
    }

    fn tag(&self) -> Tag {
        if self.position < self.limit {
            self.significant[self.position].tag
        } else {
            Tag::Other
        }
    }

    fn at(&self, tag: Tag) -> bool {
        self.tag() == tag
    }

    fn declaration_keyword_at(&self, position: usize) -> bool {
        self.significant
            .get(position)
            .is_some_and(|token| matches!(token.tag, Tag::Def | Tag::Law | Tag::Type))
    }

    fn bump(&mut self) -> Option<TokenId> {
        let token = self.token()?;
        self.position += 1;
        Some(token)
    }

    fn take(&mut self, tag: Tag) -> bool {
        if self.at(tag) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn eat(&mut self, tag: Tag) -> bool {
        let found = self.take(tag);
        self.uncertain |= !found;
        found
    }

    fn offset(&self) -> usize {
        self.token()
            .map_or_else(|| self.end(), |token| self.tokens[token.0].range.start)
    }

    fn end(&self) -> usize {
        self.position
            .checked_sub(1)
            .and_then(|position| self.significant.get(position))
            .map_or(0, |token| self.tokens[token.index].range.end)
    }

    fn line(&self, offset: usize) -> usize {
        let cached = self.line_cache.get();
        let current = self.lines[cached].content.start;
        if offset >= current {
            if self
                .lines
                .get(cached + 1)
                .is_none_or(|line| offset < line.content.start)
            {
                return cached;
            }
            if self
                .lines
                .get(cached + 2)
                .is_none_or(|line| offset < line.content.start)
            {
                self.line_cache.set(cached + 1);
                return cached + 1;
            }
        } else if cached > 0 && offset >= self.lines[cached - 1].content.start {
            self.line_cache.set(cached - 1);
            return cached - 1;
        }
        let line = self
            .lines
            .partition_point(|line| line.content.start <= offset)
            .saturating_sub(1);
        self.line_cache.set(line);
        line
    }

    fn column(&self) -> usize {
        self.column_at(self.offset())
    }

    fn column_at(&self, offset: usize) -> usize {
        let line = self.line(offset);
        if self.line_index.is_ascii_line(line) {
            return offset - self.lines[line].content.start + 1;
        }
        usize::try_from(self.line_index.position(self.source, offset).1)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }

    fn newline(&self) -> bool {
        let Some(token) = self.token() else {
            return false;
        };
        let cached = self.newline_cache.get();
        if cached.0 == self.position {
            return cached.1;
        }
        let newline =
            self.source.as_bytes()[self.end()..self.tokens[token.0].range.start].contains(&b'\n');
        self.newline_cache.set((self.position, newline));
        newline
    }

    fn imports(&mut self) {
        for line_number in 0..self.lines.len() {
            let line = self.lines[line_number];
            let mut alias_keyword = None;
            let raw = &self.source[line.content.start..line.content.end];
            let code = raw.trim_start_matches(import_whitespace);
            if !code.is_empty() && !code.starts_with('#') {
                let Some(rest) = code.strip_prefix("import").filter(|rest| {
                    rest.is_empty() || rest.chars().next().is_some_and(import_whitespace)
                }) else {
                    break;
                };
                let spaced = rest.chars().next().is_some_and(import_whitespace);
                let path = rest.trim_start_matches(import_whitespace);
                let start = line.content.end - path.len();
                let width = path.find(import_whitespace).unwrap_or(path.len());
                let end = start + width;
                if spaced {
                    self.facts
                        .completion_imports
                        .push(TextRange::new(start, end));
                }
                let mut tail = path[width..].trim_start_matches(import_whitespace);
                let mut alias = None;
                let mut alias_clause = false;
                if let Some(after_as) = tail
                    .strip_prefix("as")
                    .filter(|rest| rest.chars().next().is_some_and(import_whitespace))
                {
                    alias_clause = true;
                    alias_keyword = Some(line.content.end - tail.len());
                    tail = after_as.trim_start_matches(import_whitespace);
                    if tail
                        .as_bytes()
                        .first()
                        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                    {
                        let width = tail
                            .bytes()
                            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                            .count();
                        let start = line.content.end - tail.len();
                        alias = Some(TextRange::new(start, start + width));
                        tail = tail[width..].trim_start_matches(import_whitespace);
                    }
                }
                let valid_syntax = spaced
                    && start < end
                    && (alias.is_some() || &self.source[start..end] == "Base")
                    && (!alias_clause || alias.is_some())
                    && (tail.is_empty() || tail.starts_with('#'));
                self.facts.imports.push(IndexedImport {
                    path: TextRange::new(start, end),
                    alias,
                    valid_syntax,
                });
            }
            // The loader removes the whole header line before term parsing.
            // Header paths and aliases never become term occurrences.
            while let Some(token) = self
                .token()
                .filter(|token| self.tokens[token.0].range.start < line.full_end)
            {
                if Some(self.tokens[token.0].range.start) == alias_keyword {
                    let flags = self.tokens[token.0].flags;
                    self.tokens[token.0].flags = super::TokenFlags(
                        flags.with_semantic_type(Some(15)).0
                            | super::TokenFlags::CONTEXTUAL_KEYWORD,
                    );
                }
                let _ = self.bump();
            }
        }
    }

    fn declarations(&mut self) {
        let file_limit = self.significant.len();
        while self.position < file_limit {
            self.limit = file_limit;
            let begin = self.position;
            let decorated = self.take(Tag::At);
            if decorated {
                if self.at(Tag::Unsafe)
                    && let Some(token) = self.token()
                {
                    let flags = self.tokens[token.0].flags;
                    self.tokens[token.0].flags = super::TokenFlags(
                        flags.with_semantic_type(Some(15)).0
                            | super::TokenFlags::CONTEXTUAL_KEYWORD,
                    );
                }
                self.eat(Tag::Unsafe);
            }
            let kind = match self.tag() {
                Tag::Def => DeclarationKind::Def,
                Tag::Law => DeclarationKind::Law,
                Tag::Type => DeclarationKind::Type,
                _ => {
                    self.limit = (self.position + 1..file_limit)
                        .find(|position| self.declaration_keyword_at(*position))
                        .unwrap_or(file_limit);
                    self.recover_expressions();
                    continue;
                }
            };
            let _ = self.bump();
            self.limit = (self.position..file_limit)
                .find(|position| self.declaration_keyword_at(*position))
                .unwrap_or(file_limit);
            // A following decorator belongs to the next site, including under recovery.
            if self.limit >= 2 {
                let previous = self.significant[self.limit - 1];
                let before = self.significant[self.limit - 2];
                if previous.tag == Tag::Unsafe && before.tag == Tag::At {
                    self.limit -= 2;
                }
            }
            self.uncertain = false;
            let site_start = self.facts.sites.len();
            self.declaration(begin, kind);
            if self.position <= begin {
                self.position = begin + 1;
            }
            if self.position < self.limit {
                if let Some(site) = self.facts.sites.get_mut(site_start) {
                    site.uncertain = true;
                }
                self.recover_expressions();
            }
        }
        self.limit = file_limit;
    }

    fn recover_expressions(&mut self) {
        self.owner = None;
        self.scope = None;
        self.in_type = false;
        self.uncertain = true;
        while self.position < self.limit {
            let before = self.position;
            let _ = self.expression(0);
            if self.position == before {
                let _ = self.bump();
            }
        }
    }

    fn push_declaration(
        &mut self,
        reference: usize,
        kind: DeclarationKind,
        range: TextRange,
    ) -> SymbolId {
        let symbol = SymbolId(self.facts.symbols.len());
        let name = self.facts.references[reference].full_name;
        let name_range = self.facts.references[reference].full_range;
        let symbol_kind = match kind {
            DeclarationKind::Type => SymbolKind::Struct,
            DeclarationKind::Constructor => SymbolKind::Constructor,
            DeclarationKind::Def | DeclarationKind::Law => SymbolKind::Function,
        };
        self.facts.symbols.push(IndexedSymbol {
            id: symbol,
            name,
            name_range,
            detail_range: range,
            parameter_span: TextRange::default(),
            scope: range,
            kind: symbol_kind,
        });
        self.facts.sites.push(DeclarationSite {
            symbol,
            name,
            kind,
            name_range,
            full_range: range,
            signature_range: range,
            body_range: None,
            doc_range: self.documentation(range.start),
            uncertain: false,
            template_parameter_count: 0,
            law_fill_compatible: false,
            linkable: true,
        });
        self.mark_declaration(reference, symbol);
        symbol
    }

    fn declaration(&mut self, begin: usize, kind: DeclarationKind) {
        let Some(reference) = self.atom() else { return };
        let name_range = self.facts.references[reference].full_range;
        let start = self.tokens[self.significant[begin].index].range.start;
        let symbol = self.push_declaration(reference, kind, TextRange::new(start, self.end()));
        self.owner = Some(symbol);
        let frame = self.open_scope();
        match kind {
            DeclarationKind::Def => self.definition(symbol),
            DeclarationKind::Law => self.law(symbol),
            DeclarationKind::Type => self.datatype(symbol),
            DeclarationKind::Constructor => {}
        }
        let end = self.end().max(name_range.end);
        self.close_scope(frame, end);
        self.facts.sites[symbol.0].full_range.end = end;
        self.facts.sites[symbol.0].uncertain |= self.uncertain;
        self.facts.symbols[symbol.0].scope.end = end.max(self.facts.symbols[symbol.0].scope.start);
        self.owner = None;
    }

    fn mark_declaration(&mut self, reference: usize, symbol: SymbolId) {
        self.nonlocal_role(reference, ReferenceRole::Binding);
        let occurrence = &mut self.facts.references[reference];
        occurrence.reference.kind = ReferenceKind::Declaration;
        occurrence.reference.enclosing = None;
        self.declaration_references.push((reference, symbol));
    }

    fn definition(&mut self, symbol: SymbolId) {
        if !self.definition_header(symbol) {
            return;
        }
        let body_start = self.offset();
        self.facts.symbols[symbol.0].scope.start = body_start;
        if self.at(Tag::Import) {
            while self.take(Tag::Import) {
                if self
                    .token()
                    .is_some_and(|token| self.tokens[token.0].kind == TokenKind::StringLiteral)
                {
                    let _ = self.bump();
                } else {
                    self.uncertain = true;
                    break;
                }
            }
            self.facts.sites[symbol.0].body_range = Some(TextRange::new(body_start, self.end()));
        } else if self.token().is_some() {
            let _ = self.body(0);
            if self.end() >= body_start {
                self.facts.sites[symbol.0].body_range =
                    Some(TextRange::new(body_start, self.end()));
            }
        } else {
            self.uncertain = true;
        }
    }

    fn definition_header(&mut self, symbol: SymbolId) -> bool {
        self.take(Tag::Question);
        if !self.take(Tag::OpenParen) {
            return self.recovered_definition_header(symbol);
        }
        let telescope = self.telescope(Tag::CloseParen, symbol, None);
        self.facts.sites[symbol.0].linkable &= telescope.is_some();
        let has_return = self.take(Tag::Arrow);
        if has_return {
            let _ = self.type_expression(0);
        }
        let header_end = self.end();
        self.facts.sites[symbol.0].signature_range.end = header_end;
        self.facts.symbols[symbol.0].detail_range.end = header_end;
        self.facts.sites[symbol.0].law_fill_compatible = !has_return && telescope == Some(true);
        if !self.eat(Tag::Colon) {
            self.facts.sites[symbol.0].linkable &= self.token().is_none();
            return false;
        }
        true
    }

    fn recovered_definition_header(&mut self, symbol: SymbolId) -> bool {
        self.facts.sites[symbol.0].uncertain = true;
        let mut header_end = self.end();
        if !self.eat(Tag::Colon) {
            self.facts.sites[symbol.0].linkable = false;
            return false;
        }
        // A colon on an unfinished non-telescope header can introduce the
        // written return term. Parse that term through the same grammar and
        // keep the next indented body; this is an explicit editor recovery,
        // never an alternative grammar for complete compiler input.
        if !self.newline() && self.token().is_some() {
            let _ = self.type_expression(0);
            header_end = self.end();
        }
        self.facts.sites[symbol.0].signature_range.end = header_end;
        self.facts.symbols[symbol.0].detail_range.end = header_end;
        true
    }

    fn telescope(
        &mut self,
        close: Tag,
        owner: SymbolId,
        mut first_quantity: Option<TextRange>,
    ) -> Option<bool> {
        let mut all_bare = true;
        let mut template_prefix = true;
        while self.token().is_some() {
            let leading_quantity = first_quantity.take();
            if leading_quantity.is_none() && self.take(close) {
                return Some(all_bare);
            }
            let before = self.position;
            let start = leading_quantity.map_or_else(|| self.offset(), |range| range.start);
            let template = close == Tag::CloseParen && self.take(Tag::Tilde);
            let quantified = leading_quantity.is_some()
                || !template && (self.take(Tag::Minus) || self.take(Tag::Plus));
            if template && !template_prefix {
                self.uncertain = true;
                self.facts.sites[owner.0].linkable = false;
            }
            template_prefix &= template;
            let Some(reference) = self.atom() else {
                self.uncertain = true;
                break;
            };
            let annotation = if self.take(Tag::Colon) {
                self.type_expression(0).map(|expression| expression.range)
            } else {
                None
            };
            if (template || quantified) && annotation.is_none() {
                self.uncertain = true;
                self.facts.sites[owner.0].linkable = false;
            }
            all_bare &= annotation.is_none() && !template && !quantified;
            let full_range = TextRange::new(start, self.end());
            self.bind(reference, annotation, full_range, true);
            if template {
                self.facts.sites[owner.0].template_parameter_count += 1;
            }
            self.take(Tag::Comma);
            if self.position <= before {
                let _ = self.bump();
            }
        }
        self.uncertain = true;
        None
    }

    fn law(&mut self, symbol: SymbolId) {
        if !self.eat(Tag::Colon) {
            self.facts.sites[symbol.0].linkable = false;
            return;
        }
        self.in_type = true;
        let mut leading_for = true;
        let mut template_prefix = true;
        while self.at(Tag::For) || self.at(Tag::Exs) {
            let universal = self.at(Tag::For);
            leading_for &= universal;
            let _ = self.bump();
            let start = self.offset();
            let template = universal && self.take(Tag::Tilde);
            if universal && !template && !self.take(Tag::Minus) {
                self.take(Tag::Plus);
            }
            if template && !template_prefix {
                self.uncertain = true;
                self.facts.sites[symbol.0].linkable = false;
            }
            template_prefix &= template;
            let Some(reference) = self.atom() else {
                self.uncertain = true;
                break;
            };
            self.eat(Tag::Colon);
            let annotation = self.expression(0).map(|expression| expression.range);
            let full_range = TextRange::new(start, self.end());
            self.bind(reference, annotation, full_range, leading_for);
            if self.take(Tag::Where) {
                let _ = self.expression(0);
            }
            if template {
                self.facts.sites[symbol.0].template_parameter_count += 1;
            }
        }
        let _ = self.body(0);
        self.in_type = false;
        self.facts.sites[symbol.0].signature_range.end = self.end();
        self.facts.symbols[symbol.0].detail_range.end = self.end();
    }

    fn datatype(&mut self, symbol: SymbolId) {
        let first_quantity = if self.at(Tag::Bind) {
            let start = self.offset();
            self.take(Tag::Bind);
            Some(TextRange::new(start + 1, self.end()))
        } else {
            None
        };
        if first_quantity.is_some() || self.take(Tag::Less) {
            self.facts.sites[symbol.0].linkable = self
                .telescope(Tag::Greater, symbol, first_quantity)
                .is_some();
        } else {
            self.take(Tag::Pair);
        }
        self.eat(Tag::Is);
        let _ = self.type_expression(0);
        self.facts.sites[symbol.0].signature_range.end = self.end();
        self.facts.symbols[symbol.0].detail_range.end = self.end();
        if !self.eat(Tag::Colon) {
            self.facts.sites[symbol.0].linkable = false;
            return;
        }
        let start = self.offset();
        while self
            .token()
            .is_some_and(|token| self.tokens[token.0].kind == TokenKind::Identifier)
        {
            let begin = self.position;
            let previous_owner = self.owner;
            let Some(reference) = self.atom() else { break };
            let name = self.facts.references[reference].full_name;
            let name_range = self.facts.references[reference].full_range;
            let constructor =
                self.push_declaration(reference, DeclarationKind::Constructor, name_range);
            self.owner = Some(constructor);
            let frame = self.open_scope();
            let complete = self.eat(Tag::OpenBrace)
                && self.telescope(Tag::CloseBrace, constructor, None).is_some();
            self.facts.sites[constructor.0].linkable = complete;
            let range = TextRange::new(name_range.start, self.end());
            self.close_scope(frame, range.end);
            self.facts.symbols[constructor.0].detail_range = range;
            self.facts.symbols[constructor.0].scope = range;
            self.facts.sites[constructor.0].full_range = range;
            self.facts.sites[constructor.0].signature_range = range;
            self.facts.sites[constructor.0].uncertain = self.uncertain;
            self.facts.constructors.push(IndexedConstructor {
                symbol: constructor,
                parent: symbol,
                name,
                name_range,
                range,
            });
            self.constructor_names.insert(name);
            self.owner = previous_owner;
            if self.position <= begin {
                break;
            }
        }
        if self.end() >= start {
            self.facts.sites[symbol.0].body_range = Some(TextRange::new(start, self.end()));
        }
    }

    fn documentation(&self, start: usize) -> Option<TextRange> {
        let declaration_line = self.line(start);
        let indent = start - self.lines[declaration_line].content.start;
        let mut first = declaration_line;
        while let Some(previous) = first.checked_sub(1) {
            let line = self.lines[previous];
            if line.indent != indent
                || !self.source[line.code.start..line.code.end].starts_with('#')
            {
                break;
            }
            first = previous;
        }
        (first < declaration_line).then(|| {
            TextRange::new(
                self.lines[first].code.start,
                self.lines[declaration_line - 1].content.end,
            )
        })
    }

    fn open_scope(&mut self) -> ScopeFrame {
        let scope = ScopeId(self.facts.scopes.len());
        self.facts.scopes.push(IndexedScope {
            parent: self.scope,
            uncertain: false,
        });
        self.scope_bindings.push(None);
        self.scope = Some(scope);
        ScopeFrame {
            scope,
            environment_start: self.environment_stack.len(),
        }
    }

    fn close_scope(&mut self, frame: ScopeFrame, end: usize) {
        while self.environment_stack.len() > frame.environment_start {
            let Some(entry) = self.environment_stack.pop() else {
                break;
            };
            let name = self.facts.bindings[entry.binding].name;
            self.environment[name.0] = entry.previous;
        }
        self.close_scope_range(frame.scope, end);
        self.scope = self.facts.scopes[frame.scope.0].parent;
    }

    fn close_scope_range(&mut self, scope: ScopeId, end: usize) {
        let mut next = self.scope_bindings[scope.0];
        while let Some(binding) = next {
            self.facts.bindings[binding].scope.end =
                end.max(self.facts.bindings[binding].scope.start);
            next = self.binding_next[binding];
        }
        self.facts.scopes[scope.0].uncertain = self.uncertain;
    }

    fn open_binding(&mut self, name: NameId, binding: usize) {
        if name.0 >= self.environment.len() {
            self.environment.resize(name.0 + 1, IndexSlot::EMPTY);
        }
        let previous = std::mem::replace(&mut self.environment[name.0], IndexSlot::some(binding));
        self.environment_stack
            .push(EnvironmentEntry { binding, previous });
    }

    fn bind(
        &mut self,
        reference: usize,
        annotation: Option<TextRange>,
        full_range: TextRange,
        parameter: bool,
    ) {
        let (Some(owner), Some(scope_id)) = (self.owner, self.scope) else {
            return;
        };
        let occurrence = self.facts.references[reference];
        let index = self.facts.bindings.len();
        let name = occurrence.full_name;
        let declaration = occurrence.reference.token;
        let id = SymbolId(index);
        let anonymous = &self.source[occurrence.full_range.start..occurrence.full_range.end] == "_";
        self.facts.bindings.push(IndexedBinding {
            id,
            binding_id: BindingId(index),
            scope_id,
            name,
            name_range: occurrence.full_range,
            full_range,
            annotation,
            synthetic: occurrence.synthetic,
            anonymous,
            scope: TextRange::new(self.offset(), self.offset()),
            owner,
            declaration,
        });
        self.parameter_bindings.push(parameter);
        self.binding_next.push(self.scope_bindings[scope_id.0]);
        self.scope_bindings[scope_id.0] = Some(index);
        self.pattern_candidates[reference] = false;
        let reference = &mut self.facts.references[reference];
        reference.reference.kind = ReferenceKind::Declaration;
        reference.role = ReferenceRole::Binding;
        reference.reference.resolved = (!anonymous).then_some(id);
        reference.local_binding = (!anonymous).then_some(BindingId(index));
        reference.reference.name = name;
        reference.reference.range = occurrence.full_range;
        if !anonymous {
            self.open_binding(name, index);
        }
    }

    fn atom(&mut self) -> Option<usize> {
        let first = self.token()?;
        if self.tokens[first.0].kind != TokenKind::Identifier {
            return None;
        }
        let first_range = self.tokens[first.0].range;
        let segment_start = self.facts.segments.len();
        let last = loop {
            let token = self.bump()?;
            self.facts.segments.push(NameSegment {
                token,
                range: self.tokens[token.0].range,
            });
            if !self.at(Tag::Dot) {
                break token;
            }
            let dot = self.token()?;
            let Some(next) = self
                .significant
                .get(self.position + 1)
                .copied()
                .filter(|_| self.position + 1 < self.limit)
            else {
                break token;
            };
            if self.tokens[dot.0].range.start != self.tokens[token.0].range.end
                || self.tokens[next.index].range.start != self.tokens[dot.0].range.end
                || self.tokens[next.index].kind != TokenKind::Identifier
            {
                break token;
            }
            let _ = self.bump();
        };
        let range = TextRange::new(first_range.start, self.tokens[last.0].range.end);
        let terminal_name = self.names.for_token(last)?;
        let full_name = if first == last {
            terminal_name
        } else {
            self.names.intern_name(self.source, range)
        };
        let local_binding = self
            .environment
            .get(full_name.0)
            .copied()
            .and_then(IndexSlot::value);
        let name = if local_binding.is_some() {
            full_name
        } else {
            terminal_name
        };
        let qualifier = (first != last).then(|| {
            self.names.intern_name(
                self.source,
                TextRange::new(range.start, self.tokens[last.0 - 1].range.start),
            )
        });
        let index = self.facts.references.len();
        self.facts.references.push(IndexedReference {
            reference: Reference {
                name,
                qualifier,
                qualifier_token: (first != last).then_some(first),
                token: last,
                range: if local_binding.is_some() {
                    range
                } else {
                    self.tokens[last.0].range
                },
                enclosing: self.owner,
                kind: ReferenceKind::Read,
                resolved: local_binding.map(SymbolId),
            },
            full_name,
            full_range: range,
            segment_span: TextRange::new(segment_start, self.facts.segments.len()),
            role: if self.in_type {
                ReferenceRole::Type
            } else {
                ReferenceRole::Value
            },
            in_type: self.in_type,
            uncertain: self.uncertain,
            synthetic: false,
            local_binding: local_binding.map(BindingId),
        });
        self.pattern_candidates.push(true);
        Some(index)
    }

    fn type_expression(&mut self, precedence: u8) -> Option<Expression> {
        let previous = self.in_type;
        let start = self.pattern_candidates.len();
        self.in_type = true;
        let expression = self.expression(precedence);
        self.in_type = previous;
        self.pattern_candidates[start..].fill(false);
        expression
    }

    fn expression(&mut self, precedence: u8) -> Option<Expression> {
        // Deliberately bound recursive parser stack use. The token cursor recovers
        // to the next declaration and publishes uncertainty instead of panicking.
        if self.depth >= 512 {
            self.uncertain = true;
            self.position = self.limit;
            return None;
        }
        self.depth += 1;
        let expression = self.expression_inner(precedence);
        self.depth -= 1;
        expression
    }

    fn expression_inner(&mut self, precedence: u8) -> Option<Expression> {
        let start = self.offset();
        let references = self.facts.references.len();
        let (reference, pattern) = match self.base()? {
            Head::Name(reference) => (Some(reference), true),
            Head::Pattern => (None, true),
            Head::Other => (None, false),
        };
        let mut expression = Expression {
            range: TextRange::new(start, self.end()),
            reference,
            references: TextRange::new(references, self.facts.references.len()),
            write: None,
            pattern,
        };
        loop {
            let lookahead = self.tag();
            if matches!(lookahead, Tag::OpenParen | Tag::Bang) {
                let offload = lookahead == Tag::Bang;
                if (!offload && self.newline())
                    || (offload && self.source.as_bytes().get(self.offset() + 1) != Some(&b'('))
                {
                    break;
                }
                let _ = self.bump();
                if offload && !self.take(Tag::OpenParen) {
                    self.uncertain = true;
                    break;
                }
                self.application(&expression, offload);
                expression.reference = None;
                expression.pattern = false;
            } else if lookahead == Tag::OpenBracket && !self.newline() {
                let _ = self.bump();
                let _ = self.expression(0);
                self.eat(Tag::CloseBracket);
                if !self.newline() && self.take(Tag::Bind) {
                    expression.write = expression.reference;
                    let _ = self.expression(2);
                }
                expression.reference = None;
                expression.pattern = false;
            } else if precedence == 0 && lookahead == Tag::Lambda {
                let _ = self.bump();
                let frame = self.open_scope();
                if let Some(reference) = expression.reference {
                    self.bind_pattern_reference(reference);
                } else {
                    self.uncertain = true;
                }
                let column = self.column().saturating_sub(1);
                let _ = self.body(column);
                self.close_scope(frame, self.end());
                expression.reference = None;
                expression.pattern = false;
            } else if lookahead == Tag::Less && self.type_arguments(&mut expression, precedence) {
                expression.reference = None;
                expression.pattern = false;
            } else if let Some((priority, right)) = self.infix(lookahead) {
                if priority < precedence {
                    break;
                }
                let _ = self.bump();
                let constructor = lookahead == Tag::Pair;
                let right = self.expression(if right { priority } else { priority + 1 });
                expression.pattern &= constructor && right.is_some_and(|right| right.pattern);
                self.uncertain |= right.is_none();
                expression.reference = None;
            } else {
                break;
            }
            expression.range.end = self.end();
            expression.references.end = self.facts.references.len();
        }
        expression.range.end = self.end();
        expression.references.end = self.facts.references.len();
        Some(expression)
    }

    fn base(&mut self) -> Option<Head> {
        while self.take(Tag::Plus) {}
        let token = self.token()?;
        match self.tag() {
            Tag::Universe | Tag::Data | Tag::Quant => {
                let _ = self.bump();
                Some(Head::Other)
            }
            Tag::Kind => {
                let _ = self.bump();
                self.eat(Tag::OpenParen);
                let _ = self.expression(0);
                self.eat(Tag::CloseParen);
                Some(Head::Other)
            }
            Tag::Do => {
                self.do_block();
                Some(Head::Other)
            }
            Tag::Match
            | Tag::Case
            | Tag::Return
            | Tag::For
            | Tag::Exs
            | Tag::Where
            | Tag::Is
            | Tag::Import
            | Tag::Def
            | Tag::Law
            | Tag::Type => None,
            Tag::At => {
                self.quantified();
                Some(Head::Other)
            }
            Tag::And => {
                self.existential();
                Some(Head::Other)
            }
            Tag::Question => {
                let _ = self.bump();
                let _ = self.bump();
                Some(Head::Other)
            }
            Tag::Percent => {
                self.rewrite();
                Some(Head::Other)
            }
            Tag::Backslash => {
                self.lambda_match();
                Some(Head::Other)
            }
            Tag::OpenParen => Some(self.group()),
            Tag::OpenBracket => Some(if self.list() {
                Head::Pattern
            } else {
                Head::Other
            }),
            Tag::OpenBrace => {
                self.annotation();
                Some(Head::Other)
            }
            _ if self.tokens[token.0].kind == TokenKind::Identifier => {
                let reference = self.atom()?;
                if self.offset() == self.end() && self.take(Tag::OpenBrace) {
                    self.nonlocal_role(reference, ReferenceRole::Constructor);
                    let pattern = self.arguments(Tag::CloseBrace);
                    Some(if pattern { Head::Pattern } else { Head::Other })
                } else {
                    Some(Head::Name(reference))
                }
            }
            _ if self.tokens[token.0].kind == TokenKind::Number => {
                let natural = self.source.as_bytes()[self.tokens[token.0].range.end - 1] == b'n';
                let end = self.tokens[token.0].range.end;
                let _ = self.bump();
                if natural && self.at(Tag::Plus) && self.offset() == end {
                    let _ = self.bump();
                    let pattern = self
                        .expression(0)
                        .is_some_and(|expression| expression.pattern);
                    Some(if pattern { Head::Pattern } else { Head::Other })
                } else {
                    Some(Head::Pattern)
                }
            }
            _ if self.tokens[token.0].kind == TokenKind::StringLiteral => {
                let _ = self.bump();
                Some(Head::Pattern)
            }
            _ => None,
        }
    }

    fn infix(&self, tag: Tag) -> Option<(u8, bool)> {
        let priority = match tag {
            Tag::Arrow => (0, true),
            Tag::And | Tag::Or => (1, true),
            Tag::LogicalOr => (2, false),
            Tag::LogicalAnd => (3, false),
            Tag::Less | Tag::LessEqual | Tag::Greater | Tag::GreaterEqual => (4, false),
            Tag::Pair | Tag::Concat | Tag::Parallel => (5, true),
            Tag::BitOr => (6, false),
            Tag::BitXor => (7, false),
            Tag::BitAnd => (8, false),
            Tag::ShiftLeft | Tag::ShiftRight => (9, false),
            Tag::Plus | Tag::Minus => (10, false),
            Tag::Star | Tag::Slash | Tag::Percent => (11, false),
            _ => return None,
        };
        let start = self.offset();
        let end = self
            .token()
            .map_or(start, |token| self.tokens[token.0].range.end);
        if (matches!(tag, Tag::Greater | Tag::GreaterEqual | Tag::ShiftRight)
            && self.end() == start)
            || (matches!(tag, Tag::Plus | Tag::Minus)
                && self
                    .source
                    .as_bytes()
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_'))
        {
            return None;
        }
        Some(priority)
    }

    fn type_arguments(&mut self, expression: &mut Expression, precedence: u8) -> bool {
        if expression.reference.is_none() {
            return false;
        }
        // The compiler commits after parsing the first comparison-level operand.
        // A glued '<' starts a family application; spaced '<' can compare.
        if self.end() != self.offset() && precedence > 4 {
            return false;
        }
        let before = self.position;
        let reference = expression.reference;
        let _ = self.bump();
        let references = self.facts.references.len();
        let calls = self.facts.calls.len();
        let _ = self.expression(5);
        if self.at(Tag::Greater) || self.at(Tag::Comma) {
            if let Some(reference) = reference {
                self.nonlocal_role(reference, ReferenceRole::Type);
            }
            for occurrence in &mut self.facts.references[references..] {
                occurrence.in_type = true;
                if occurrence.role == ReferenceRole::Value {
                    occurrence.role = ReferenceRole::Type;
                }
            }
            for call in &mut self.facts.calls[calls..] {
                call.caller = None;
            }
            self.take(Tag::Comma);
            let previous = self.in_type;
            self.in_type = true;
            self.arguments(Tag::Greater);
            self.in_type = previous;
        } else {
            // The first operand was already consumed exactly once as a comparison.
            if self.position == before + 1 {
                self.uncertain = true;
            }
        }
        true
    }

    fn application(&mut self, callee: &Expression, offload: bool) {
        let open = TokenId(self.significant[self.position - 1].index);
        let slot = callee
            .reference
            .map(|reference| self.reserve_call(callee, reference, open, offload));
        let argument_mark = self.argument_work.len();
        let separator_mark = self.separator_work.len();
        while self.token().is_some() && !self.at(Tag::CloseParen) {
            let before = self.position;
            self.take(Tag::Tilde);
            if let Some(argument) = self.expression(0) {
                self.argument_work.push(argument.range);
            }
            if self.at(Tag::Comma) {
                self.separator_work.push(self.offset());
                let _ = self.bump();
            }
            if self.position == before {
                self.uncertain = true;
                let _ = self.bump();
            }
        }
        let argument_end = self.offset();
        self.eat(Tag::CloseParen);
        let Some(slot) = slot else {
            self.argument_work.truncate(argument_mark);
            self.separator_work.truncate(separator_mark);
            return;
        };
        let stored_arguments = self.facts.arguments.len();
        self.facts
            .arguments
            .extend_from_slice(&self.argument_work[argument_mark..]);
        self.argument_work.truncate(argument_mark);
        let stored_separators = self.facts.separators.len();
        self.facts
            .separators
            .extend_from_slice(&self.separator_work[separator_mark..]);
        self.separator_work.truncate(separator_mark);
        let call_end = self.end();
        let call = &mut self.facts.calls[slot];
        call.call_range.end = call_end;
        call.argument_range.end = argument_end;
        call.argument_indices = TextRange::new(stored_arguments, self.facts.arguments.len());
        call.separator_indices = TextRange::new(stored_separators, self.facts.separators.len());
    }

    fn reserve_call(
        &mut self,
        callee: &Expression,
        reference: usize,
        open: TokenId,
        offload: bool,
    ) -> usize {
        self.facts.references[reference].reference.kind = ReferenceKind::Call;
        self.facts.references[reference].role = if offload {
            ReferenceRole::Offload
        } else {
            ReferenceRole::Call
        };
        let occurrence = self.facts.references[reference];
        let end = self.tokens[open.0].range.end;
        let slot = self.facts.calls.len();
        self.facts.calls.push(CallSite {
            caller: (!self.in_type).then_some(self.owner).flatten(),
            // SyntaxIndex resolves callees from the finalized token identities.
            callee: None,
            name: occurrence.reference.name,
            qualifier: occurrence.reference.qualifier,
            qualifier_token: occurrence.reference.qualifier_token,
            callee_token: occurrence.reference.token,
            callee_range: occurrence.reference.range,
            call_range: TextRange::new(callee.range.start, end),
            argument_range: TextRange::new(end, end),
            argument_indices: TextRange::default(),
            separator_indices: TextRange::default(),
            open,
        });
        slot
    }

    fn arguments(&mut self, close: Tag) -> bool {
        let mut pattern = true;
        while self.token().is_some() {
            if self.take(close) {
                return pattern;
            }
            let before = self.position;
            pattern &= self
                .expression(0)
                .is_some_and(|expression| expression.pattern);
            self.take(Tag::Comma);
            if self.position == before {
                self.uncertain = true;
                let _ = self.bump();
            }
        }
        self.uncertain = true;
        false
    }

    fn group(&mut self) -> Head {
        let _ = self.bump();
        let column = self.column().saturating_sub(1);
        let first = self.body(column);
        let tuple = self.at(Tag::Comma);
        let mut pattern = first.is_some_and(|expression| expression.pattern);
        while self.take(Tag::Comma) {
            pattern &= self
                .body(column)
                .is_some_and(|expression| expression.pattern);
        }
        if self.take(Tag::Colon) {
            let _ = self.type_expression(0);
        }
        let complete = self.eat(Tag::CloseParen);
        if !pattern || !complete {
            Head::Other
        } else if tuple {
            Head::Pattern
        } else {
            first
                .and_then(|expression| expression.reference)
                .map_or(Head::Pattern, Head::Name)
        }
    }

    fn list(&mut self) -> bool {
        let _ = self.bump();
        if self.take(Tag::CloseBracket) {
            return true;
        }
        let first = self.expression(0);
        if self.take(Tag::Colon) {
            let _ = self.type_expression(12);
            if !self.take(Tag::Star) {
                self.eat(Tag::Caret);
            }
            let _ = self.expression(0);
            self.eat(Tag::CloseBracket);
            false
        } else {
            self.take(Tag::Comma);
            let remaining = self.arguments(Tag::CloseBracket);
            first.is_some_and(|expression| expression.pattern) && remaining
        }
    }

    fn annotation(&mut self) {
        let _ = self.bump();
        if self.take(Tag::EqualEqual) {
            self.eat(Tag::CloseBrace);
            return;
        }
        let _ = self.expression(0);
        if self.take(Tag::EqualEqual) || self.take(Tag::NotEqual) {
            let _ = self.expression(0);
        }
        self.eat(Tag::Colon);
        let _ = self.type_expression(0);
        self.eat(Tag::CloseBrace);
    }

    fn quantified(&mut self) {
        let _ = self.bump();
        if !self.take(Tag::Minus) {
            self.take(Tag::Plus);
        }
        let Some(reference) = self.atom() else {
            self.uncertain = true;
            return;
        };
        let start = self.facts.references[reference].full_range.start;
        self.eat(Tag::Colon);
        let annotation = self.type_expression(1).map(|expression| expression.range);
        let full_range = TextRange::new(start, self.end());
        self.eat(Tag::Arrow);
        let frame = self.open_scope();
        self.bind(reference, annotation, full_range, false);
        let _ = self.type_expression(0);
        self.close_scope(frame, self.end());
    }

    fn existential(&mut self) {
        let _ = self.bump();
        if self
            .token()
            .is_some_and(|token| self.tokens[token.0].kind == TokenKind::Number)
        {
            let _ = self.bump();
            return;
        }
        let Some(reference) = self.atom() else {
            self.uncertain = true;
            return;
        };
        let start = self.facts.references[reference].full_range.start;
        self.eat(Tag::Colon);
        let annotation = self.type_expression(1).map(|expression| expression.range);
        let full_range = TextRange::new(start, self.end());
        self.eat(Tag::Arrow);
        let frame = self.open_scope();
        self.bind(reference, annotation, full_range, false);
        let _ = self.type_expression(0);
        self.close_scope(frame, self.end());
    }

    fn rewrite(&mut self) {
        let Some(anchor) = self.bump() else { return };
        let equation = self.expression(0);
        let binder = if self.take(Tag::At) {
            let _ = self.expression(0);
            equation.and_then(|expression| expression.reference)
        } else {
            None
        };
        self.eat(Tag::Colon);
        let frame = self.open_scope();
        self.bind_rewrite_proof(anchor);
        if let Some(reference) = binder {
            self.bind_pattern_reference(reference);
        }
        let _ = self.type_expression(0);
        self.close_scope(frame, self.end());
        self.take(Tag::Semicolon);
        let column = self.column().saturating_sub(1);
        let _ = self.body(column);
    }

    fn bind_rewrite_proof(&mut self, anchor: TokenId) {
        let (Some(owner), Some(scope_id), Some(name)) =
            (self.owner, self.scope, self.names.find(self.source, "_"))
        else {
            return;
        };
        // With no written '_' in the file there can be no use of this implicit
        // binder, so the scanner's name table avoids retaining unused facts.
        let index = self.facts.bindings.len();
        let range = self.tokens[anchor.0].range;
        let offset = self.offset();
        self.facts.bindings.push(IndexedBinding {
            id: SymbolId(index),
            binding_id: BindingId(index),
            scope_id,
            name,
            name_range: range,
            full_range: range,
            annotation: None,
            synthetic: true,
            anonymous: false,
            scope: TextRange::new(offset, offset),
            owner,
            declaration: anchor,
        });
        self.parameter_bindings.push(false);
        self.binding_next.push(self.scope_bindings[scope_id.0]);
        self.scope_bindings[scope_id.0] = Some(index);
        self.open_binding(name, index);
    }

    fn lambda_match(&mut self) {
        let _ = self.bump();
        self.eat(Tag::OpenBrace);
        while self.token().is_some() && !self.take(Tag::CloseBrace) {
            let before = self.position;
            let term = self.expression(0);
            if self.take(Tag::Colon) {
                if let Some(reference) = term.and_then(|expression| expression.reference) {
                    self.nonlocal_role(reference, ReferenceRole::Constructor);
                }
                let _ = self.expression(0);
            }
            self.take(Tag::Semicolon);
            if self.position == before {
                self.uncertain = true;
                let _ = self.bump();
            }
        }
    }

    fn nonlocal_role(&mut self, reference: usize, role: ReferenceRole) {
        self.pattern_candidates[reference] = false;
        let token = self.facts.references[reference].reference.token;
        let terminal_name = self.names.for_token(token);
        let reference = &mut self.facts.references[reference];
        reference.role = role;
        reference.reference.resolved = None;
        reference.local_binding = None;
        reference.reference.range = self.tokens[token.0].range;
        if let Some(name) = terminal_name {
            reference.reference.name = name;
        }
    }

    fn body(&mut self, column: usize) -> Option<Expression> {
        let environment_start = self.environment_stack.len();
        let scope_start = self.scope;
        if self.depth >= 512 {
            self.uncertain = true;
            self.position = self.limit;
            return None;
        }
        self.depth += 1;
        let mut result = None;
        let mut local = false;
        while self.token().is_some() {
            if self.take(Tag::Match) {
                self.match_body(column);
                break;
            }
            let before = self.position;
            let start = self.offset();
            let erased = self.take(Tag::Minus);
            let Some(mut first) = self.expression(0) else {
                self.uncertain = true;
                break;
            };
            if erased {
                first.range.start = start;
                first.pattern &= first.reference.is_some();
            }
            let mut pattern_end = first.references.end;
            let mut pattern = first.pattern;
            let mut count = 1;
            while !self.newline() && (self.takeable_parallel_name() || self.at(Tag::Plus)) {
                let Some(next) = self.expression(0) else {
                    break;
                };
                pattern &= first.reference.is_some() && next.reference.is_some();
                pattern_end = next.references.end;
                count += 1;
            }
            let annotation = if count == 1 && self.at(Tag::Colon) {
                self.typed_annotation()
            } else {
                None
            };
            pattern &= annotation.is_none() || first.reference.is_some();
            if !self.take(Tag::Equal) {
                if self.implicit_write(first) {
                    local = true;
                    continue;
                }
                result = Some(first);
                break;
            }
            let mut complete = true;
            for _ in 0..count {
                if self.expression(0).is_none() {
                    self.uncertain = true;
                    complete = false;
                    break;
                }
            }
            if !complete {
                break;
            }
            pattern &= self.patterns_have_no_bare_constructors(TextRange::new(
                first.references.start,
                pattern_end,
            ));
            if !pattern {
                self.uncertain = true;
                for reference in &mut self.facts.references[first.references.start..pattern_end] {
                    reference.uncertain = true;
                }
                break;
            }
            self.bind_let_patterns(&first, pattern_end, count, annotation);
            local = true;
            if self.position <= before {
                self.uncertain = true;
                let _ = self.bump();
            }
        }
        self.close_body_scopes(scope_start, environment_start);
        self.depth -= 1;
        result.map(|mut expression| {
            if local {
                expression.pattern = false;
                expression.reference = None;
            }
            expression
        })
    }

    fn bind_let_patterns(
        &mut self,
        first: &Expression,
        pattern_end: usize,
        count: usize,
        annotation: Option<TextRange>,
    ) {
        self.take(Tag::Semicolon);
        self.open_scope();
        for reference in first.references.start..pattern_end {
            let occurrence = self.facts.references[reference];
            if self.pattern_candidates[reference]
                && occurrence.reference.kind == ReferenceKind::Read
            {
                let full = if count == 1 && first.reference.is_some() {
                    TextRange::new(
                        first.range.start,
                        annotation.map_or(first.range.end, |range| range.end),
                    )
                } else {
                    occurrence.full_range
                };
                self.bind(reference, annotation, full, false);
            }
        }
    }

    fn implicit_write(&mut self, expression: Expression) -> bool {
        let Some(reference) = expression.write else {
            return false;
        };
        if !(self.at(Tag::Semicolon)
            || self.token().is_some() && self.column() == self.column_at(expression.range.start))
        {
            return false;
        }
        self.take(Tag::Semicolon);
        self.open_scope();
        let mut declaration = self.facts.references[reference];
        declaration.synthetic = true;
        let index = self.facts.references.len();
        self.facts.references.push(declaration);
        self.pattern_candidates.push(false);
        self.bind(index, None, declaration.full_range, false);
        true
    }

    fn close_body_scopes(&mut self, parent: Option<ScopeId>, environment_start: usize) {
        let end = self.end();
        while self.scope != parent {
            let Some(scope) = self.scope else { break };
            self.close_scope_range(scope, end);
            self.scope = self.facts.scopes[scope.0].parent;
        }
        while self.environment_stack.len() > environment_start {
            let Some(entry) = self.environment_stack.pop() else {
                break;
            };
            let name = self.facts.bindings[entry.binding].name;
            self.environment[name.0] = entry.previous;
        }
    }

    fn typed_annotation(&mut self) -> Option<TextRange> {
        let position = self.position;
        let uncertain = self.uncertain;
        let checkpoint = (
            self.facts.references.len(),
            self.facts.segments.len(),
            self.facts.bindings.len(),
            self.facts.scopes.len(),
            self.facts.calls.len(),
            self.facts.arguments.len(),
            self.facts.separators.len(),
        );
        let _ = self.bump();
        let annotation = self.type_expression(0).map(|expression| expression.range);
        if self.at(Tag::Equal) {
            return annotation;
        }
        self.position = position;
        self.uncertain = uncertain;
        self.facts.references.truncate(checkpoint.0);
        self.pattern_candidates.truncate(checkpoint.0);
        self.facts.segments.truncate(checkpoint.1);
        self.facts.bindings.truncate(checkpoint.2);
        self.parameter_bindings.truncate(checkpoint.2);
        self.binding_next.truncate(checkpoint.2);
        self.facts.scopes.truncate(checkpoint.3);
        self.scope_bindings.truncate(checkpoint.3);
        self.facts.calls.truncate(checkpoint.4);
        self.facts.arguments.truncate(checkpoint.5);
        self.facts.separators.truncate(checkpoint.6);
        None
    }

    fn reserved(&self) -> bool {
        matches!(
            self.tag(),
            Tag::Match
                | Tag::Case
                | Tag::Do
                | Tag::Return
                | Tag::For
                | Tag::Exs
                | Tag::Where
                | Tag::Is
                | Tag::Import
                | Tag::Def
                | Tag::Law
                | Tag::Type
                | Tag::Universe
                | Tag::Data
                | Tag::Kind
                | Tag::Quant
        )
    }

    fn bind_pattern_reference(&mut self, reference: usize) {
        let occurrence = self.facts.references[reference];
        if occurrence.reference.qualifier.is_some() && occurrence.reference.resolved.is_none() {
            self.uncertain = true;
            self.facts.references[reference].uncertain = true;
            return;
        }
        self.bind(reference, None, occurrence.full_range, false);
    }

    fn patterns_have_no_bare_constructors(&self, references: TextRange) -> bool {
        (references.start..references.end).all(|index| {
            !self.pattern_candidates[index]
                || !self
                    .constructor_names
                    .contains(&self.facts.references[index].full_name)
        })
    }

    fn completion_pattern_end(&self, start: usize, column: usize) -> usize {
        let header_line = self.line(start);
        let mut previous_line = header_line;
        let mut depth = 0usize;
        for token in &self.significant[self.position..self.limit] {
            let range = self.tokens[token.index].range;
            let line = self.line(range.start);
            if line > previous_line {
                if depth == 0 {
                    return self.lines[previous_line].content.end;
                }
                if self.lines[line].indent < column
                    && !matches!(
                        token.tag,
                        Tag::CloseParen | Tag::CloseBrace | Tag::CloseBracket
                    )
                {
                    return self.lines[line].content.start;
                }
            }
            previous_line = line;
            match token.tag {
                Tag::Colon if depth == 0 => return range.start,
                Tag::OpenParen | Tag::OpenBrace | Tag::OpenBracket => depth += 1,
                Tag::CloseParen | Tag::CloseBrace | Tag::CloseBracket => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        if depth == 0 {
            self.lines[previous_line].content.end
        } else {
            self.significant
                .get(self.limit)
                .map_or(self.source.len(), |token| {
                    self.lines[self.line(self.tokens[token.index].range.start)]
                        .content
                        .start
                })
        }
    }

    fn match_body(&mut self, column: usize) {
        let mut scrutinees = 0;
        let mut expected_type = None;
        while self.token().is_some() && !self.at(Tag::Colon) {
            let Some(expression) = self.expression(0) else {
                self.uncertain = true;
                return;
            };
            if scrutinees == 0 {
                expected_type = expression
                    .reference
                    .and_then(|reference| self.facts.references[reference].local_binding)
                    .and_then(|binding| self.facts.bindings[binding.0].annotation);
            }
            scrutinees += 1;
            self.take(Tag::Comma);
        }
        if !self.eat(Tag::Colon) {
            return;
        }
        let case_column = self.column();
        while self.at(Tag::Case) && self.column() >= case_column && case_column > column {
            let arm_column = self.column();
            let _ = self.bump();
            let pattern_start = self.end();
            let completion_end = self.completion_pattern_end(pattern_start, arm_column);
            let references = self.facts.references.len();
            let mut count = 0;
            let mut pattern = true;
            while self.token().is_some() && !self.at(Tag::Colon) {
                let before = self.position;
                pattern &= self
                    .expression(0)
                    .is_some_and(|expression| expression.pattern);
                count += 1;
                self.take(Tag::Comma);
                if self.position == before {
                    self.uncertain = true;
                    break;
                }
            }
            self.facts
                .completion_patterns
                .push(super::completion::PatternSite {
                    range: TextRange::new(pattern_start, completion_end),
                    expected_type,
                });
            let complete = self.eat(Tag::Colon) && count == scrutinees && pattern;
            let end = self.facts.references.len();
            let complete = complete
                && self.patterns_have_no_bare_constructors(TextRange::new(references, end));
            let frame = self.open_scope();
            if complete {
                for reference in references..end {
                    if self.pattern_candidates[reference] {
                        self.bind_pattern_reference(reference);
                    }
                }
            } else {
                self.uncertain = true;
                for reference in &mut self.facts.references[references..end] {
                    reference.uncertain = true;
                }
            }
            let _ = self.body(arm_column);
            self.close_scope(frame, self.end());
        }
    }

    fn do_block(&mut self) {
        let _ = self.bump();
        if let Some(reference) = self.atom() {
            let reference = self.facts.references.remove(reference);
            let _ = self.pattern_candidates.pop();
            self.facts.segments.truncate(reference.segment_span.start);
        }
        if !self.take(Tag::Pair) {
            self.eat(Tag::Less);
            let previous = self.in_type;
            self.in_type = true;
            self.arguments(Tag::Greater);
            self.in_type = previous;
        }
        self.eat(Tag::Colon);
        let column = self.column();
        let environment_start = self.environment_stack.len();
        let parent = self.scope;
        while self.token().is_some() {
            if self.take(Tag::Return) {
                self.uncertain |= self.expression(0).is_none();
                break;
            }
            let before = self.position;
            let term = self.expression(0);
            let annotation = if term.and_then(|expression| expression.reference).is_some()
                && self.take(Tag::Colon)
            {
                self.type_expression(1).map(|expression| expression.range)
            } else {
                None
            };
            let assignment = annotation.is_some() && self.take(Tag::Equal);
            let bind = !assignment && self.take(Tag::Bind);
            if assignment || bind {
                if self.expression(0).is_none() {
                    self.uncertain = true;
                    break;
                }
                self.take(Tag::Semicolon);
                self.open_scope();
                if annotation.is_some()
                    && let Some(expression) = term
                    && let Some(reference) = expression.reference
                {
                    self.bind(
                        reference,
                        annotation,
                        TextRange::new(
                            expression.range.start,
                            annotation.map_or(expression.range.end, |range| range.end),
                        ),
                        false,
                    );
                }
            } else if self.take(Tag::Semicolon)
                || self.token().is_some() && self.newline() && self.column() == column
            {
                // A do step has no source-written binder.
            } else {
                break;
            }
            if self.position <= before {
                self.uncertain = true;
                let _ = self.bump();
            }
        }
        self.close_body_scopes(parent, environment_start);
    }

    fn takeable_parallel_name(&self) -> bool {
        self.token()
            .is_some_and(|token| self.tokens[token.0].kind == TokenKind::Identifier)
            && !self.reserved()
    }

    fn finish(mut self) -> FileFacts {
        let symbol_count = self.facts.symbols.len();
        if self.bindings_are_ordered() {
            self.finish_ordered_bindings(symbol_count);
            self.finalize_resolution_ids(symbol_count, None);
            self.discard_anonymous_occurrences();
            return self.facts;
        }
        let mut parameters = vec![0usize; symbol_count];
        let mut locals = vec![0usize; symbol_count];
        for (binding, parameter) in self.facts.bindings.iter().zip(&self.parameter_bindings) {
            if *parameter {
                parameters[binding.owner.0] += 1;
            } else {
                locals[binding.owner.0] += 1;
            }
        }
        let mut next_parameters = Vec::with_capacity(symbol_count);
        let mut next_locals = Vec::with_capacity(symbol_count);
        let mut total = 0;
        for (symbol, (parameters, locals)) in self
            .facts
            .symbols
            .iter_mut()
            .zip(parameters.into_iter().zip(locals))
        {
            symbol.parameter_span = TextRange::new(total, total + parameters);
            next_parameters.push(total);
            next_locals.push(total + parameters);
            total += parameters + locals;
        }
        let mut permutation = vec![0; self.facts.bindings.len()];
        let mut order = vec![0; self.facts.bindings.len()];
        for (index, (binding, parameter)) in self
            .facts
            .bindings
            .iter()
            .zip(&self.parameter_bindings)
            .enumerate()
        {
            let next = if *parameter {
                &mut next_parameters[binding.owner.0]
            } else {
                &mut next_locals[binding.owner.0]
            };
            permutation[index] = *next;
            order[*next] = index;
            *next += 1;
        }
        let bindings: Vec<_> = order
            .into_iter()
            .enumerate()
            .map(|(index, old)| {
                let mut binding = self.facts.bindings[old];
                binding.binding_id = BindingId(index);
                binding.id = SymbolId(symbol_count + index);
                binding
            })
            .collect();
        self.facts.bindings = bindings;
        self.finalize_resolution_ids(symbol_count, Some(&permutation));
        self.discard_anonymous_occurrences();
        self.facts
    }

    fn bindings_are_ordered(&self) -> bool {
        let mut owner = None;
        let mut local_seen = false;
        for (binding, parameter) in self.facts.bindings.iter().zip(&self.parameter_bindings) {
            if owner != Some(binding.owner) {
                if owner.is_some_and(|owner: SymbolId| owner.0 > binding.owner.0) {
                    return false;
                }
                owner = Some(binding.owner);
                local_seen = false;
            }
            if *parameter && local_seen {
                return false;
            }
            local_seen |= !parameter;
        }
        true
    }

    fn finish_ordered_bindings(&mut self, symbol_count: usize) {
        let mut position = 0;
        for symbol in &mut self.facts.symbols {
            let start = position;
            while position < self.facts.bindings.len()
                && self.facts.bindings[position].owner == symbol.id
                && self.parameter_bindings[position]
            {
                position += 1;
            }
            symbol.parameter_span = TextRange::new(start, position);
            while position < self.facts.bindings.len()
                && self.facts.bindings[position].owner == symbol.id
            {
                position += 1;
            }
        }
        for binding in &mut self.facts.bindings {
            binding.id = SymbolId(symbol_count + binding.binding_id.0);
        }
    }

    fn finalize_resolution_ids(&mut self, symbol_count: usize, permutation: Option<&[usize]>) {
        let remap = |index| permutation.map_or(index, |permutation| permutation[index]);
        for reference in &mut self.facts.references {
            if let Some(binding) = reference.reference.resolved {
                reference.reference.resolved = Some(SymbolId(symbol_count + remap(binding.0)));
            }
            if let Some(permutation) = permutation {
                reference.local_binding = reference
                    .local_binding
                    .map(|binding| BindingId(permutation[binding.0]));
            }
        }
        for (reference, symbol) in self.declaration_references.drain(..) {
            self.facts.references[reference].reference.resolved = Some(symbol);
        }
    }

    fn discard_anonymous_occurrences(&mut self) {
        let source = self.source;
        let names = &self.names;
        self.facts.references.retain(|reference| {
            let range = names.range(reference.full_name);
            !(reference.reference.kind == ReferenceKind::Declaration
                && reference.reference.resolved.is_none()
                && &source[range.start..range.end] == "_")
        });
    }
}
