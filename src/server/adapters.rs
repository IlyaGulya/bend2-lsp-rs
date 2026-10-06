use crate::analysis::{self, CompletionKind, DocumentSnapshot, SymbolKind, TextRange};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, DocumentSymbol, FoldingRange, FoldingRangeKind, InlayHint,
    InlayHintKind, InlayHintLabel, Location, ParameterInformation, ParameterLabel, Position, Range,
    SelectionRange, SemanticToken, SignatureHelp, SignatureInformation, SymbolInformation,
};
use url::Url;

pub(super) fn position_at(snapshot: &DocumentSnapshot, offset: usize) -> Position {
    let (line, character) = snapshot.line_index.position(&snapshot.text, offset);
    Position::new(line, character)
}

pub(super) fn offset_at(snapshot: &DocumentSnapshot, position: Position) -> usize {
    snapshot
        .line_index
        .offset(&snapshot.text, position.line, position.character)
}

pub(super) fn range(snapshot: &DocumentSnapshot, range: TextRange) -> Range {
    Range::new(
        position_at(snapshot, range.start),
        position_at(snapshot, range.end),
    )
}

pub(super) fn text_range(snapshot: &DocumentSnapshot, range: Range) -> TextRange {
    TextRange::new(
        offset_at(snapshot, range.start),
        offset_at(snapshot, range.end),
    )
}

#[tracing::instrument(name = "analysis.query", skip_all, fields(kind = "document_symbols", result_count = tracing::field::Empty))]
pub(super) fn document_symbols(snapshot: &DocumentSnapshot) -> Vec<DocumentSymbol> {
    let symbols = analysis::document_symbols(snapshot)
        .into_iter()
        .map(|symbol| document_symbol(snapshot, symbol))
        .collect::<Vec<_>>();
    tracing::Span::current().record("result_count", symbols.len());
    symbols
}

pub(super) fn document_symbol_by_id(
    snapshot: &DocumentSnapshot,
    id: analysis::SymbolId,
) -> Option<DocumentSymbol> {
    analysis::document_symbol_by_id(snapshot, id).map(|symbol| document_symbol(snapshot, symbol))
}

pub(super) fn document_symbol(
    snapshot: &DocumentSnapshot,
    symbol: analysis::Symbol,
) -> DocumentSymbol {
    let children = (!symbol.children.is_empty()).then(|| {
        symbol
            .children
            .into_iter()
            .map(|child| document_symbol(snapshot, child))
            .collect()
    });
    DocumentSymbol {
        name: symbol.name,
        detail: Some(symbol.detail),
        kind: symbol_kind(symbol.kind),
        tags: None,
        deprecated: None,
        range: range(snapshot, symbol.range),
        selection_range: range(snapshot, symbol.selection_range),
        children,
    }
}

pub(super) fn completion_items(items: Vec<analysis::Completion>) -> Vec<CompletionItem> {
    items
        .into_iter()
        .map(|item| {
            let mut result = CompletionItem::new_simple(item.label, item.detail);
            result.kind = Some(match item.kind {
                CompletionKind::Function => CompletionItemKind::FUNCTION,
                CompletionKind::Struct => CompletionItemKind::STRUCT,
                CompletionKind::Keyword => CompletionItemKind::KEYWORD,
                CompletionKind::Variable => CompletionItemKind::VARIABLE,
            });
            result
        })
        .collect()
}

pub(super) fn inlay_hints(
    snapshot: &DocumentSnapshot,
    hints: Vec<analysis::InlayHint>,
) -> Vec<InlayHint> {
    hints
        .into_iter()
        .map(|hint| InlayHint {
            position: position_at(snapshot, hint.position),
            label: InlayHintLabel::String(hint.label(&snapshot.text)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: Some(true),
            data: None,
        })
        .collect()
}

pub(super) fn folding_ranges(ranges: Vec<analysis::FoldingRange>) -> Vec<FoldingRange> {
    ranges
        .into_iter()
        .map(|range| FoldingRange {
            start_line: u32::try_from(range.start_line).unwrap_or(u32::MAX),
            start_character: None,
            end_line: u32::try_from(range.end_line).unwrap_or(u32::MAX),
            end_character: None,
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: None,
        })
        .collect()
}

pub(super) fn selection_range(
    snapshot: &DocumentSnapshot,
    selection: analysis::SelectionRange,
) -> SelectionRange {
    SelectionRange {
        range: range(snapshot, selection.range),
        parent: selection
            .parent
            .map(|parent| Box::new(selection_range(snapshot, *parent))),
    }
}

pub(super) fn semantic_tokens(
    snapshot: &DocumentSnapshot,
    tokens: Vec<analysis::SemanticToken>,
) -> Vec<SemanticToken> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut previous_line = 0;
    let mut previous_character = 0;
    for token in tokens {
        let start = position_at(snapshot, token.range.start);
        let end = position_at(snapshot, token.range.end);
        if start.line != end.line || start.character == end.character {
            continue;
        }
        result.push(SemanticToken {
            delta_line: start.line - previous_line,
            delta_start: if start.line == previous_line {
                start.character - previous_character
            } else {
                start.character
            },
            length: end.character - start.character,
            token_type: token.token_type,
            token_modifiers_bitset: 0,
        });
        previous_line = start.line;
        previous_character = start.character;
    }
    result
}

pub(super) fn signature_help(help: analysis::SignatureHelp) -> SignatureHelp {
    SignatureHelp {
        signatures: vec![SignatureInformation {
            label: help.label,
            documentation: None,
            parameters: Some(
                help.parameters
                    .into_iter()
                    .map(|parameter| ParameterInformation {
                        label: ParameterLabel::Simple(parameter),
                        documentation: None,
                    })
                    .collect(),
            ),
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: Some(u32::try_from(help.active_parameter).unwrap_or(u32::MAX)),
    }
}

pub(super) fn workspace_symbols(
    snapshot: &DocumentSnapshot,
    uri: &Url,
    query: &str,
) -> Vec<SymbolInformation> {
    analysis::workspace_symbols(snapshot, query)
        .into_iter()
        .map(|symbol| SymbolInformation {
            name: symbol.name,
            kind: symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: range(snapshot, symbol.range),
            },
            container_name: None,
        })
        .collect()
}

fn symbol_kind(kind: SymbolKind) -> tower_lsp::lsp_types::SymbolKind {
    match kind {
        SymbolKind::Function => tower_lsp::lsp_types::SymbolKind::FUNCTION,
        SymbolKind::Struct => tower_lsp::lsp_types::SymbolKind::STRUCT,
        SymbolKind::Constructor => tower_lsp::lsp_types::SymbolKind::ENUM_MEMBER,
    }
}
