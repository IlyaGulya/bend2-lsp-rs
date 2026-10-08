use crate::analysis::{self, CompletionKind, DocumentSnapshot, SymbolKind, TextRange};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, DocumentSymbol, FoldingRange,
    FoldingRangeKind, InlayHint, InlayHintKind, InlayHintLabel, Location, ParameterInformation,
    ParameterLabel, Position, Range, SelectionRange, SemanticToken, SignatureHelp,
    SignatureInformation, SymbolInformation, TextEdit,
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
                CompletionKind::Constructor => CompletionItemKind::CONSTRUCTOR,
            });
            result
        })
        .collect()
}

pub(super) fn completion_sort_text(
    item: &CompletionItem,
    prefix: &str,
    out_of_scope: bool,
) -> Option<String> {
    let (class, gaps, length) = analysis::completion_match(&item.label, prefix)?;
    let priority = u8::from(out_of_scope);
    let scope = u8::from(item.kind != Some(CompletionItemKind::VARIABLE));
    Some(format!(
        "{priority}{class:01}{gaps:010}{scope:01}{length:010}{}",
        item.label
    ))
}

/// Apply snapshot-local edits without disturbing an auto-import's additional edits.
pub(super) fn finish_completion_items(
    snapshot: &DocumentSnapshot,
    replacement: TextRange,
    prefix: &str,
    mut items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    let replacement = range(snapshot, replacement);
    for item in &mut items {
        if item.sort_text.is_none() {
            item.sort_text = completion_sort_text(item, prefix, false);
        }
        item.filter_text = Some(item.label.clone());
        if item.text_edit.is_none() {
            let mut new_text = item
                .insert_text
                .take()
                .unwrap_or_else(|| item.label.clone());
            if let Some(edits) = &mut item.additional_text_edits {
                edits.retain(|edit| {
                    if edit.range.start == edit.range.end
                        && replacement.start <= edit.range.start
                        && edit.range.start <= replacement.end
                    {
                        if edit.range.start == replacement.end && replacement.start != replacement.end {
                            new_text.push_str(&edit.new_text);
                        } else {
                            new_text.insert_str(0, &edit.new_text);
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range: replacement,
                new_text,
            }));
        }
    }
    items.sort_unstable_by(|left, right| left.sort_text.cmp(&right.sort_text));
    items
}

/// Match file names without making leading `./`, directories, or `.bend` part
/// of a filename's exact/prefix rank. Explicit directories still constrain the
/// candidate through its import path.
pub(super) fn import_completion_match(
    target: &str,
    prefix: &str,
) -> Option<(u8, usize, usize)> {
    let component = prefix.rsplit('/').next().unwrap_or(prefix);
    let filename = target.rsplit('/').next().unwrap_or(target);
    let with_extension = component.contains('.');
    let filename = if with_extension {
        filename
    } else {
        filename.strip_suffix(".bend").unwrap_or(filename)
    };
    let path = if with_extension {
        target
    } else {
        target.strip_suffix(".bend").unwrap_or(target)
    };
    let path = if prefix.starts_with("./") {
        path
    } else {
        path.strip_prefix("./").unwrap_or(path)
    };
    let path_match = analysis::completion_match(path, prefix);
    if prefix.contains('/') {
        path_match
    } else {
        analysis::completion_match(filename, prefix)
            .into_iter()
            .chain(path_match)
            .min()
    }
}

/// Zed filters LSP items against the surrounding completion word, not the
/// replacement edit. Keep its searchable file component separate from the full
/// import label and insertion text, and recompute it when the path changes.
pub(super) fn finish_import_completion_items(
    snapshot: &DocumentSnapshot,
    replacement: TextRange,
    prefix: &str,
    mut items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    let replacement = range(snapshot, replacement);
    let component = prefix.rsplit('/').next().unwrap_or(prefix);
    for item in &mut items {
        if let Some((class, gaps, length)) = import_completion_match(&item.label, prefix) {
            item.sort_text = Some(format!("{class:01}{gaps:010}{length:010}{}", item.label));
        }
        let filename = item.label.rsplit('/').next().unwrap_or(&item.label);
        let filename = if component.contains('.') {
            filename
        } else {
            filename.strip_suffix(".bend").unwrap_or(filename)
        };
        let path = if component.contains('.') {
            item.label.as_str()
        } else {
            item.label.strip_suffix(".bend").unwrap_or(&item.label)
        };
        let path = if prefix.starts_with("./") {
            path
        } else {
            path.strip_prefix("./").unwrap_or(path)
        };
        let filename_match = analysis::completion_match(filename, component);
        let path_match = analysis::completion_match(path, component);
        let filter = if filename_match.is_some()
            && (path_match.is_none() || filename_match <= path_match)
        {
            filename
        } else {
            path
        };
        item.filter_text = Some(filter.to_owned());
        if item.text_edit.is_none() {
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range: replacement,
                new_text: item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone()),
            }));
        }
    }
    items.sort_unstable_by(|left, right| left.sort_text.cmp(&right.sort_text));
    items
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
