use super::{
    DelimiterPair, DiagnosticKind, IndexSlot, NameId, NameTable, ScanOutput, SyntaxDiagnostic,
    TextRange, Token, TokenFlags, TokenId, TokenKind,
};

pub(super) struct Scanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    offset: usize,
    tokens: Vec<Token>,
    semantic_token_count: usize,
    names: NameTable,
    diagnostics: Vec<SyntaxDiagnostic>,
    delimiters: Vec<(char, usize)>,
    delimiter_pairs: Vec<DelimiterPair>,
    delimiter_context: Vec<IndexSlot>,
    line_starts: Vec<usize>,
    line_ascii: Vec<u64>,
}

impl<'a> Scanner<'a> {
    pub(super) fn new(source: &'a str, collect_line_starts: bool) -> Self {
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
            semantic_token_count: 0,
            names: NameTable::new(),
            diagnostics: Vec::new(),
            delimiters: Vec::new(),
            delimiter_pairs: Vec::new(),
            delimiter_context: Vec::new(),
            line_starts,
            line_ascii,
        }
    }

    pub(super) fn scan(mut self) -> ScanOutput {
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
            } else if b"+-*/%=<>!&|^~".contains(&byte)
                || [".|.", ".^.", ".&."]
                    .iter()
                    .any(|operator| self.source[self.offset..].starts_with(*operator))
            {
                self.operator();
            } else {
                if !byte.is_ascii() {
                    self.mark_non_ascii();
                }
                self.punctuation();
            }
        }
        for &(delimiter, pair) in &self.delimiters {
            let range = self.tokens[self.delimiter_pairs[pair].open.0].range;
            self.diagnostics.push(SyntaxDiagnostic {
                range,
                kind: DiagnosticKind::UnclosedDelimiter(delimiter),
            });
        }
        // Opening slots already have source order; incomplete opens are not matches.
        self.delimiter_pairs.retain(|pair| pair.open != pair.close);
        ScanOutput {
            tokens: self.tokens,
            semantic_token_count: self.semantic_token_count,
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
        while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
            self.offset += 1;
        }
        if self.bytes.get(self.offset) == Some(&b'n') {
            self.offset += 1;
        } else if self.bytes.get(self.offset) == Some(&b'.')
            && self
                .bytes
                .get(self.offset + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            self.offset += 1;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if matches!(self.bytes.get(self.offset), Some(b'e' | b'E')) {
                self.offset += 1;
                if matches!(self.bytes.get(self.offset), Some(b'+' | b'-')) {
                    self.offset += 1;
                }
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                }
            }
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
            let pair = self.delimiter_pairs.len();
            self.delimiter_pairs.push(DelimiterPair {
                open: id,
                close: id,
            });
            self.delimiters.push((delimiter, pair));
            self.delimiter_context[id.0] = IndexSlot::some(id.0);
            return;
        }
        let expected = match delimiter {
            ')' => '(',
            ']' => '[',
            _ => '{',
        };
        match self.delimiters.last().copied() {
            Some((open, pair)) if open == expected => {
                self.delimiters.truncate(self.delimiters.len() - 1);
                self.delimiter_pairs[pair].close = id;
                self.delimiter_context[id.0] = IndexSlot::from_option(
                    self.delimiters
                        .last()
                        .map(|(_, pair)| self.delimiter_pairs[*pair].open.0),
                );
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
        const OPERATORS: &[&str] = &[
            "<&>", ".|.", ".^.", ".&.", "->", "=>", "<-", "==", "!=", "<=", ">=", "&&", "||", "<<",
            ">>", "<>", "++",
        ];
        let start = self.offset;
        let glued_close =
            self.bytes[start] == b'>' && start > 0 && !self.bytes[start - 1].is_ascii_whitespace();
        let width = if glued_close {
            1
        } else {
            OPERATORS
                .iter()
                .find(|operator| self.source[start..].starts_with(**operator))
                .map_or(1, |operator| operator.len())
        };
        self.offset += width;
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
        let token_type = match kind {
            TokenKind::Identifier => name.map(|name| self.names.facts[name.0].semantic_type),
            TokenKind::Comment => Some(17),
            TokenKind::StringLiteral => Some(18),
            TokenKind::Number => Some(19),
            TokenKind::Operator => Some(20),
            TokenKind::Punctuation | TokenKind::Delimiter => None,
        };
        self.semantic_token_count += usize::from(!matches!(
            kind,
            TokenKind::Punctuation | TokenKind::Delimiter
        ));
        self.tokens.push(Token {
            range: TextRange::new(start, end),
            kind,
            flags: flags.with_semantic_type(token_type),
        });
        self.delimiter_context.push(IndexSlot::from_option(
            self.delimiters
                .last()
                .map(|(_, pair)| self.delimiter_pairs[*pair].open.0),
        ));
        if name.is_none() {
            self.names.append_non_name();
        }
    }
}
