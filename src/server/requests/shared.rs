use super::super::{
    features::{cursor_in_comment_or_string, token_at},
    lsp::Backend,
};
use crate::analysis::{self, DocumentSnapshot};
use tokio::sync::RwLockReadGuard;
use url::Url;

impl Backend {
    fn prelude_load_finished(&self) -> bool {
        if *self.compiler.base_module_attempted.read() {
            return true;
        }
        self.compiler.base_module.read().is_some()
    }

    pub(super) async fn document_read_with_prelude(&self, uri: &Url) -> RwLockReadGuard<'_, ()> {
        loop {
            let read = self.document_read(uri).await;
            if self.prelude_load_finished() {
                return read;
            }
            let needs_prelude = {
                let database = self.workspace.read();
                database.is_document_open(uri) && database.imports_prelude(uri)
            };
            if !needs_prelude {
                return read;
            }
            // Base registration commits through the same workspace write barrier.
            // Release this view for cold loading, then check the current revision again.
            drop(read);
            self.load_prelude_module().await;
        }
    }

    pub(super) async fn workspace_read_with_prelude(
        &self,
        uri: Option<&Url>,
    ) -> RwLockReadGuard<'_, ()> {
        loop {
            let read = self.workspace_ready_read().await;
            if self.prelude_load_finished() {
                return read;
            }
            let needs_prelude = {
                let database = self.workspace.read();
                match uri {
                    Some(uri) => database.imports_prelude(uri),
                    None => database.workspace_imports_prelude(),
                }
            };
            if !needs_prelude {
                return read;
            }
            drop(read);
            self.load_prelude_module().await;
        }
    }
}

// Declaration queries must not reinterpret a lexical binding or a module alias
// under the cursor as the declaration named by the entire qualified expression.
pub(super) fn declaration_token_at(snapshot: &DocumentSnapshot, offset: usize) -> Option<String> {
    if cursor_in_comment_or_string(snapshot, offset) {
        return None;
    }
    let syntax = &snapshot.syntax;
    let cursor = syntax.token_at_or_before(offset)?;
    let token = syntax.token(cursor)?;
    if offset < token.range.start || offset > token.range.end {
        return None;
    }
    let mut root = cursor;
    while let Some(previous) = root.0.checked_sub(2) {
        let qualifier = analysis::TokenId(previous);
        let dot = analysis::TokenId(previous + 1);
        let Some(qualifier_token) = syntax.token(qualifier) else {
            break;
        };
        let Some(dot_token) = syntax.token(dot) else {
            break;
        };
        let Some(member_token) = syntax.token(root) else {
            break;
        };
        if qualifier_token.kind != analysis::TokenKind::Identifier
            || syntax.token_text(&snapshot.text, dot) != Some(".")
            || qualifier_token.range.end != dot_token.range.start
            || dot_token.range.end != member_token.range.start
        {
            break;
        }
        root = qualifier;
    }
    if syntax
        .symbol_for_token(root)
        .is_some_and(|symbol| syntax.binding_by_id(symbol).is_some())
    {
        return None;
    }
    let root_text = syntax.token_text(&snapshot.text, root)?;
    if root == cursor
        && analysis::imports(snapshot).iter().any(|import| {
            import.alias_text(&snapshot.text) == Some(root_text)
                || import.path.contains(offset)
                || import.alias.is_some_and(|range| range.contains(offset))
        })
    {
        return None;
    }
    Some(token_at(snapshot, offset))
}

pub(super) fn binding_at(snapshot: &DocumentSnapshot, offset: usize) -> Option<analysis::SymbolId> {
    let syntax = &snapshot.syntax;
    let token_id = syntax.token_at_or_before(offset)?;
    let token = syntax.token(token_id)?;
    if token.kind != analysis::TokenKind::Identifier
        || offset < token.range.start
        || offset > token.range.end
    {
        return None;
    }
    let symbol = syntax.symbol_for_token(token_id)?;
    syntax.binding_by_id(symbol).map(|_| symbol)
}

pub(super) fn binding_ranges(
    snapshot: &DocumentSnapshot,
    symbol: analysis::SymbolId,
    include_declaration: bool,
) -> Vec<analysis::TextRange> {
    let declaration = snapshot.syntax.symbol_declaration_range(symbol);
    snapshot
        .syntax
        .references(symbol)
        .map(|reference| reference.range)
        .filter(|range| include_declaration || Some(*range) != declaration)
        .collect()
}
