use crate::analysis::{DocumentSnapshot, LineIndex};
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;

/// Editable staging text. Every LSP range addresses the result of prior changes
/// in the same notification; snapshots remain immutable throughout staging.
pub(super) struct DocumentText {
    text: String,
    line_index: LineIndex,
}

impl DocumentText {
    pub(super) fn apply_changes(
        document: &DocumentSnapshot,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Self {
        let mut text = if changes.first().is_some_and(|change| change.range.is_none()) {
            String::new()
        } else {
            document.text.clone()
        };
        let mut line_index = None;
        for change in changes {
            if let Some(range) = change.range {
                let current_index = line_index.as_ref().unwrap_or(&document.line_index);
                let start = current_index.offset(&text, range.start.line, range.start.character);
                let end = current_index.offset(&text, range.end.line, range.end.character);
                if start <= end {
                    text.replace_range(start..end, &change.text);
                }
            } else {
                text = change.text;
            }
            line_index = Some(LineIndex::new(&text));
        }
        Self {
            text,
            line_index: line_index.unwrap_or_else(|| document.line_index.clone()),
        }
    }

    pub(super) fn into_parts(self) -> (String, LineIndex) {
        (self.text, self.line_index)
    }
}
