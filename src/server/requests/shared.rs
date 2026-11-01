use super::super::lsp::Backend;
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
