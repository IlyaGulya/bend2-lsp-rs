#[derive(Clone)]
struct Token {
    text: String,
    kind: u8,
    gap: bool,
}
pub(super) fn format_bend(source: &str, tab_size: usize, spaces: bool) -> String {
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let final_eol = source.ends_with('\n');
    let mut raw: Vec<&str> = source.split('\n').collect();
    if final_eol {
        raw.pop();
    }
    let mut lines = Vec::new();
    for raw_line in raw {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let indent_len = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let indent = &line[..indent_len];
        let body = &line[indent_len..];
        let Some((code, comment)) = split_comment(body) else {
            return source.into();
        };
        lines.push((
            indent.to_string(),
            code.trim().to_string(),
            comment.to_string(),
        ));
    }
    let mut stack = vec![0usize];
    let mut depths = Vec::new();
    for (indent, code, comment) in &lines {
        if code.is_empty() && comment.is_empty() {
            depths.push(0);
            continue;
        }
        let width = indent
            .chars()
            .fold(0, |n, c| if c == '\t' { n + (8 - n % 8) } else { n + 1 });
        while stack.len() > 1 && width < stack.last().copied().unwrap_or_default() {
            stack.pop();
        }
        let previous_width = stack.last().copied().unwrap_or_default();
        if width > previous_width {
            stack.push(width);
        } else if width != previous_width
            && let Some(last_width) = stack.last_mut()
        {
            *last_width = width;
        }
        depths.push(stack.len() - 1);
    }
    let formatted: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, (_, code, comment))| {
            if code.is_empty() && comment.is_empty() {
                return String::new();
            }
            let prefix = if spaces {
                " ".repeat(depths[i] * tab_size.max(1))
            } else {
                "\t".repeat(depths[i])
            };
            let tokens = lex(code);
            let code = format_tokens(&tokens);
            if code.is_empty() {
                prefix + comment
            } else if comment.is_empty() {
                prefix + &code
            } else {
                prefix + &code + "  " + comment
            }
        })
        .collect();
    let result = formatted.join(eol) + if final_eol { eol } else { "" };
    if fingerprint(source) != fingerprint(&result) {
        return source.into();
    }
    result
}
fn fingerprint(source: &str) -> Option<Vec<(usize, Vec<String>)>> {
    let mut raw: Vec<&str> = source.split('\n').collect();
    if source.ends_with('\n') {
        raw.pop();
    }
    let mut stack = vec![0usize];
    let mut result = Vec::with_capacity(raw.len());
    for line in raw {
        let indent_len = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let indent = &line[..indent_len];
        let (code, comment) = split_comment(&line[indent_len..])?;
        let code = code.trim();
        let tokens = lex(code);
        let width = indent
            .chars()
            .fold(0, |n, c| if c == '\t' { n + (8 - n % 8) } else { n + 1 });
        while stack.len() > 1 && width < *stack.last()? {
            stack.pop();
        }
        if width > *stack.last()? {
            stack.push(width);
        } else if width != *stack.last()? {
            *stack.last_mut()? = width;
        }
        let depth = if tokens.is_empty() && comment.is_empty() {
            0
        } else {
            stack.len() - 1
        };
        result.push((depth, tokens.into_iter().map(|token| token.text).collect()));
    }
    Some(result)
}
fn split_comment(s: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '#' {
            return Some((&s[..i], &s[i..]));
        }
    }
    quote.is_none().then_some((s, ""))
}
fn lex_import_path(
    code: &str,
    chars: &[(usize, char)],
    mut index: usize,
) -> Option<(Token, usize)> {
    while index < chars.len() && chars[index].1.is_whitespace() {
        index += 1;
    }
    let start = chars.get(index)?.0;
    while index < chars.len() && !chars[index].1.is_whitespace() {
        index += 1;
    }
    let end = chars.get(index).map_or(code.len(), |(offset, _)| *offset);
    Some((
        Token {
            text: code[start..end].into(),
            kind: 0,
            gap: true,
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

fn lex(code: &str) -> Vec<Token> {
    const MULTI: [&str; 17] = [
        "<&>", ".|.", ".^.", ".&.", "==", "!=", "->", "<-", "=>", "&&", "||", "++", "<>", "<=",
        ">=", "<<", ">>",
    ];
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let mut i = 0;
    let mut had_gap = false;
    let mut tokens = Vec::new();
    while i < chars.len() {
        if tokens
            .last()
            .is_some_and(|token: &Token| token.text == "import")
        {
            let Some((token, next_index)) = lex_import_path(code, &chars, i) else {
                break;
            };
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
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let next = chars[i].1;
                i += 1;
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == c {
                    break;
                }
            }
            if i == chars.len() && chars.last().is_some_and(|item| item.1 != c) {
                return Vec::new();
            }
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
            let multi = MULTI.iter().find(|operator| rest.starts_with(**operator));
            i += multi.map_or(1, |operator| operator.chars().count());
        }
        let end = if i < chars.len() {
            chars[i].0
        } else {
            code.len()
        };
        tokens.push(Token {
            text: code[start..end].into(),
            kind,
            gap: had_gap,
        });
        had_gap = false;
    }
    tokens
}
fn format_tokens(tokens: &[Token]) -> String {
    let mut out = String::new();
    for i in 0..tokens.len() {
        if i > 0 && needs_space(tokens, i) {
            out.push(' ');
        }
        out.push_str(&tokens[i].text);
    }
    out
}
const BINARY: &[&str] = &[
    "=", "==", "!=", "->", "<-", "=>", "+", "-", "*", "/", "%", "&&", "||", "++", "<>", "<&>",
    "<=", ">=", "<<", ">>", ".|.", ".^.", ".&.", "&", "|",
];
fn unary(tokens: &[Token], index: usize) -> bool {
    let token = tokens[index].text.as_str();
    if !["+", "-", "~", "?", "@", "&", "%"].contains(&token) {
        return false;
    }
    let previous = index.checked_sub(1).map(|i| tokens[i].text.as_str());
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
fn keep_angle_gap(left: &Token, right: &Token) -> Option<bool> {
    let angles = ["<", ">", "<<", ">>"];
    (angles.contains(&left.text.as_str()) || angles.contains(&right.text.as_str()))
        .then_some(right.gap)
}
fn needs_space(t: &[Token], i: usize) -> bool {
    let left = &t[i - 1];
    let right = &t[i];
    let a = left.text.as_str();
    let b = right.text.as_str();
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
        return false;
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
}
