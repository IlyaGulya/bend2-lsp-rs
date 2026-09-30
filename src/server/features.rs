use super::adapters;
use crate::analysis::{DiagnosticKind, DocumentSnapshot, TextRange, TokenKind};
use tower_lsp::lsp_types::{
    CallHierarchyItem, Diagnostic, DiagnosticSeverity, DocumentSymbol, NumberOrString, SymbolKind,
    TypeHierarchyItem,
};
use url::Url;
pub(super) fn named_document_symbol(
    snapshot: &DocumentSnapshot,
    name: &str,
) -> Option<DocumentSymbol> {
    for symbol in adapters::document_symbols(snapshot) {
        if symbol.name == name {
            return Some(symbol);
        }
        if let Some(child) = symbol
            .children
            .and_then(|children| children.into_iter().find(|child| child.name == name))
        {
            return Some(child);
        }
    }
    None
}

pub(super) fn call_hierarchy_item(uri: Url, symbol: DocumentSymbol) -> CallHierarchyItem {
    CallHierarchyItem {
        name: symbol.name,
        kind: symbol.kind,
        tags: None,
        detail: symbol.detail,
        uri,
        range: symbol.range,
        selection_range: symbol.selection_range,
        data: None,
    }
}

pub(super) fn type_hierarchy_symbol(
    snapshot: &DocumentSnapshot,
    name: &str,
) -> Option<(DocumentSymbol, Option<DocumentSymbol>)> {
    for symbol in adapters::document_symbols(snapshot) {
        if symbol.kind == SymbolKind::STRUCT && symbol.name == name {
            return Some((symbol, None));
        }
        if let Some(child) = symbol
            .children
            .as_ref()
            .and_then(|children| children.iter().find(|child| child.name == name))
        {
            return Some((child.clone(), Some(symbol)));
        }
    }
    None
}

pub(super) fn type_hierarchy_item(uri: Url, symbol: DocumentSymbol) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: symbol.name,
        kind: symbol.kind,
        tags: None,
        detail: symbol.detail,
        uri,
        range: symbol.range,
        selection_range: symbol.selection_range,
        data: None,
    }
}
pub(super) fn cursor_in_comment_or_string(snapshot: &DocumentSnapshot, offset: usize) -> bool {
    snapshot.syntax.is_in_comment_or_string(offset)
}
pub(super) fn lexical_diagnostics(snapshot: &DocumentSnapshot) -> Vec<Diagnostic> {
    snapshot
        .syntax
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            let (message, code) = match diagnostic.kind {
                DiagnosticKind::UnresolvedHole => ("Unresolved hole '?TODO'.", "holes"),
                DiagnosticKind::UnterminatedString(quote) => (
                    if quote == '"' {
                        "Unterminated string literal."
                    } else {
                        "Unterminated character literal."
                    },
                    "parsing",
                ),
                DiagnosticKind::UnmatchedDelimiter(delimiter) => {
                    return diag(
                        snapshot,
                        diagnostic.range.start,
                        diagnostic.range.end,
                        format!("Unmatched '{delimiter}'."),
                        "parsing",
                    );
                }
                DiagnosticKind::UnclosedDelimiter(delimiter) => {
                    return diag(
                        snapshot,
                        diagnostic.range.start,
                        diagnostic.range.end,
                        format!("Unclosed '{delimiter}'."),
                        "parsing",
                    );
                }
            };
            diag(
                snapshot,
                diagnostic.range.start,
                diagnostic.range.end,
                message,
                code,
            )
        })
        .collect()
}
pub(super) fn diag(
    snapshot: &DocumentSnapshot,
    start: usize,
    end: usize,
    message: impl Into<String>,
    code: &str,
) -> Diagnostic {
    Diagnostic {
        range: adapters::range(snapshot, TextRange::new(start, end)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(code.into())),
        source: Some("bend2".into()),
        message: message.into(),
        ..Default::default()
    }
}

pub(super) fn code_end_offset(snapshot: &DocumentSnapshot, start: usize) -> usize {
    let start = start.min(snapshot.text.len());
    let tokens = snapshot.syntax.tokens();
    let first = tokens.partition_point(|token| token.range.end <= start);
    tokens[first..]
        .iter()
        .rev()
        .find(|token| token.kind != TokenKind::Comment)
        .map_or(start, |token| token.range.end)
}
pub(super) fn static_hover(token: &str) -> Option<String> {
    let description = match token {
        "def" => "Declares a top-level function.",
        "type" => "Declares an algebraic datatype.",
        "law" => "Declares a proposition that must be proved.",
        "match" => "Pattern-matches one or more values.",
        "case" => "Introduces a match case.",
        "do" => "Sequences IO operations.",
        "return" => "Returns from a `do` block.",
        "for" => "Introduces a universally quantified law variable.",
        "exs" => "Introduces an existential law variable.",
        "where" => "Adds a proposition to a law binder.",
        "import" => "Imports Base or a namespaced `.bend` file.",
        "Type" => "The universe of unrestricted types.",
        "Data" => "The universe of affine data.",
        "Kind" => "A quantity-indexed type universe.",
        "Quant" => "The type of quantities.",
        "->" => "Function type or return-type separator.",
        "=>" => "Lambda body separator.",
        "==" => "Propositional equality.",
        "!=" => "Negated equality.",
        "<&>" => "Quantity minimum.",
        "&0" => "Erased quantity.",
        "&1" => "Affine quantity.",
        "&2" => "Unrestricted quantity.",
        "{==}" => "Reflexivity proof.",
        "%" => "Equality rewrite.",
        "!" => "Runs a parallel call on the GPU when available.",
        _ => return None,
    };
    Some(format!("**{token}**\n\n{description}"))
}
pub(super) fn token_at(snapshot: &DocumentSnapshot, offset: usize) -> String {
    let text = &snapshot.text;
    let tokens = snapshot.syntax.tokens();
    if tokens.is_empty() {
        return String::new();
    }
    let insertion = tokens.partition_point(|token| token.range.start <= offset);
    let first = insertion.saturating_sub(4);
    for start in first..insertion {
        let mut end = start;
        while end < tokens.len() && end - start < 4 {
            if end > start && tokens[end - 1].range.end != tokens[end].range.start {
                break;
            }
            end += 1;
            let range = TextRange::new(tokens[start].range.start, tokens[end - 1].range.end);
            if range.start <= offset
                && offset <= range.end
                && matches!(
                    &text[range.start..range.end],
                    "{==}" | "<&>" | "->" | "=>" | "==" | "!=" | "&0" | "&1" | "&2"
                )
            {
                return text[range.start..range.end].to_owned();
            }
        }
    }
    let Some(index) = insertion.checked_sub(1) else {
        return String::new();
    };
    let token = tokens[index];
    if token.range.start > offset || offset > token.range.end {
        return String::new();
    }
    if matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
        || token.kind == TokenKind::Punctuation
            && matches!(&text[token.range.start..token.range.end], "." | "/")
    {
        let is_name_fragment = |index: usize| {
            let token = tokens[index];
            matches!(token.kind, TokenKind::Identifier | TokenKind::Number)
                || token.kind == TokenKind::Punctuation
                    && matches!(&text[token.range.start..token.range.end], "." | "/")
        };
        let mut start = index;
        while start > 0
            && tokens[start - 1].range.end == tokens[start].range.start
            && is_name_fragment(start - 1)
        {
            start -= 1;
        }
        let mut end = index + 1;
        while end < tokens.len()
            && tokens[end - 1].range.end == tokens[end].range.start
            && is_name_fragment(end)
        {
            end += 1;
        }
        return text[tokens[start].range.start..tokens[end - 1].range.end].to_owned();
    }
    text[token.range.start..token.range.end].to_owned()
}
