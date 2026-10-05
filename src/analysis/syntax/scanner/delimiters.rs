use super::{
    DelimiterPair, DiagnosticKind, Scanner, SyntaxDiagnostic, TextRange, TokenFlags, TokenId,
    TokenKind,
};

impl Scanner<'_> {
    pub(super) fn delimiter(&mut self) {
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
}
