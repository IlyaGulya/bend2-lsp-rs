mod layout;

struct Token<'a> {
    text: &'a str,
    kind: u8,
    gap: bool,
    col: usize,
    width: usize,
}

struct Line<'a> {
    indent: &'a str,
    code: &'a str,
    comment: &'a str,
    tokens: Vec<Token<'a>>,
}

fn split_lines(source: &str) -> Option<Vec<Line<'_>>> {
    let mut raw: Vec<_> = source.split('\n').collect();
    if source.ends_with('\n') {
        raw.pop();
    }
    raw.into_iter()
        .map(|raw_line| {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let indent_len = line
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let (code, comment) = split_comment(&line[indent_len..])?;
            let code = code.trim();
            Some(Line {
                indent: &line[..indent_len],
                code,
                comment,
                tokens: lex(code),
            })
        })
        .collect()
}

pub(super) fn format_bend(source: &str, tab_size: usize, spaces: bool) -> String {
    let Some(lines) = split_lines(source) else {
        return source.into();
    };
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let final_eol = source.ends_with('\n');
    let mut layout = layout::Layout::new(&lines);
    let mut result = String::with_capacity(source.len());
    for (row, line) in lines.iter().enumerate() {
        layout.write_line(row, line, tab_size.max(1), spaces, &mut result);
        if row + 1 < lines.len() || final_eol {
            result.push_str(eol);
        }
    }
    let Some(formatted_lines) = split_lines(&result) else {
        return source.into();
    };
    if layout::fingerprint(&lines) != layout::fingerprint(&formatted_lines) {
        return source.into();
    }
    result
}

#[cfg(test)]
fn fingerprint(source: &str) -> Option<layout::Fingerprint<'_>> {
    split_lines(source).map(|lines| layout::fingerprint(&lines))
}

pub(super) fn opens_indented_body(line: &str) -> Option<bool> {
    let (code, _) = split_comment(line)?;
    if code.trim_end().ends_with(':') {
        return Some(true);
    }
    let mut words = code.split_whitespace();
    let mut keyword = words.next();
    if keyword == Some("@unsafe") {
        keyword = words.next();
    }
    Some(
        matches!(keyword, Some("def" | "law" | "type"))
            && words
                .next()
                .and_then(|name| name.as_bytes().first())
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_'),
    )
}

fn split_comment(source: &str) -> Option<(&str, &str)> {
    let mut chars = source.char_indices().peekable();
    while let Some((offset, character)) = chars.next() {
        if matches!(character, '"' | '\'') {
            // In ''', the first apostrophe after the opener is the character.
            if character == '\'' && chars.peek().is_some_and(|(_, next)| *next == '\'') {
                chars.next();
            }
            let mut escaped = false;
            let mut closed = false;
            for (_, next) in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == character {
                    closed = true;
                    break;
                }
            }
            if !closed {
                return None;
            }
        } else if character == '#' {
            return Some((&source[..offset], &source[offset..]));
        }
    }
    Some((source, ""))
}
fn lex_import_path<'a>(
    code: &'a str,
    chars: &[(usize, char)],
    mut index: usize,
    mut column: usize,
) -> Option<(Token<'a>, usize)> {
    while index < chars.len() && chars[index].1.is_whitespace() {
        column += chars[index].1.len_utf16();
        index += 1;
    }
    let start = chars.get(index)?.0;
    while index < chars.len() && !chars[index].1.is_whitespace() {
        index += 1;
    }
    let end = chars.get(index).map_or(code.len(), |(offset, _)| *offset);
    let text = &code[start..end];
    Some((
        Token {
            text,
            kind: 0,
            gap: true,
            col: column,
            width: text.encode_utf16().count(),
        },
        index,
    ))
}

fn lex_number(chars: &[(usize, char)], mut index: usize) -> usize {
    index += 1;
    while index < chars.len() && chars[index].1.is_ascii_digit() {
        index += 1;
    }
    if index + 1 < chars.len() && chars[index].1 == '.' && chars[index + 1].1.is_ascii_digit() {
        index += 1;
        while index < chars.len() && chars[index].1.is_ascii_digit() {
            index += 1;
        }
    }
    if index < chars.len() && matches!(chars[index].1, 'e' | 'E') {
        let next = index + 1;
        let exponent = next < chars.len() && chars[next].1.is_ascii_digit()
            || next + 1 < chars.len()
                && matches!(chars[next].1, '+' | '-')
                && chars[next + 1].1.is_ascii_digit();
        if exponent {
            index += 1;
            if index < chars.len() && matches!(chars[index].1, '+' | '-') {
                index += 1;
            }
            while index < chars.len() && chars[index].1.is_ascii_digit() {
                index += 1;
            }
        }
    }
    if index < chars.len() && chars[index].1 == 'n' {
        index += 1;
    }
    index
}

fn lex_literal(chars: &[(usize, char)], mut index: usize) -> Option<usize> {
    let quote = chars[index].1;
    index += 1;
    if quote == '\'' && chars.get(index).is_some_and(|(_, next)| *next == '\'') {
        index += 1;
    }
    let mut escaped = false;
    while index < chars.len() {
        let next = chars[index].1;
        index += 1;
        if escaped {
            escaped = false;
        } else if next == '\\' {
            escaped = true;
        } else if next == quote {
            return Some(index);
        }
    }
    None
}

fn lex(code: &str) -> Vec<Token<'_>> {
    const MULTI: [&str; 17] = [
        "<&>", ".|.", ".^.", ".&.", "==", "!=", "->", "<-", "=>", "&&", "||", "++", "<>", "<=",
        ">=", "<<", ">>",
    ];
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let mut i = 0;
    let mut had_gap = false;
    let mut tokens = Vec::new();
    let mut column = 0;
    let mut cursor = 0;
    while i < chars.len() {
        column += code[cursor..chars[i].0].encode_utf16().count();
        cursor = chars[i].0;
        if tokens
            .last()
            .is_some_and(|token: &Token<'_>| token.text == "import")
        {
            let Some((token, next_index)) = lex_import_path(code, &chars, i, column) else {
                break;
            };
            column = token.col + token.width;
            cursor = chars
                .get(next_index)
                .map_or(code.len(), |(offset, _)| *offset);
            tokens.push(token);
            i = next_index;
            had_gap = false;
            continue;
        }
        let c = chars[i].1;
        if c.is_whitespace() {
            had_gap = true;
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let mut kind = 3;
        if c == '"' || c == '\'' {
            kind = 2;
            let Some(next) = lex_literal(&chars, i) else {
                return Vec::new();
            };
            i = next;
        } else if c.is_ascii_alphabetic() || c == '_' {
            kind = 0;
            i += 1;
            while i < chars.len()
                && (chars[i].1.is_ascii_alphanumeric() || "_.".contains(chars[i].1))
            {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            kind = 1;
            i = lex_number(&chars, i);
        } else if c == '?'
            && chars
                .get(i + 1)
                .is_some_and(|(_, ch)| ch.is_ascii_alphabetic() || *ch == '_')
        {
            kind = 0;
            i += 1;
            while i < chars.len() && (chars[i].1.is_ascii_alphanumeric() || chars[i].1 == '_') {
                i += 1;
            }
        } else {
            let rest = &code[start..];
            let multi = MULTI.iter().find(|operator| {
                rest.starts_with(**operator)
                    && (**operator != ">="
                        || tokens.is_empty()
                        || (had_gap && tokens.last().is_none_or(|token| token.text != ",")))
            });
            i += multi.map_or(1, |operator| operator.chars().count());
        }
        let end = chars.get(i).map_or(code.len(), |(offset, _)| *offset);
        let text = &code[start..end];
        let width = text.encode_utf16().count();
        tokens.push(Token {
            text,
            kind,
            gap: had_gap,
            col: column,
            width,
        });
        column += width;
        cursor = end;
        had_gap = false;
    }
    tokens
}
const BINARY: &[&str] = &[
    "=", "==", "!=", "->", "<-", "=>", "+", "-", "*", "/", "%", "&&", "||", "++", "<>", "<&>",
    "<=", ">=", "<<", ">>", ".|.", ".^.", ".&.", "&", "|",
];
fn unary(tokens: &[Token<'_>], index: usize) -> bool {
    let token = tokens[index].text;
    if !["+", "-", "~", "?", "@", "&", "%"].contains(&token) {
        return false;
    }
    let previous = index.checked_sub(1).map(|i| tokens[i].text);
    let Some(next) = tokens.get(index + 1) else {
        return false;
    };
    let at_prefix = previous.is_none_or(|p| {
        ["(", "{", "[", "<", ",", ":", "=", "for", "case", "~"].contains(&p) || BINARY.contains(&p)
    });
    match token {
        "~" | "%" => at_prefix,
        "+" | "-" => next.kind == 0 && (!next.gap || (index > 0 && at_prefix)),
        _ => (next.kind == 0 || next.kind == 1) && at_prefix,
    }
}
fn keep_angle_gap(left: &Token<'_>, right: &Token<'_>) -> Option<bool> {
    let angles = ["<", ">", "<<", ">>"];
    (angles.contains(&left.text) || angles.contains(&right.text)).then_some(right.gap)
}
fn needs_space(t: &[Token<'_>], i: usize) -> bool {
    let left = &t[i - 1];
    let right = &t[i];
    let a = left.text;
    let b = right.text;
    if [")", "]", "}", ",", ";"].contains(&b) {
        return false;
    }
    if b == ":" {
        return false;
    }
    if ["(", "[", "{"].contains(&a) {
        return false;
    }
    if a == "," {
        return true;
    }
    if b == "!" && (left.kind == 0 || [")", "]", "}"].contains(&a)) {
        return false;
    }
    if a == "!" && b == "(" {
        return false;
    }
    if b == "(" || b == "[" {
        let suffix = (left.kind == 0
            && ![
                "return", "match", "case", "do", "for", "exs", "where", "is", "import", "def",
                "type", "law",
            ]
            .contains(&a))
            || left.kind == 1
            || left.kind == 2
            || [")", "]", "}", ">", ">>"].contains(&a);
        return !suffix;
    }
    if b == "{"
        && ((left.kind == 0 && !["return", "case"].contains(&a)) || [">", ">>", "}"].contains(&a))
    {
        return left.kind == 0 && right.gap;
    }
    if a == "\\" && b == "{" {
        return false;
    }
    if a == "." || b == "." {
        return false;
    }
    if left.kind == 1 && left.text.ends_with('n') && ["+", "++"].contains(&b) {
        return right.gap;
    }
    if ["+", "++"].contains(&a)
        && t.get(i.wrapping_sub(2))
            .is_some_and(|token| token.kind == 1 && token.text.ends_with('n'))
    {
        return right.gap;
    }
    if unary(t, i - 1) {
        return false;
    }
    if unary(t, i) {
        return !["(", "[", "{", "<"].contains(&a);
    }
    if let Some(gap) = keep_angle_gap(left, right) {
        return gap;
    }
    if BINARY.contains(&a) || BINARY.contains(&b) || a == ":" {
        return true;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, format_bend};

    #[test]
    fn formatting_preserves_comments_strings_crlf_and_indent_style() {
        let source = "def main:\r\n\tvalue( \"a # b\" )  # note\r\n";
        let spaces = format_bend(source, 2, true);
        assert_eq!(spaces, "def main:\r\n  value(\"a # b\")  # note\r\n");
        assert_eq!(fingerprint(source), fingerprint(&spaces));

        let tabs = format_bend(source, 2, false);
        assert_eq!(tabs, "def main:\r\n\tvalue(\"a # b\")  # note\r\n");
        assert_eq!(fingerprint(source), fingerprint(&tabs));
    }

    #[test]
    fn formatting_preserves_unknown_punctuation_token_sequence() {
        let unknown = "def foo:\n  @@@ ???\n  1\n";
        let formatted = format_bend(unknown, 2, true);
        assert_eq!(formatted, "def foo:\n  @ @ @ ? ? ?\n  1\n");
        assert_eq!(fingerprint(unknown), fingerprint(&formatted));
    }

    #[test]
    fn formatting_preserves_missing_final_newline_and_incomplete_input() {
        let source = "def main:\n    1";
        let formatted = format_bend(source, 2, true);
        assert_eq!(formatted, "def main:\n  1");
        assert!(!formatted.ends_with('\n'));
        assert_eq!(fingerprint(source), fingerprint(&formatted));

        let incomplete = "def broken:\n  \"unterminated";
        assert_eq!(format_bend(incomplete, 2, true), incomplete);
    }

    #[test]
    fn formatting_preserves_inline_body_continuation_columns() {
        let source = concat!(
            "import Base\n",
            "def main() -> IO(Unit):\n",
            "    do IO<Unit>: IO.print(\"a\")\n",
            "                 IO.print(\"b\")\n",
        );
        let formatted = format_bend(source, 2, true);
        assert_eq!(
            formatted,
            concat!(
                "import Base\n",
                "def main() -> IO(Unit):\n",
                "  do IO<Unit>: IO.print(\"a\")\n",
                "               IO.print(\"b\")\n",
            ),
        );
        assert_eq!(format_bend(&formatted, 2, true), formatted);
    }

    #[test]
    fn formatting_keeps_inline_case_anchors_when_using_tabs() {
        let source = concat!(
            "import Base\n",
            "def f(n: Bool) -> U32:\n",
            "    match n: case True{}: 0\n",
            "             case False{}: 1\n",
        );
        let formatted = format_bend(source, 4, false);
        assert_eq!(
            formatted,
            concat!(
                "import Base\n",
                "def f(n: Bool) -> U32:\n",
                "\tmatch n: case True{}: 0\n",
                "\t         case False{}: 1\n",
            ),
        );
        assert_eq!(format_bend(&formatted, 4, false), formatted);
    }

    #[test]
    fn formatting_pins_count_non_bmp_literals_as_utf16_columns() {
        let source = concat!(
            "import Base\n",
            "def main()->String & Array<U32>:\n",
            "  a = [0 : U32 * 2n]\n",
            "  (\"😀\",a[0]<-3\n",
            "        a[1]<-5)\n",
        );
        let formatted = format_bend(source, 2, true);
        assert_eq!(
            formatted,
            concat!(
                "import Base\n",
                "def main() -> String & Array<U32>:\n",
                "  a = [0: U32 * 2n]\n",
                "  (\"😀\", a[0] <- 3\n",
                "         a[1] <- 5)\n",
            ),
        );
        assert_eq!(format_bend(&formatted, 2, true), formatted);
    }

    #[test]
    fn formatting_preserves_raw_apostrophes_and_their_following_comments() {
        let source = "import Base\ndef main()->Char:\n    '''# quote\n";
        let formatted = format_bend(source, 2, true);
        assert_eq!(
            formatted,
            "import Base\ndef main() -> Char:\n  '''  # quote\n"
        );
        assert_eq!(format_bend(&formatted, 2, true), formatted);
    }
}
