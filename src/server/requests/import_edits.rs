use super::super::adapters;
use crate::{
    analysis::{DocumentSnapshot, Reference, ReferenceKind, TextRange, TokenId, TokenKind},
    workspace::{Document, WorkspaceDb},
};
use std::{borrow::Cow, collections::HashMap};
use tower_lsp::lsp_types::{
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, TextEdit,
    WorkspaceEdit,
};
use url::Url;

pub(super) fn versioned_edit(document: &Document, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: document.uri.clone(),
                version: Some(document.revision.0),
            },
            edits: edits.into_iter().map(OneOf::Left).collect(),
        }])),
        ..Default::default()
    }
}

pub(super) fn unresolved_reference(document: &Document, offset: usize) -> Option<&Reference> {
    let syntax = &document.syntax;
    let token = syntax.token_at_or_before(offset)?;
    let indexed = syntax.token(token)?;
    if indexed.kind != TokenKind::Identifier
        || offset > indexed.range.end
        || syntax.is_in_comment_or_string(offset)
        || syntax.imports().iter().any(|import| {
            import.path.contains(offset) || import.alias.is_some_and(|alias| alias.contains(offset))
        })
        || syntax.token(TokenId(token.0 + 1)).is_some_and(|next| {
            next.range.start == indexed.range.end
                && syntax.token_text(&document.text, TokenId(token.0 + 1)) == Some(".")
        })
    {
        return None;
    }
    let reference = syntax.reference_for_token(token)?;
    if reference.kind == ReferenceKind::Declaration
        || reference.resolved.is_some()
        || syntax.is_keyword(reference.name)
    {
        return None;
    }
    if let Some(qualifier) = reference.qualifier {
        let qualifier = syntax.name_text(&document.text, qualifier);
        if syntax
            .imports()
            .iter()
            .any(|import| import.alias_text(&document.text) == Some(qualifier))
            || reference
                .qualifier_token
                .is_some_and(|token| syntax.symbol_for_token(token).is_some())
        {
            return None;
        }
    }
    Some(reference)
}

pub(super) fn auto_import_edits(
    document: &Document,
    workspace: &WorkspaceDb,
    target: &Url,
    path: &str,
    reference: &Reference,
) -> Option<Vec<TextEdit>> {
    let syntax = &document.syntax;
    let offset = reference.range.start;
    let mut already_imported = false;
    let mut alias = None;
    for import in syntax.imports() {
        if (path == "Base" && import.path_text(&document.text) == "Base")
            || workspace
                .import_target(&document.uri, import.path)
                .is_some_and(|doc| &doc.uri == target)
        {
            already_imported = true;
            if let Some(existing) = import.alias_text(&document.text)
                && !syntax
                    .bindings_at(offset)
                    .any(|binding| syntax.name_text(&document.text, binding.name) == existing)
                && syntax
                    .name_id(&document.text, existing)
                    .and_then(|name| syntax.symbol_by_name(name))
                    .is_none()
                && syntax
                    .imports()
                    .iter()
                    .find(|entry| entry.alias_text(&document.text) == Some(existing))
                    .is_some_and(|entry| entry.path == import.path)
            {
                alias = Some(existing.to_owned());
                break;
            }
        }
    }
    // Do not change the namespace of an existing unaliased import or add a
    // duplicate import to work around a shadowed module alias.
    if already_imported && alias.is_none() {
        return None;
    }
    let name = syntax.name_text(&document.text, reference.name);
    let mut edits = Vec::with_capacity(2);
    let alias = if let Some(alias) = alias {
        alias
    } else {
        let stem = if path == "Base" {
            "Base"
        } else {
            path.rsplit('/').next()?.strip_suffix(".bend")?
        };
        let mut base: String = stem
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        if base.is_empty() || base.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            base.insert_str(0, "Module");
        }
        if let Some(first) = base.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let mut alias = base.clone();
        let mut suffix = 2_u64;
        while syntax.name_id(&document.text, &alias).is_some()
            || syntax
                .imports()
                .iter()
                .any(|import| import.alias_text(&document.text) == Some(&alias))
        {
            alias = format!("{base}{suffix}");
            suffix += 1;
        }
        edits.push(new_import_edit(document, path, &alias));
        alias
    };
    let start = reference
        .qualifier_token
        .and_then(|token| syntax.token(token))
        .map_or(reference.range.start, |token| token.range.start);
    edits.push(TextEdit {
        range: adapters::range(document, TextRange::new(start, reference.range.end)),
        new_text: format!("{alias}.{name}"),
    });
    Some(edits)
}

fn new_import_edit(document: &Document, path: &str, alias: &str) -> TextEdit {
    let syntax = &document.syntax;
    let insertion = syntax.imports().last().map_or_else(
        || {
            syntax
                .tokens()
                .iter()
                .find(|token| token.kind != TokenKind::Comment)
                .and_then(|token| {
                    let line = adapters::position_at(document, token.range.start).line as usize;
                    syntax.line_content_range(line).map(|line| line.start)
                })
                .unwrap_or(document.text.len())
        },
        |import| {
            let line = adapters::position_at(document, import.path.start).line as usize;
            syntax.line_full_end(line).unwrap_or(import.path.end)
        },
    );
    let newline = syntax
        .line_content_range(0)
        .and_then(|line| document.text.as_bytes().get(line.end))
        .map_or("\n", |byte| if *byte == b'\r' { "\r\n" } else { "\n" });
    let prefix = if insertion > 0 && document.text.as_bytes().get(insertion - 1) != Some(&b'\n') {
        newline
    } else {
        ""
    };
    TextEdit {
        range: adapters::range(document, TextRange::new(insertion, insertion)),
        new_text: format!("{prefix}import {path} as {alias}{newline}"),
    }
}

struct ImportRecord<'a> {
    path: &'a str,
    target: Option<Url>,
    alias: Option<&'a str>,
    range: TextRange,
    line_start: usize,
    comment: Option<TextRange>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ImportIdentity<'a> {
    Target(&'a Url),
    Path(&'a str),
}

impl ImportRecord<'_> {
    fn identity(&self) -> ImportIdentity<'_> {
        self.target
            .as_ref()
            .map_or(ImportIdentity::Path(self.path), ImportIdentity::Target)
    }
}

fn comment_only(snapshot: &DocumentSnapshot, line: usize) -> bool {
    let Some(content) = snapshot.syntax.line_content_range(line) else {
        return false;
    };
    let tokens = snapshot.syntax.tokens();
    let first = tokens.partition_point(|token| token.range.end <= content.start);
    tokens
        .get(first)
        .is_some_and(|token| token.kind == TokenKind::Comment && token.range.start < content.end)
}

pub(super) fn organize_imports(document: &Document, workspace: &WorkspaceDb) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let mut group = Vec::new();
    let mut previous_line = None;
    for import in document.syntax.imports() {
        let line = adapters::position_at(document, import.path.start).line as usize;
        let Some(content) = document.syntax.line_content_range(line) else {
            continue;
        };
        let Some(end) = document.syntax.line_full_end(line) else {
            continue;
        };
        let attached_start = if let Some(previous) = previous_line {
            if (previous + 1..line).all(|line| comment_only(document, line)) {
                document
                    .syntax
                    .line_full_end(previous)
                    .unwrap_or(content.start)
            } else {
                organize_group(document, &mut group, &mut edits);
                let mut start_line = line;
                while start_line > previous + 1 && comment_only(document, start_line - 1) {
                    start_line -= 1;
                }
                document
                    .syntax
                    .line_content_range(start_line)
                    .map_or(content.start, |line| line.start)
            }
        } else {
            content.start
        };
        let tokens = document.syntax.tokens();
        let first = tokens.partition_point(|token| token.range.end <= import.path.end);
        let comment = tokens[first..]
            .iter()
            .take_while(|token| token.range.start < content.end)
            .find(|token| token.kind == TokenKind::Comment)
            .map(|token| token.range);
        let import_end = import.alias.map_or(import.path.end, |alias| alias.end);
        let suffix_end = comment.map_or(content.end, |comment| comment.start);
        // Incomplete or nonstandard import suffixes are not safe to rewrite.
        if !document.text[import_end..suffix_end].trim().is_empty() {
            organize_group(document, &mut group, &mut edits);
            previous_line = None;
            continue;
        }
        group.push(ImportRecord {
            path: import.path_text(&document.text),
            target: workspace
                .import_target(&document.uri, import.path)
                .map(|target| target.uri),
            alias: import.alias_text(&document.text),
            range: TextRange::new(attached_start, end),
            line_start: content.start,
            comment,
        });
        previous_line = Some(line);
    }
    organize_group(document, &mut group, &mut edits);
    edits
}

fn organize_group(
    document: &Document,
    group: &mut Vec<ImportRecord<'_>>,
    edits: &mut Vec<TextEdit>,
) {
    let (Some(first), Some(last)) = (group.first(), group.last()) else {
        return;
    };
    let range = TextRange::new(first.range.start, last.range.end);
    let original = &document.text[range.start..range.end];
    let newline = if document.text[first.range.start..first.range.end].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut aliases = HashMap::new();
    let conflicting = group.iter().any(|record| {
        aliases
            .insert(record.alias, record.identity())
            .is_some_and(|identity| identity != record.identity())
    });
    let mut indices: Vec<_> = (0..group.len()).collect();
    if !conflicting {
        indices.sort_by_key(|index| (group[*index].path, group[*index].alias));
    }
    let mut seen: HashMap<_, usize> = HashMap::new();
    let mut chunks: Vec<Cow<'_, str>> = Vec::with_capacity(group.len());
    for index in indices {
        let record = &group[index];
        let key = (record.identity(), record.alias);
        if let Some(previous) = seen.get(&key).copied() {
            let mut comments = document.text[record.range.start..record.line_start].to_owned();
            if let Some(comment) = record.comment {
                comments.push_str(&document.text[comment.start..record.range.end]);
                if !comments.ends_with('\n') {
                    comments.push_str(newline);
                }
            }
            // Keep duplicate comments attached to the surviving equivalent import.
            let chunk = chunks[previous].to_mut();
            chunk.insert_str(0, &comments);
        } else {
            seen.insert(key, chunks.len());
            let mut chunk = Cow::Borrowed(&document.text[record.range.start..record.range.end]);
            // A last import without a newline may move ahead of another import.
            if !chunk.ends_with('\n') && group.len() > 1 {
                chunk.to_mut().push_str(newline);
            }
            chunks.push(chunk);
        }
    }
    let mut replacement = chunks.concat();
    if !original.ends_with('\n') && replacement.ends_with('\n') {
        replacement.pop();
        if replacement.ends_with('\r') {
            replacement.pop();
        }
    }
    if replacement != original {
        edits.push(TextEdit {
            range: adapters::range(document, range),
            new_text: replacement,
        });
    }
    group.clear();
}
