use super::super::{
    adapters,
    formatter::{self, format_bend},
    lsp::Backend,
};
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        DocumentFormattingParams, DocumentOnTypeFormattingParams, DocumentRangeFormattingParams,
        Position, Range, TextEdit,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let start = adapters::offset_at(&doc, Position::new(params.range.start.line, 0));
        let end = adapters::offset_at(&doc, params.range.end);
        if start > end {
            return Ok(Some(Vec::new()));
        }
        let selected = &doc.text[start..end];
        let formatted = format_bend(
            selected,
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        if formatted == selected {
            return Ok(Some(Vec::new()));
        }
        let first_line = selected
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let indent_length = first_line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let base_indent = &first_line[..indent_length];
        let mut new_text = String::with_capacity(formatted.len() + base_indent.len());
        for line in formatted.split_inclusive('\n') {
            if !line.trim_matches(['\r', '\n']).is_empty() {
                new_text.push_str(base_indent);
            }
            new_text.push_str(line);
        }
        Ok(Some(vec![TextEdit {
            range: Range::new(
                adapters::position_at(&doc, start),
                adapters::position_at(&doc, end),
            ),
            new_text,
        }]))
    }

    pub(in crate::server) async fn handle_on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self
            .document_read(&params.text_document_position.text_document.uri)
            .await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        if params.ch != "\n" {
            return Ok(Some(Vec::new()));
        }
        let offset = adapters::offset_at(&doc, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        let previous = doc.text[..line_start]
            .trim_end_matches(['\r', '\n'])
            .rsplit('\n')
            .next()
            .unwrap_or("");
        let previous_indent = previous
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let previous_indent_text = &previous[..previous_indent];
        let Some(opens_body) = formatter::opens_indented_body(&previous[previous_indent..]) else {
            return Ok(Some(Vec::new()));
        };
        let unit = if params.options.insert_spaces {
            " ".repeat(params.options.tab_size as usize)
        } else {
            "\t".to_owned()
        };
        let desired = if opens_body {
            format!("{previous_indent_text}{unit}")
        } else {
            previous_indent_text.to_owned()
        };
        let line_end = doc.text[offset..]
            .find('\n')
            .map_or(doc.text.len(), |index| offset + index);
        let current_line = &doc.text[line_start..line_end];
        let current_indent = current_line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if current_line[..current_indent] == desired {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(vec![TextEdit {
            range: Range::new(
                adapters::position_at(&doc, line_start),
                adapters::position_at(&doc, line_start + current_indent),
            ),
            new_text: desired,
        }]))
    }

    pub(in crate::server) async fn handle_formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let formatted = format_bend(
            &doc.text,
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        if formatted == doc.text {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: adapters::position_at(&doc, doc.text.len()),
            },
            new_text: formatted,
        }]))
    }
}
