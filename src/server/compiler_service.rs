use super::{
    compiler::{
        CachedCompilerResult, CompilerCompatibility, CompilerConfig, CompilerReapers,
        CompilerStamp, compiler_base, compiler_compatibility, compiler_stamp,
    },
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

struct CachedCompatibility {
    stamp: Option<CompilerStamp>,
    result: Arc<CompilerCompatibility>,
}

pub(super) struct CompilerService {
    pub(super) config: State<CompilerConfig>,
    pub(super) base_module: State<Option<BaseModule>>,
    pub(super) base_module_attempted: State<bool>,
    pub(super) base_module_load: Mutex<()>,
    pub(super) semaphore: Arc<Semaphore>,
    pub(super) results: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
    pub(super) reapers: Arc<CompilerReapers>,
    compatibility: Mutex<HashMap<CompilerConfig, CachedCompatibility>>,
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
            compatibility: Mutex::new(HashMap::new()),
            reapers,
        }
    }
}

impl CompilerService {
    pub(super) async fn invalidate_compatibility(&self) {
        self.compatibility.lock().await.clear();
    }
    pub(super) async fn compatibility(
        &self,
        config: &CompilerConfig,
    ) -> Arc<CompilerCompatibility> {
        let mut cache = self.compatibility.lock().await;
        let path = config.path.clone();
        let stamp = super::state::blocking_result(
            tokio::task::spawn_blocking(move || compiler_stamp(&path)).await,
        );
        if let Some(cached) = cache.get(config)
            && cached.stamp == stamp
        {
            return cached.result.clone();
        }
        let result = compiler_compatibility(config, self.reapers.clone()).await;
        if let CompilerCompatibility::Available { version } = &result {
            tracing::info!(compiler = %config.path, %version, "Bend compiler CLI is compatible");
        }
        if result.failure().is_none() && self.base_module.read().is_none() {
            *self.base_module_attempted.write() = false;
        }
        let result = Arc::new(result);
        if stamp.is_some()
            && result
                .failure()
                .is_none_or(|(_, code)| code != "compiler-unavailable")
        {
            if cache.len() >= 32 {
                cache.clear();
            }
            cache.insert(
                config.clone(),
                CachedCompatibility {
                    stamp,
                    result: result.clone(),
                },
            );
        } else {
            cache.remove(config);
        }
        result
    }
}

impl Backend {
    pub(super) async fn check_compiler_compatibility(&self) -> Arc<CompilerCompatibility> {
        let config = self.compiler.config.read().clone();
        self.compiler.compatibility(&config).await
    }
    pub(super) async fn load_prelude_module(&self) {
        if self.compiler.base_module.read().is_some() {
            return;
        }
        let _load = self.compiler.base_module_load.lock().await;
        if self.compiler.base_module.read().is_some() {
            return;
        }
        let config = self.compiler.config.read().clone();
        if self
            .compiler
            .compatibility(&config)
            .await
            .failure()
            .is_some()
        {
            *self.compiler.base_module_attempted.write() = true;
            return;
        }
        if *self.compiler.base_module_attempted.read() {
            return;
        }
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
