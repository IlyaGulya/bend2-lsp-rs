use super::{
    compiler::{CachedCompilerResult, CompilerConfig, CompilerReapers, compiler_base},
    lsp::Backend,
    orchestration::run_staging,
    state::State,
};
use crate::analysis::{DocumentSnapshot, Revision};
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, Semaphore};
use url::Url;

#[derive(Clone)]
pub(super) struct BaseModule {
    pub(super) uri: Url,
    pub(super) snapshot: Arc<DocumentSnapshot>,
}
impl Deref for BaseModule {
    type Target = DocumentSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

pub(super) struct CompilerService {
    pub(super) config: State<CompilerConfig>,
    pub(super) base_module: State<Option<BaseModule>>,
    pub(super) base_module_attempted: State<bool>,
    pub(super) base_module_load: Mutex<()>,
    pub(super) semaphore: Arc<Semaphore>,
    pub(super) results: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
    pub(super) reapers: Arc<CompilerReapers>,
}
impl CompilerService {
    pub(super) fn new(reapers: Arc<CompilerReapers>) -> Self {
        Self {
            config: State::new(CompilerConfig::default()),
            base_module: State::new(None),
            base_module_attempted: State::new(false),
            base_module_load: Mutex::new(()),
            semaphore: Arc::new(Semaphore::new(4)),
            results: Arc::new(RwLock::new(HashMap::new())),
            reapers,
        }
    }
}

impl Backend {
    pub(super) async fn load_prelude_module(&self) {
        if self.compiler.base_module.read().is_some() || *self.compiler.base_module_attempted.read()
        {
            return;
        }
        let _load = self.compiler.base_module_load.lock().await;
        if self.compiler.base_module.read().is_some() || *self.compiler.base_module_attempted.read()
        {
            return;
        }
        let config = self.compiler.config.read().clone();
        let output = compiler_base(&config, self.compiler.reapers.clone()).await;
        let source = output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .filter(|source| !source.is_empty());
        if let Some(source) = source {
            let loaded = run_staging(self.workspace.staging.clone(), move || {
                let directory = tempfile::tempdir().ok()?;
                let path = directory.path().join("Base.bend");
                std::fs::write(&path, &source).ok()?;
                let uri = Url::from_file_path(&path).ok()?;
                let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, source));
                Some((BaseModule { uri, snapshot }, path, directory))
            })
            .await
            .flatten();
            if let Some((module, path, directory)) = loaded {
                let _workspace_update = self.workspace.updates.write().await;
                self.workspace.commit(None, None, |database| {
                    database.register_compiler_document(
                        module.uri.clone(),
                        path,
                        module.snapshot.clone(),
                        directory,
                    );
                    *self.compiler.base_module.write() = Some(module);
                    Some(())
                });
            }
        }
        *self.compiler.base_module_attempted.write() = true;
    }
}
