use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::Command,
    sync::{Mutex as AsyncMutex, Semaphore, mpsc},
    task::JoinHandle,
    time::sleep,
};
use tower::Service;
use tower_lsp::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::{Request, Response, Result},
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
        CallHierarchyServerCapability, CodeAction, CodeActionKind, CodeActionOrCommand,
        CodeActionParams, CodeActionProviderCapability, CodeActionResponse, CodeLens,
        CodeLensOptions, CodeLensParams, CompletionItem, CompletionItemKind, CompletionOptions,
        CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
        DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, DocumentLink, DocumentLinkOptions, DocumentLinkParams,
        DocumentOnTypeFormattingOptions, DocumentOnTypeFormattingParams,
        DocumentRangeFormattingParams, DocumentSymbol, DocumentSymbolParams,
        DocumentSymbolResponse, FoldingRange, FoldingRangeParams, FoldingRangeProviderCapability,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
        HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams, InlayHint,
        InlayHintParams, Location, MarkupContent, MarkupKind, MessageType, NumberOrString, OneOf,
        Position, Range, ReferenceParams, Registration, RenameParams, SelectionRange,
        SelectionRangeParams, SelectionRangeProviderCapability, SemanticTokenType, SemanticTokens,
        SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
        SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
        ServerCapabilities, ServerInfo, SignatureHelp, SignatureHelpOptions, SignatureHelpParams,
        SymbolInformation, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind,
        TextDocumentSyncOptions, TextEdit, TypeDefinitionProviderCapability, TypeHierarchyItem,
        TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
        WorkDoneProgressOptions, WorkspaceEdit, WorkspaceFoldersServerCapabilities,
        WorkspaceServerCapabilities, WorkspaceSymbolParams,
    },
};
use url::Url;
mod analysis;

#[derive(Clone)]
struct Document {
    uri: Url,
    language_id: String,
    version: i32,
    text: String,
}

#[derive(Clone)]
struct ImportedDiagnostics {
    version: i32,
    by_uri: HashMap<Url, Vec<Diagnostic>>,
}

#[derive(Clone)]
struct CompilerConfig {
    path: String,
    arguments: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct CompilerSnapshot {
    compiler_path: String,
    compiler_arguments: Vec<String>,
    compiler_stamp: Option<(u64, SystemTime)>,
    sources: Vec<(PathBuf, String)>,
}

struct CachedCompilerResult {
    snapshot: CompilerSnapshot,
    diagnostics: Vec<(PathBuf, Diagnostic)>,
}
impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            path: "bend".into(),
            arguments: Vec::new(),
        }
    }
}

impl CompilerConfig {
    fn from_settings(settings: &serde_json::Value) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut warnings = Vec::new();
        let Some(settings) = settings.get("bend2-lsp") else {
            return (config, warnings);
        };
        let Some(settings) = settings.as_object() else {
            warnings.push("bend2-lsp settings must be an object".into());
            return (config, warnings);
        };
        for key in settings.keys() {
            if key != "compilerPath" && key != "compilerArguments" {
                warnings.push(format!("Unknown bend2-lsp setting: {key}"));
            }
        }
        if let Some(value) = settings.get("compilerPath") {
            if let Some(path) = value.as_str().filter(|path| !path.is_empty()) {
                config.path = path.into();
            } else {
                warnings.push("bend2-lsp.compilerPath must be a non-empty string".into());
            }
        }
        if let Some(value) = settings.get("compilerArguments") {
            if let Some(arguments) = value.as_array().and_then(|values| {
                values
                    .iter()
                    .map(serde_json::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            }) {
                config.arguments = arguments.into_iter().map(str::to_owned).collect();
            } else {
                warnings.push("bend2-lsp.compilerArguments must be an array of strings".into());
            }
        }
        (config, warnings)
    }
}

#[derive(Clone)]
struct BaseModule {
    uri: Url,
    source: Arc<str>,
    _directory: Arc<tempfile::TempDir>,
}

type AnalysisTasks = Arc<AsyncMutex<HashMap<Url, (u64, JoinHandle<()>)>>>;

#[derive(Clone)]
struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, Document>>>,
    imported_diagnostics: Arc<RwLock<HashMap<Url, ImportedDiagnostics>>>,
    imported_diagnostic_publish: Arc<AsyncMutex<()>>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    watch_registration: Arc<RwLock<bool>>,
    type_hierarchy_registration: Arc<RwLock<bool>>,
    compiler_config: Arc<RwLock<CompilerConfig>>,
    base_module: Arc<RwLock<Option<BaseModule>>>,
    base_module_attempted: Arc<RwLock<bool>>,
    base_module_load: Arc<AsyncMutex<()>>,
    analysis_tasks: AnalysisTasks,
    next_analysis_id: Arc<AtomicU64>,
    compiler_semaphore: Arc<Semaphore>,
    compiler_results: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
}

impl Backend {
    fn document(&self, uri: &Url) -> Option<Document> {
        self.documents.read().ok()?.get(uri).cloned()
    }
    fn workspace_documents(&self) -> Vec<Document> {
        self.documents
            .read()
            .map(|documents| documents.values().cloned().collect())
            .unwrap_or_default()
    }

    fn document_path(&self, uri: &Url) -> Option<PathBuf> {
        uri.to_file_path()
            .ok()
            .or_else(|| self.virtual_document_path(uri))
    }

    fn virtual_document_path(&self, uri: &Url) -> Option<PathBuf> {
        if uri.scheme() != "untitled" {
            return None;
        }
        let root = self.workspace_roots.read().ok()?.first()?.clone();
        let mut hasher = DefaultHasher::new();
        uri.hash(&mut hasher);
        Some(root.join(format!(".bend2-lsp-virtual-{:016x}.bend", hasher.finish())))
    }

    fn module_document(&self, source: &Document, alias: &str) -> Option<(Url, Arc<str>)> {
        let import = analysis::imports(&source.text)
            .into_iter()
            .find(|import| import.alias.as_deref() == Some(alias))?;
        if import.path == "Base" {
            let base = self.base_module.read().ok()?.clone()?;
            return Some((base.uri, base.source));
        }
        let base = self.document_path(&source.uri)?;
        let path = import_target_path(&base, &import.path)?;
        let uri = Url::from_file_path(&path).ok()?;
        let text = self
            .document(&uri)
            .map(|document| Arc::<str>::from(document.text))
            .or_else(|| std::fs::read_to_string(&path).ok().map(Arc::<str>::from))?;
        Some((uri, text))
    }

    fn prelude_module(&self, source: &Document) -> Option<BaseModule> {
        if !analysis::imports(&source.text)
            .iter()
            .any(|import| import.path == "Base")
        {
            return None;
        }
        self.base_module.read().ok()?.clone()
    }

    fn prelude_declaration(&self, source: &Document, name: &str) -> Option<BaseModule> {
        let module = self.prelude_module(source)?;
        analysis::declaration_range(&module.source, name)?;
        Some(module)
    }

    async fn load_prelude_module(&self) {
        if self.base_module.read().is_ok_and(|module| module.is_some())
            || self
                .base_module_attempted
                .read()
                .is_ok_and(|attempted| *attempted)
        {
            return;
        }
        let _load = self.base_module_load.lock().await;
        if self.base_module.read().is_ok_and(|module| module.is_some())
            || self
                .base_module_attempted
                .read()
                .is_ok_and(|attempted| *attempted)
        {
            return;
        }
        let config = self
            .compiler_config
            .read()
            .map(|config| config.clone())
            .unwrap_or_default();
        let output = Command::new(&config.path)
            .args(&config.arguments)
            .arg("base")
            .output()
            .await;
        let source = output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .filter(|source| !source.is_empty());
        if let Some(source) = source {
            let loaded = tokio::task::spawn_blocking(move || {
                let directory = tempfile::tempdir().ok()?;
                let path = directory.path().join("Base.bend");
                std::fs::write(&path, &source).ok()?;
                let uri = Url::from_file_path(path).ok()?;
                Some(BaseModule {
                    uri,
                    source: Arc::from(source),
                    _directory: Arc::new(directory),
                })
            })
            .await
            .ok()
            .flatten();
            if let Some(module) = loaded
                && let Ok(mut current) = self.base_module.write()
            {
                *current = Some(module);
            }
        }
        if let Ok(mut attempted) = self.base_module_attempted.write() {
            *attempted = true;
        }
    }
    async fn ensure_prelude_module(&self, source: &Document) {
        if analysis::imports(&source.text)
            .iter()
            .any(|import| import.path == "Base")
        {
            self.load_prelude_module().await;
        }
    }

    fn indexed_documents(&self) -> Vec<Document> {
        let mut by_path = HashMap::<PathBuf, Document>::new();
        let mut unpathed = Vec::new();
        let mut pending = Vec::new();
        for document in self.workspace_documents() {
            if let Some(path) = self.document_path(&document.uri) {
                let path = normalize_path(&path);
                by_path.insert(path.clone(), document);
                pending.push(path);
            } else {
                unpathed.push(document);
            }
        }
        let mut visited = HashSet::new();
        while let Some(path) = pending.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            let document = if let Some(document) = by_path.get(&path) {
                document.clone()
            } else {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                let document = Document {
                    uri,
                    language_id: "bend".into(),
                    version: -1,
                    text,
                };
                by_path.insert(path.clone(), document.clone());
                document
            };
            pending.extend(imported_paths(&path, &document.text));
        }
        unpathed.extend(by_path.into_values());
        unpathed
    }

    fn symbol_source(&self, document: &Document, name: &str) -> (Url, Arc<str>, String) {
        if let Some((alias, member)) = name.split_once('.')
            && let Some((uri, source)) = self.module_document(document, alias)
        {
            return (uri, source, member.to_owned());
        }
        (
            document.uri.clone(),
            Arc::from(document.text.clone()),
            name.to_owned(),
        )
    }
    fn hierarchy_source(&self, document: &Document, name: &str) -> Option<(Url, Arc<str>, String)> {
        let (uri, source, member) = self.symbol_source(document, name);
        if named_document_symbol(&source, &member).is_some() {
            return Some((uri, source, member));
        }
        let module = self.prelude_module(document)?;
        named_document_symbol(&module.source, name)?;
        Some((module.uri, module.source, name.to_owned()))
    }

    fn hierarchy_documents(&self) -> Vec<Document> {
        let mut documents = self.indexed_documents();
        if let Some(module) = self
            .base_module
            .read()
            .ok()
            .and_then(|module| module.clone())
            && !documents.iter().any(|document| document.uri == module.uri)
        {
            documents.push(Document {
                uri: module.uri,
                language_id: "bend".into(),
                version: -1,
                text: module.source.to_string(),
            });
        }
        documents
    }

    fn hierarchy_document(&self, uri: &Url) -> Option<Document> {
        if let Some(document) = self.document(uri) {
            return Some(document);
        }
        let path = uri.to_file_path().ok()?;
        Some(Document {
            uri: uri.clone(),
            language_id: "bend".into(),
            version: -1,
            text: std::fs::read_to_string(path).ok()?,
        })
    }

    fn symbol_references(
        &self,
        target_uri: &Url,
        name: &str,
        include_declaration: bool,
    ) -> Vec<Location> {
        let target_path = self
            .document_path(target_uri)
            .map(|path| normalize_path(&path));
        let target_text = self
            .document(target_uri)
            .map(|document| document.text)
            .or_else(|| {
                target_path
                    .as_ref()
                    .and_then(|path| std::fs::read_to_string(path).ok())
            });
        let declaration = target_text
            .as_deref()
            .and_then(|source| analysis::declaration_range(source, name));
        let mut locations = Vec::new();
        for candidate in self.indexed_documents() {
            if !Self::supported(&candidate) {
                continue;
            }
            let candidate_path = self
                .document_path(&candidate.uri)
                .map(|path| normalize_path(&path));
            if candidate_path == target_path {
                for range in analysis::identifier_ranges(&candidate.text, name) {
                    if include_declaration || declaration != Some(range) {
                        locations.push(Location {
                            uri: candidate.uri.clone(),
                            range,
                        });
                    }
                }
                continue;
            }
            for import in analysis::imports(&candidate.text) {
                let Some(alias) = import.alias else {
                    continue;
                };
                let Some((module_uri, _)) = self.module_document(&candidate, &alias) else {
                    continue;
                };
                let module_path = self
                    .document_path(&module_uri)
                    .map(|path| normalize_path(&path));
                if module_path != target_path {
                    continue;
                }
                let qualifier = format!("{alias}.");
                for range in analysis::identifier_ranges(&candidate.text, name) {
                    let start = offset_at(&candidate.text, range.start);
                    if candidate.text[..start].ends_with(&qualifier) {
                        locations.push(Location {
                            uri: candidate.uri.clone(),
                            range,
                        });
                    }
                }
            }
        }
        locations.sort_by_key(|location| {
            (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
            )
        });
        locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
        locations
    }

    fn supported(doc: &Document) -> bool {
        doc.language_id == "bend" || doc.language_id == "bend2"
    }
    fn open_file_overlays(&self) -> HashMap<PathBuf, String> {
        self.documents
            .read()
            .map(|documents| {
                documents
                    .values()
                    .filter_map(|document| {
                        self.document_path(&document.uri)
                            .map(|path| (normalize_path(&path), document.text.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn dependent_documents(&self, changed: &Url) -> Vec<Document> {
        let Some(changed_path) = self.document_path(changed) else {
            return Vec::new();
        };
        let Ok(documents) = self.documents.read() else {
            return Vec::new();
        };
        let mut affected = HashSet::from([normalize_path(&changed_path)]);
        let mut result = Vec::new();
        let mut included = HashSet::new();
        loop {
            let mut found = false;
            for document in documents.values() {
                let Some(path) = self.document_path(&document.uri) else {
                    continue;
                };
                let path = normalize_path(&path);
                if affected.contains(&path)
                    || !imported_paths(&path, &document.text)
                        .iter()
                        .any(|dependency| affected.contains(dependency))
                {
                    continue;
                }
                affected.insert(path);
                if included.insert(document.uri.clone()) {
                    result.push(document.clone());
                }
                found = true;
            }
            if !found {
                break;
            }
        }
        result
    }
    async fn schedule_diagnostics(&self, uri: Url, version: i32) {
        let mut tasks = self.analysis_tasks.lock().await;
        if let Some((_, previous)) = tasks.remove(&uri) {
            previous.abort();
        }
        let generation = self.next_analysis_id.fetch_add(1, Ordering::Relaxed);
        let backend = self.clone();
        let task_uri = uri.clone();
        let handle = tokio::spawn(async move {
            backend.run_diagnostics(task_uri.clone(), version).await;
            let mut tasks = backend.analysis_tasks.lock().await;
            if tasks
                .get(&task_uri)
                .is_some_and(|(current, _)| *current == generation)
            {
                tasks.remove(&task_uri);
            }
        });
        tasks.insert(uri, (generation, handle));
    }

    async fn cancel_diagnostics(&self, uri: &Url) {
        if let Some((_, task)) = self.analysis_tasks.lock().await.remove(uri) {
            task.abort();
        }
    }

    async fn cancel_all_diagnostics(&self) {
        let tasks = std::mem::take(&mut *self.analysis_tasks.lock().await);
        for (_, task) in tasks.into_values() {
            task.abort();
        }
    }

    async fn run_diagnostics(&self, uri: Url, version: i32) {
        sleep(Duration::from_millis(250)).await;
        let Some(doc) = self.document(&uri) else {
            return;
        };
        if doc.version != version || !Self::supported(&doc) {
            return;
        }
        let mut diagnostics = lexical_diagnostics(&doc.text);
        let overlays = self.open_file_overlays();
        if let Some(path) = self.document_path(&uri) {
            let compiler_config = self
                .compiler_config
                .read()
                .map(|config| config.clone())
                .unwrap_or_default();
            let compiler = compiler_diagnostics(
                &path,
                &doc.text,
                overlays,
                compiler_config,
                self.compiler_semaphore.clone(),
                self.compiler_results.clone(),
            )
            .await;
            if !diagnostics
                .iter()
                .any(|item| item.code == Some(NumberOrString::String("holes".into())))
            {
                if self
                    .document(&uri)
                    .is_none_or(|current| current.version != version)
                {
                    return;
                }
                let root_path = normalize_path(&path);
                let mut imported = HashMap::<Url, Vec<Diagnostic>>::new();
                for (source_path, item) in compiler {
                    if normalize_path(&source_path) == root_path {
                        diagnostics.push(item);
                    } else if let Ok(source_uri) = Url::from_file_path(&source_path)
                        && self.document(&source_uri).is_none()
                    {
                        imported.entry(source_uri).or_default().push(item);
                    }
                }
                self.replace_import_diagnostics(uri.clone(), version, imported)
                    .await;
            }
        }
        if self
            .document(&uri)
            .is_some_and(|current| current.version == version)
        {
            self.client
                .publish_diagnostics(uri, diagnostics, Some(version))
                .await;
        }
    }
    async fn replace_import_diagnostics(
        &self,
        root_uri: Url,
        version: i32,
        by_uri: HashMap<Url, Vec<Diagnostic>>,
    ) {
        let affected = {
            let Ok(mut snapshots) = self.imported_diagnostics.write() else {
                return;
            };
            if snapshots
                .get(&root_uri)
                .is_some_and(|snapshot| snapshot.version > version)
            {
                return;
            }
            let mut affected = HashSet::new();
            if let Some(previous) = snapshots.get(&root_uri) {
                affected.extend(previous.by_uri.keys().cloned());
            }
            affected.extend(by_uri.keys().cloned());
            snapshots.insert(root_uri, ImportedDiagnostics { version, by_uri });
            affected
        };
        self.publish_import_diagnostics(affected).await;
    }

    async fn remove_import_diagnostics(&self, root_uri: &Url) {
        let affected = {
            let Ok(mut snapshots) = self.imported_diagnostics.write() else {
                return;
            };
            let Some(previous) = snapshots.remove(root_uri) else {
                return;
            };
            previous.by_uri.into_keys().collect()
        };
        self.publish_import_diagnostics(affected).await;
    }

    async fn publish_import_diagnostics(&self, affected: HashSet<Url>) {
        let _publish = self.imported_diagnostic_publish.lock().await;
        for uri in affected {
            if self.document(&uri).is_some() {
                continue;
            }
            let diagnostics = self
                .imported_diagnostics
                .read()
                .map(|snapshots| {
                    let mut combined = Vec::new();
                    for snapshot in snapshots.values() {
                        if let Some(items) = snapshot.by_uri.get(&uri) {
                            for item in items {
                                if !combined.iter().any(|existing: &Diagnostic| {
                                    existing.range == item.range
                                        && existing.message == item.message
                                        && existing.code == item.code
                                }) {
                                    combined.push(item.clone());
                                }
                            }
                        }
                    }
                    combined
                })
                .unwrap_or_default();
            if self.document(&uri).is_none() {
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .await;
            }
        }
    }
}

fn named_document_symbol(source: &str, name: &str) -> Option<DocumentSymbol> {
    for symbol in analysis::document_symbols(source) {
        if symbol.name == name {
            return Some(symbol);
        }
        if let Some(child) = symbol
            .children
            .as_ref()
            .and_then(|children| children.iter().find(|child| child.name == name))
        {
            return Some(child.clone());
        }
    }
    None
}

fn function_symbols(source: &str) -> Vec<DocumentSymbol> {
    analysis::document_symbols(source)
        .into_iter()
        .filter(|symbol| symbol.kind == SymbolKind::FUNCTION)
        .collect()
}

fn call_sites(source: &str, name: &str) -> Vec<Range> {
    let declaration = analysis::declaration_range(source, name);
    analysis::identifier_ranges(source, name)
        .into_iter()
        .filter(|range| Some(*range) != declaration)
        .filter(|range| {
            let mut offset = offset_at(source, range.end);
            while offset < source.len() && source.as_bytes()[offset].is_ascii_whitespace() {
                offset += 1;
            }
            source.as_bytes().get(offset) == Some(&b'(')
        })
        .collect()
}

fn range_contains_range(outer: Range, inner: Range) -> bool {
    (outer.start.line, outer.start.character) <= (inner.start.line, inner.start.character)
        && (inner.end.line, inner.end.character) <= (outer.end.line, outer.end.character)
}

fn call_hierarchy_item(uri: Url, symbol: DocumentSymbol) -> CallHierarchyItem {
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

fn type_hierarchy_symbol(
    source: &str,
    name: &str,
) -> Option<(DocumentSymbol, Option<DocumentSymbol>)> {
    for symbol in analysis::document_symbols(source) {
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

fn type_hierarchy_item(uri: Url, symbol: DocumentSymbol) -> TypeHierarchyItem {
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

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                ..Default::default()
            },
        )),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
            first_trigger_character: "\n".into(),
            more_trigger_character: None,
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into()]),
            ..Default::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: Some(vec![")".into()]),
            ..Default::default()
        }),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        inlay_hint_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        document_link_provider: Some(DocumentLinkOptions {
            work_done_progress_options: WorkDoneProgressOptions::default(),
            resolve_provider: Some(false),
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions::default(),
                legend: SemanticTokensLegend {
                    token_types: vec![
                        SemanticTokenType::NAMESPACE,
                        SemanticTokenType::TYPE,
                        SemanticTokenType::CLASS,
                        SemanticTokenType::ENUM,
                        SemanticTokenType::INTERFACE,
                        SemanticTokenType::STRUCT,
                        SemanticTokenType::TYPE_PARAMETER,
                        SemanticTokenType::PARAMETER,
                        SemanticTokenType::VARIABLE,
                        SemanticTokenType::PROPERTY,
                        SemanticTokenType::ENUM_MEMBER,
                        SemanticTokenType::EVENT,
                        SemanticTokenType::FUNCTION,
                        SemanticTokenType::METHOD,
                        SemanticTokenType::MACRO,
                        SemanticTokenType::KEYWORD,
                        SemanticTokenType::MODIFIER,
                        SemanticTokenType::COMMENT,
                        SemanticTokenType::STRING,
                        SemanticTokenType::NUMBER,
                        SemanticTokenType::OPERATOR,
                    ],
                    token_modifiers: Vec::new(),
                },
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Bool(true)),
            },
        )),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        ..Default::default()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut roots: Vec<PathBuf> = params
            .workspace_folders
            .unwrap_or_default()
            .into_iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .map(|path| normalize_path(&path))
            .collect();
        if roots.is_empty()
            && let Some(uri) = params.root_uri
            && let Ok(path) = uri.to_file_path()
        {
            roots.push(normalize_path(&path));
        }
        if let Ok(mut workspace_roots) = self.workspace_roots.write() {
            *workspace_roots = roots;
        }
        let supports_watch_registration = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watchers| watchers.dynamic_registration)
            .unwrap_or(false);
        let supports_type_hierarchy_registration = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.type_hierarchy.as_ref())
            .and_then(|hierarchy| hierarchy.dynamic_registration)
            .unwrap_or(false);
        if let Ok(mut registration) = self.type_hierarchy_registration.write() {
            *registration = supports_type_hierarchy_registration;
        }
        if let Ok(mut watch_registration) = self.watch_registration.write() {
            *watch_registration = supports_watch_registration;
        }
        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: "bend2-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }
    async fn initialized(&self, _: InitializedParams) {
        let mut registrations = Vec::new();
        if self.watch_registration.read().is_ok_and(|enabled| *enabled) {
            registrations.push(Registration {
                id: "bend2-lsp-bend-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(serde_json::json!({
                    "watchers":[{"globPattern":"**/*.bend","kind":7}]
                })),
            });
        }
        if self
            .type_hierarchy_registration
            .read()
            .is_ok_and(|enabled| *enabled)
        {
            registrations.push(Registration {
                id: "bend2-lsp-type-hierarchy".into(),
                method: "textDocument/prepareTypeHierarchy".into(),
                register_options: Some(serde_json::json!({
                    "documentSelector":[
                        {"language":"bend"},
                        {"language":"bend2"}
                    ]
                })),
            });
        }
        if !registrations.is_empty() {
            let _ = self.client.register_capability(registrations).await;
        }
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        if let Ok(mut roots) = self.workspace_roots.write() {
            for folder in params.event.removed {
                if let Ok(path) = folder.uri.to_file_path() {
                    let path = normalize_path(&path);
                    roots.retain(|root| *root != path);
                }
            }
            for folder in params.event.added {
                if let Ok(path) = folder.uri.to_file_path() {
                    let path = normalize_path(&path);
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
            }
        }
        for document in self.workspace_documents() {
            self.schedule_diagnostics(document.uri, document.version)
                .await;
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let (config, warnings) = CompilerConfig::from_settings(&params.settings);
        if let Ok(mut current) = self.compiler_config.write() {
            *current = config;
        }
        {
            let _load = self.base_module_load.lock().await;
            if let Ok(mut module) = self.base_module.write() {
                *module = None;
            }
            if let Ok(mut attempted) = self.base_module_attempted.write() {
                *attempted = false;
            }
        }
        for warning in warnings {
            self.client.log_message(MessageType::WARNING, warning).await;
        }
        let documents = self.workspace_documents();
        if documents.iter().any(|document| {
            analysis::imports(&document.text)
                .iter()
                .any(|import| import.path == "Base")
        }) {
            self.load_prelude_module().await;
        }
        for document in documents {
            self.schedule_diagnostics(document.uri, document.version)
                .await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        self.cancel_all_diagnostics().await;
        Ok(())
    }
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let item = params.text_document;
        let needs_prelude = analysis::imports(&item.text)
            .iter()
            .any(|import| import.path == "Base");
        let doc = Document {
            uri: item.uri.clone(),
            language_id: item.language_id,
            version: item.version,
            text: item.text,
        };
        if let Ok(mut docs) = self.documents.write() {
            docs.insert(item.uri.clone(), doc);
        }
        if needs_prelude {
            self.load_prelude_module().await;
        }
        let dependents = self.dependent_documents(&item.uri);
        self.schedule_diagnostics(item.uri, item.version).await;
        for document in dependents {
            self.schedule_diagnostics(document.uri, document.version)
                .await;
        }
    }
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let mut changed = false;
        if let Ok(mut docs) = self.documents.write()
            && let Some(doc) = docs.get_mut(&uri)
            && version > doc.version
        {
            for change in params.content_changes {
                if let Some(range) = change.range {
                    let start = offset_at(&doc.text, range.start);
                    let end = offset_at(&doc.text, range.end);
                    if start <= end {
                        doc.text.replace_range(start..end, &change.text);
                    }
                } else {
                    doc.text = change.text;
                }
            }
            doc.version = version;
            changed = true;
        }
        if !changed {
            return;
        }
        if self.document(&uri).is_some_and(|document| {
            analysis::imports(&document.text)
                .iter()
                .any(|import| import.path == "Base")
        }) {
            self.load_prelude_module().await;
        }
        let dependents = self.dependent_documents(&uri);
        self.schedule_diagnostics(uri, version).await;
        for document in dependents {
            self.schedule_diagnostics(document.uri, document.version)
                .await;
        }
    }
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut affected = HashMap::<Url, i32>::new();
        for event in params.changes {
            let uri = event.uri;
            if let Some(document) = self.document(&uri) {
                affected.insert(document.uri, document.version);
            }
            for document in self.dependent_documents(&uri) {
                affected.insert(document.uri, document.version);
            }
        }
        for (uri, version) in affected {
            self.schedule_diagnostics(uri, version).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.cancel_diagnostics(&uri).await;
        if let Ok(mut docs) = self.documents.write() {
            docs.remove(&uri);
        }
        self.remove_import_diagnostics(&uri).await;
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), None)
            .await;
        for document in self.dependent_documents(&uri) {
            self.schedule_diagnostics(document.uri, document.version)
                .await;
        }
    }
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let start = offset_at(&doc.text, Position::new(params.range.start.line, 0));
        let end = offset_at(&doc.text, params.range.end);
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
            range: Range::new(position_at(&doc.text, start), position_at(&doc.text, end)),
            new_text,
        }]))
    }
    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
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
        let offset = offset_at(&doc.text, td.position);
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
        let previous_code = previous[previous_indent..].trim_end();
        let unit = if params.options.insert_spaces {
            " ".repeat(params.options.tab_size as usize)
        } else {
            "\t".to_owned()
        };
        let desired = if previous_code.ends_with(':') {
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
                position_at(&doc.text, line_start),
                position_at(&doc.text, line_start + current_indent),
            ),
            new_text: desired,
        }]))
    }
    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
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
                end: position_at(&doc.text, doc.text.len()),
            },
            new_text: formatted,
        }]))
    }
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = offset_at(&doc.text, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        if cursor_in_comment_or_string(&doc.text[line_start..offset]) {
            return Ok(Some(CompletionResponse::Array(Vec::new())));
        }
        let prefix_start = doc.text[line_start..offset]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_ascii_alphanumeric() || *character == '_')
            .last()
            .map_or(offset, |(index, _)| line_start + index);
        let prefix = &doc.text[prefix_start..offset];
        let bytes = doc.text.as_bytes();
        if prefix_start > 0 && bytes[prefix_start - 1] == b'.' {
            let alias_end = prefix_start - 1;
            let mut alias_start = alias_end;
            while alias_start > 0
                && (bytes[alias_start - 1].is_ascii_alphanumeric()
                    || bytes[alias_start - 1] == b'_')
            {
                alias_start -= 1;
            }
            let alias = &doc.text[alias_start..alias_end];
            if let Some((_, source)) = self.module_document(&doc, alias) {
                return Ok(Some(CompletionResponse::Array(
                    analysis::module_completion_items(&source, prefix),
                )));
            }
            if let Some(module) = self.prelude_module(&doc) {
                return Ok(Some(CompletionResponse::Array(
                    analysis::qualified_completion_items(&module.source, alias, prefix),
                )));
            }
        }
        let mut items = analysis::completion_items(&doc.text, prefix);
        if !prefix.is_empty() {
            let mut labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
            if let Some(module) = self.prelude_module(&doc) {
                items.extend(
                    analysis::completion_items(&module.source, prefix)
                        .into_iter()
                        .filter(|item| labels.insert(item.label.clone())),
                );
            }
            for import in analysis::imports(&doc.text) {
                if let Some(alias) = import.alias.filter(|alias| alias.starts_with(prefix))
                    && labels.insert(alias.clone())
                {
                    let mut item = CompletionItem::new_simple(
                        alias,
                        format!("Imported module {}", import.path),
                    );
                    item.kind = Some(CompletionItemKind::MODULE);
                    items.push(item);
                }
            }
        }
        Ok(Some(CompletionResponse::Array(items)))
    }
    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(analysis::signature_help(
            &doc.text,
            offset_at(&doc.text, td.position),
        ))
    }
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = offset_at(&doc.text, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        if cursor_in_comment_or_string(&doc.text[line_start..offset]) {
            return Ok(None);
        }
        let token = token_at(&doc.text, offset);
        let Some((uri, source, name)) = self.hierarchy_source(&doc, &token) else {
            return Ok(None);
        };
        let Some(symbol) = function_symbols(&source)
            .into_iter()
            .find(|symbol| symbol.name == name)
        else {
            return Ok(None);
        };
        Ok(Some(vec![call_hierarchy_item(uri, symbol)]))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let target = params.item;
        let target_name = target.name.rsplit('.').next().unwrap_or(&target.name);
        let mut calls = Vec::<(CallHierarchyItem, Vec<Range>)>::new();
        for caller_doc in self.hierarchy_documents() {
            if !Self::supported(&caller_doc) {
                continue;
            }
            for caller in function_symbols(&caller_doc.text) {
                for range in call_sites(&caller_doc.text, target_name) {
                    let token =
                        token_at(&caller_doc.text, offset_at(&caller_doc.text, range.start));
                    let Some((uri, _, name)) = self.hierarchy_source(&caller_doc, &token) else {
                        continue;
                    };
                    if uri != target.uri || name != target.name {
                        continue;
                    }
                    if let Some((_, ranges)) = calls
                        .iter_mut()
                        .find(|(item, _)| item.uri == caller_doc.uri && item.name == caller.name)
                    {
                        ranges.push(range);
                    } else {
                        calls.push((
                            call_hierarchy_item(caller_doc.uri.clone(), caller.clone()),
                            vec![range],
                        ));
                    }
                }
            }
        }
        Ok(Some(
            calls
                .into_iter()
                .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
                .collect(),
        ))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let caller_item = params.item;
        let Some(caller_doc) = self.hierarchy_document(&caller_item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let Some(caller) = function_symbols(&caller_doc.text)
            .into_iter()
            .find(|symbol| symbol.name == caller_item.name)
        else {
            return Ok(Some(Vec::new()));
        };
        let mut calls = Vec::<(CallHierarchyItem, Vec<Range>)>::new();
        for target_doc in self.hierarchy_documents() {
            if !Self::supported(&target_doc) {
                continue;
            }
            for target in function_symbols(&target_doc.text) {
                let member = target.name.rsplit('.').next().unwrap_or(&target.name);
                for range in call_sites(&caller_doc.text, member) {
                    if !range_contains_range(caller.range, range) {
                        continue;
                    }
                    let token =
                        token_at(&caller_doc.text, offset_at(&caller_doc.text, range.start));
                    let Some((uri, _, name)) = self.hierarchy_source(&caller_doc, &token) else {
                        continue;
                    };
                    if uri != target_doc.uri || name != target.name {
                        continue;
                    }
                    if let Some((_, ranges)) = calls
                        .iter_mut()
                        .find(|(item, _)| item.uri == target_doc.uri && item.name == target.name)
                    {
                        ranges.push(range);
                    } else {
                        calls.push((
                            call_hierarchy_item(target_doc.uri.clone(), target.clone()),
                            vec![range],
                        ));
                    }
                }
            }
        }
        Ok(Some(
            calls
                .into_iter()
                .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
                .collect(),
        ))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = offset_at(&doc.text, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        if cursor_in_comment_or_string(&doc.text[line_start..offset]) {
            return Ok(None);
        }
        let token = token_at(&doc.text, offset);
        let Some((uri, source, name)) = self.hierarchy_source(&doc, &token) else {
            return Ok(None);
        };
        let Some((symbol, _)) = type_hierarchy_symbol(&source, &name) else {
            return Ok(None);
        };
        Ok(Some(vec![type_hierarchy_item(uri, symbol)]))
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let parent = analysis::document_symbols(&doc.text)
            .into_iter()
            .find(|symbol| {
                symbol.kind == SymbolKind::STRUCT
                    && symbol.children.as_ref().is_some_and(|children| {
                        children.iter().any(|child| child.name == item.name)
                    })
            });
        Ok(Some(
            parent
                .map(|symbol| vec![type_hierarchy_item(doc.uri, symbol)])
                .unwrap_or_default(),
        ))
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let children = analysis::document_symbols(&doc.text)
            .into_iter()
            .find(|symbol| symbol.kind == SymbolKind::STRUCT && symbol.name == item.name)
            .and_then(|symbol| symbol.children)
            .unwrap_or_default()
            .into_iter()
            .map(|symbol| type_hierarchy_item(doc.uri.clone(), symbol))
            .collect();
        Ok(Some(children))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let mut lenses = Vec::new();
        for symbol in analysis::document_symbols(&doc.text) {
            if symbol.kind != SymbolKind::FUNCTION
                || symbol
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.starts_with("law "))
            {
                continue;
            }
            let declaration = analysis::declaration_range(&doc.text, &symbol.name);
            let locations: Vec<Location> = analysis::identifier_ranges(&doc.text, &symbol.name)
                .into_iter()
                .filter(|range| Some(*range) != declaration)
                .map(|range| Location {
                    uri: doc.uri.clone(),
                    range,
                })
                .collect();
            if locations.is_empty() {
                continue;
            }
            let count = locations.len();
            lenses.push(CodeLens {
                range: symbol.selection_range,
                command: Some(tower_lsp::lsp_types::Command {
                    title: format!("{count} reference{}", if count == 1 { "" } else { "s" }),
                    command: "editor.action.showReferences".into(),
                    arguments: Some(vec![
                        serde_json::json!(doc.uri.as_str()),
                        serde_json::json!(symbol.selection_range.start),
                        serde_json::json!(locations),
                    ]),
                }),
                data: None,
            });
        }
        Ok(Some(lenses))
    }
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(analysis::inlay_hints(&doc.text, params.range)))
    }
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.document(&uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        if params
            .context
            .only
            .as_ref()
            .is_some_and(|only| !only.iter().any(|kind| kind == &CodeActionKind::QUICKFIX))
        {
            return Ok(Some(Vec::new()));
        }
        let requested_range = params.range;
        let mut actions = Vec::new();
        for diagnostic in params.context.diagnostics {
            if (diagnostic.range.end.line, diagnostic.range.end.character)
                < (requested_range.start.line, requested_range.start.character)
                || (requested_range.end.line, requested_range.end.character)
                    < (
                        diagnostic.range.start.line,
                        diagnostic.range.start.character,
                    )
            {
                continue;
            }
            if diagnostic.code != Some(NumberOrString::String("parsing".into())) {
                continue;
            }
            let Some(open) = diagnostic
                .message
                .strip_prefix("Unclosed '")
                .and_then(|message| message.chars().next())
            else {
                continue;
            };
            let Some(close) = (match open {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                _ => None,
            }) else {
                continue;
            };
            let insertion = position_at(
                &doc.text,
                code_end_offset(&doc.text, offset_at(&doc.text, diagnostic.range.start)),
            );
            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range::new(insertion, insertion),
                        new_text: close.to_string(),
                    }],
                )])),
                ..Default::default()
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Insert '{close}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic]),
                edit: Some(edit),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }
        Ok(Some(actions))
    }
    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(analysis::folding_ranges(&doc.text)))
    }
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(
            params
                .positions
                .into_iter()
                .map(|position| analysis::selection_range(&doc.text, position))
                .collect(),
        ))
    }
    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let Some(source_path) = self.document_path(&doc.uri) else {
            return Ok(Some(Vec::new()));
        };
        let mut links = Vec::new();
        for import in analysis::imports(&doc.text) {
            if !import.path.ends_with(".bend") {
                continue;
            }
            let Some(target) = import_target_path(&source_path, &import.path) else {
                continue;
            };
            let Ok(target_uri) = Url::from_file_path(&target) else {
                continue;
            };
            if target.exists() || self.document(&target_uri).is_some() {
                links.push(DocumentLink {
                    range: import.path_range,
                    target: Some(target_uri),
                    tooltip: Some(format!("Open {}", import.path)),
                    data: None,
                });
            }
        }
        Ok(Some(links))
    }
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: analysis::semantic_tokens(&doc.text),
        })))
    }
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(DocumentSymbolResponse::Nested(
            analysis::document_symbols(&doc.text),
        )))
    }
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let token = token_at(&doc.text, offset_at(&doc.text, td.position));
        let imported = token.split_once('.').and_then(|(alias, member)| {
            self.module_document(&doc, alias)
                .and_then(|(_, source)| analysis::declaration_hover(&source, member))
        });
        let prelude = self
            .prelude_declaration(&doc, &token)
            .and_then(|module| analysis::declaration_hover(&module.source, &token));
        let Some(value) = analysis::declaration_hover(&doc.text, &token)
            .or(imported)
            .or(prelude)
            .or_else(|| static_hover(&token))
        else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = offset_at(&doc.text, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let line_end = doc.text[offset..]
            .find('\n')
            .map_or(doc.text.len(), |i| offset + i);
        let line = &doc.text[line_start..line_end];
        let column = offset - line_start;
        if cursor_in_comment_or_string(&line[..column]) {
            return Ok(None);
        }
        let import = analysis::imports(&doc.text).into_iter().find(|import| {
            position_in_range(td.position, import.path_range)
                || import
                    .alias_range
                    .is_some_and(|range| position_in_range(td.position, range))
        });
        if let Some(import) = import {
            let target_uri = if import.path == "Base" {
                self.prelude_module(&doc).map(|module| module.uri)
            } else {
                self.document_path(&doc.uri)
                    .and_then(|source| import_target_path(&source, &import.path))
                    .and_then(|target| Url::from_file_path(target).ok())
            };
            if let Some(uri) = target_uri
                && (uri.to_file_path().is_ok_and(|path| path.exists())
                    || self.document(&uri).is_some())
            {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range: Range::default(),
                })));
            }
            return Ok(None);
        }
        let token = token_at(&doc.text, offset);
        if let Some(range) = analysis::declaration_range(&doc.text, &token) {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: doc.uri,
                range,
            })));
        }
        if let Some(module) = self.prelude_declaration(&doc, &token)
            && let Some(range) = analysis::declaration_range(&module.source, &token)
        {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: module.uri,
                range,
            })));
        }
        let Some((alias, name)) = token.split_once('.') else {
            return Ok(None);
        };
        let Some((target_uri, target_text)) = self.module_document(&doc, alias) else {
            return Ok(None);
        };
        let Some(range) = analysis::declaration_range(&target_text, name) else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        })))
    }
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let documents = self.indexed_documents();
        let prelude_document = documents
            .iter()
            .find(|document| {
                analysis::imports(&document.text)
                    .iter()
                    .any(|import| import.path == "Base")
            })
            .cloned();
        let includes_base = prelude_document.is_some();
        if let Some(document) = prelude_document {
            self.ensure_prelude_module(&document).await;
        }
        let mut symbols = Vec::new();
        for doc in documents {
            if Self::supported(&doc) {
                symbols.extend(analysis::workspace_symbols(
                    &doc.text,
                    &doc.uri,
                    &params.query,
                ));
            }
        }
        if includes_base
            && let Some(module) = self
                .base_module
                .read()
                .ok()
                .and_then(|module| module.clone())
        {
            symbols.extend(analysis::workspace_symbols(
                &module.source,
                &module.uri,
                &params.query,
            ));
        }
        Ok(Some(symbols))
    }
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let token = token_at(&doc.text, offset_at(&doc.text, td.position));
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        Ok(Some(self.symbol_references(
            &target_uri,
            &name,
            params.context.include_declaration,
        )))
    }
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let token = token_at(&doc.text, offset_at(&doc.text, td.position));
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        Ok(Some(
            self.symbol_references(&target_uri, &name, true)
                .into_iter()
                .filter(|location| location.uri == doc.uri)
                .map(|location| DocumentHighlight {
                    range: location.range,
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect(),
        ))
    }
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let token = token_at(&doc.text, offset_at(&doc.text, td.position));
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        let mut changes = HashMap::<Url, Vec<TextEdit>>::new();
        for location in self.symbol_references(&target_uri, &name, true) {
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: params.new_name.clone(),
            });
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let token = token_at(&doc.text, offset_at(&doc.text, td.position));
        let imported = token.split_once('.').and_then(|(alias, member)| {
            self.module_document(&doc, alias)
                .map(|(uri, source)| (uri, source, member.to_owned()))
        });
        let local = analysis::type_declaration_range(&doc.text, &token)
            .map(|_| (doc.uri.clone(), Arc::from(doc.text.clone()), token.clone()));
        let prelude = self
            .prelude_module(&doc)
            .filter(|module| analysis::type_declaration_range(&module.source, &token).is_some())
            .map(|module| (module.uri, module.source, token.clone()));
        let (target_uri, target_text, name) = imported
            .or(local)
            .or(prelude)
            .unwrap_or_else(|| (doc.uri.clone(), Arc::from(doc.text.clone()), token.clone()));
        let range = analysis::type_declaration_range(&target_text, &name).or_else(|| {
            (target_uri == doc.uri)
                .then(|| analysis::parameter_type_declaration_range(&target_text, &name))
                .flatten()
        });
        let Some(range) = range else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range,
        })))
    }
}
fn cursor_in_comment_or_string(prefix: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in prefix.chars() {
        if let Some(current) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == current {
                quote = None;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == '#' {
            return true;
        }
    }
    quote.is_some()
}

struct ExitAwareService<S> {
    inner: S,
    exit: tokio::sync::watch::Sender<bool>,
}

impl<S> Service<Request> for ExitAwareService<S>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        if request.method() == "exit" {
            self.exit.send_replace(true);
        }
        self.inner.call(request)
    }
}

struct StdinChannel {
    receiver: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
}

impl StdinChannel {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel(4);
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut chunk = vec![0; 8192];
                match stdin.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        chunk.truncate(read);
                        if sender.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            receiver,
            current: Vec::new(),
            offset: 0,
        }
    }
}

impl AsyncRead for StdinChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        loop {
            if this.offset < this.current.len() {
                let length = buffer.remaining().min(this.current.len() - this.offset);
                if length == 0 {
                    return Poll::Ready(Ok(()));
                }
                buffer.put_slice(&this.current[this.offset..this.offset + length]);
                this.offset += length;
                if this.offset == this.current.len() {
                    this.current.clear();
                    this.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match this.receiver.poll_recv(context) {
                Poll::Ready(Some(chunk)) => {
                    this.current = chunk;
                    this.offset = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let stdout = tokio::io::stdout();
    let analysis_tasks = Arc::new(AsyncMutex::new(HashMap::new()));
    let server_tasks = analysis_tasks.clone();
    let (service, socket) = LspService::new(move |client| Backend {
        client,
        documents: Arc::new(RwLock::new(HashMap::new())),
        imported_diagnostics: Arc::new(RwLock::new(HashMap::new())),
        imported_diagnostic_publish: Arc::new(AsyncMutex::new(())),
        workspace_roots: Arc::new(RwLock::new(Vec::new())),
        watch_registration: Arc::new(RwLock::new(false)),
        type_hierarchy_registration: Arc::new(RwLock::new(false)),
        compiler_config: Arc::new(RwLock::new(CompilerConfig::default())),
        base_module: Arc::new(RwLock::new(None)),
        base_module_attempted: Arc::new(RwLock::new(false)),
        base_module_load: Arc::new(AsyncMutex::new(())),
        analysis_tasks: server_tasks.clone(),
        next_analysis_id: Arc::new(AtomicU64::new(1)),
        compiler_semaphore: Arc::new(Semaphore::new(4)),
        compiler_results: Arc::new(RwLock::new(HashMap::new())),
    });
    let (exit, mut exit_rx) = tokio::sync::watch::channel(false);
    let service = ExitAwareService {
        inner: service,
        exit,
    };
    tokio::select! {
        () = Server::new(StdinChannel::new(), stdout, socket).serve(service) => {}
        _ = exit_rx.changed() => {}
    }
    cancel_task_handles(&analysis_tasks).await;
}

async fn cancel_task_handles(tasks: &AsyncMutex<HashMap<Url, (u64, JoinHandle<()>)>>) {
    let tasks = std::mem::take(&mut *tasks.lock().await);
    for (_, task) in tasks.into_values() {
        task.abort();
    }
}

fn offset_at(text: &str, pos: Position) -> usize {
    let mut offset = 0;
    for (line_no, line) in text.split_inclusive('\n').enumerate() {
        if line_no == pos.line as usize {
            let line = line.trim_end_matches('\n').trim_end_matches('\r');
            let mut byte = 0;
            let mut units = 0;
            for ch in line.chars() {
                if units + ch.len_utf16() > pos.character as usize {
                    break;
                }
                units += ch.len_utf16();
                byte += ch.len_utf8();
            }
            return (offset + byte).min(text.len());
        }
        offset += line.len();
    }
    text.len()
}
fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}
fn position_at(text: &str, offset: usize) -> Position {
    let offset = floor_char_boundary(text, offset);
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count();
    let col = prefix
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count();
    Position::new(
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(col).unwrap_or(u32::MAX),
    )
}

fn position_in_range(position: Position, range: Range) -> bool {
    let after_start = position.line > range.start.line
        || position.line == range.start.line && position.character >= range.start.character;
    let before_end = position.line < range.end.line
        || position.line == range.end.line && position.character < range.end.character;
    after_start && before_end
}
fn lexical_diagnostics(text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut opens: Vec<(char, usize)> = Vec::new();
    let mut quote = None;
    let mut quote_at = 0;
    let mut escaped = false;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            } else if c == '\n' {
                out.push(diag(
                    text,
                    quote_at,
                    at,
                    format!(
                        "Unterminated {} literal.",
                        if q == '"' { "string" } else { "character" }
                    ),
                    "parsing",
                ));
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            quote_at = at;
            i += 1;
            continue;
        }
        let tail = &text[at..];
        if tail.starts_with("?TODO")
            && !tail[5..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            out.push(diag(text, at, at + 5, "Unresolved hole '?TODO'.", "holes"));
        }
        if "([{".contains(c) {
            opens.push((c, at));
        } else if ")]}".contains(c) {
            let expected = match c {
                ')' => '(',
                ']' => '[',
                _ => '{',
            };
            if opens.last().is_some_and(|o| o.0 == expected) {
                opens.pop();
            } else {
                out.push(diag(
                    text,
                    at,
                    at + c.len_utf8(),
                    format!("Unmatched '{c}'."),
                    "parsing",
                ));
            }
        }
        i += 1;
    }
    if let Some(q) = quote {
        out.push(diag(
            text,
            quote_at,
            text.len(),
            format!(
                "Unterminated {} literal.",
                if q == '"' { "string" } else { "character" }
            ),
            "parsing",
        ));
    }
    for (c, at) in opens {
        out.push(diag(
            text,
            at,
            at + c.len_utf8(),
            format!("Unclosed '{c}'."),
            "parsing",
        ));
    }
    out
}
fn diag(
    text: &str,
    start: usize,
    end: usize,
    message: impl Into<String>,
    code: &str,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: position_at(text, start),
            end: position_at(text, end),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(code.into())),
        source: Some("bend2".into()),
        message: message.into(),
        ..Default::default()
    }
}

fn code_end_offset(text: &str, start: usize) -> usize {
    let mut offset = floor_char_boundary(text, start.min(text.len()));
    let mut last = offset;
    let mut quote = None;
    let mut escaped = false;
    while offset < text.len() {
        let character = text[offset..].chars().next().unwrap_or_default();
        let next = offset + character.len_utf8();
        if let Some(current) = quote {
            offset = next;
            last = offset;
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == current {
                quote = None;
            }
        } else if character == '#' {
            offset = text[offset..]
                .find('\n')
                .map_or(text.len(), |index| offset + index);
        } else if character == '"' || character == '\'' {
            quote = Some(character);
            offset = next;
            last = offset;
        } else {
            offset = next;
            if !character.is_whitespace() {
                last = offset;
            }
        }
    }
    last
}
async fn compiler_diagnostics(
    path: &Path,
    text: &str,
    mut overlays: HashMap<PathBuf, String>,
    config: CompilerConfig,
    semaphore: Arc<Semaphore>,
    cache: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
) -> Vec<(PathBuf, Diagnostic)> {
    let root = normalize_path(path);
    overlays.insert(root.clone(), text.to_owned());
    let Ok(permit) = semaphore.acquire_owned().await else {
        return Vec::new();
    };
    let stage_root = root.clone();
    let staged = tokio::task::spawn_blocking(move || {
        let staging = tempfile::tempdir().ok()?;
        let (entry, sources) = stage_import_graph(&stage_root, &overlays, staging.path())?;
        Some((permit, staging, entry, sources))
    })
    .await;
    let Ok(Some((_permit, _staging, entry, sources))) = staged else {
        return Vec::new();
    };
    let mut snapshot_sources = sources.clone();
    snapshot_sources.sort_by(|left, right| left.0.cmp(&right.0));
    let cacheable_sources = cacheable_sources(&sources)
        && sources
            .iter()
            .map(|(_, source)| source.len())
            .fold(0usize, usize::saturating_add)
            <= 1_048_576;
    let compiler_stamp = cacheable_sources
        .then(|| compiler_stamp(&config.path))
        .flatten();
    let cacheable = cacheable_sources && compiler_stamp.is_some();
    let snapshot = CompilerSnapshot {
        compiler_path: config.path.clone(),
        compiler_arguments: config.arguments.clone(),
        compiler_stamp,
        sources: snapshot_sources,
    };
    if cacheable
        && let Ok(results) = cache.read()
        && let Some(result) = results.get(&root)
        && result.snapshot == snapshot
    {
        return result.diagnostics.clone();
    }
    let output = match Command::new(&config.path)
        .kill_on_drop(true)
        .args(&config.arguments)
        .arg(entry)
        .arg("--check-only")
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let message = format!("Unable to run Bend compiler '{}': {error}", config.path);
            return vec![(root, diag(text, 0, 0, message, "compiler-unavailable"))];
        }
    };
    let diagnostics = compiler_output_diagnostics(&root, text, &sources, &output);
    if cacheable && let Ok(mut results) = cache.write() {
        if !results.contains_key(&root)
            && results.len() >= 32
            && let Some(evicted) = results.keys().next().cloned()
        {
            results.remove(&evicted);
        }
        results.insert(
            root,
            CachedCompilerResult {
                snapshot,
                diagnostics: diagnostics.clone(),
            },
        );
    }
    diagnostics
}
fn compiler_output_diagnostics(
    root: &Path,
    text: &str,
    sources: &[(PathBuf, String)],
    output: &std::process::Output,
) -> Vec<(PathBuf, Diagnostic)> {
    if output.status.success() {
        return Vec::new();
    }
    let raw = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let raw = String::from_utf8_lossy(raw);
    let detail = raw.split("\nLocation:").next().unwrap_or(&raw).trim();
    let message = detail.strip_prefix("Error:\n").unwrap_or(detail).trim();
    if message.is_empty() {
        return Vec::new();
    }
    let marker = raw.lines().find_map(|line| {
        let (line_number, code) = line.split_once(">|")?;
        Some((
            line_number.trim().parse::<usize>().ok()?,
            code.strip_prefix(' ')
                .unwrap_or(code)
                .trim_end_matches('\r'),
        ))
    });
    let location = marker.and_then(|(line, excerpt)| {
        let mut matches = sources.iter().filter_map(|(source_path, source_text)| {
            let (start, end) = source_line_range(source_text, line.saturating_sub(1))?;
            (source_text.get(start..end)? == excerpt)
                .then(|| (source_path.clone(), source_text.clone(), start, end))
        });
        let unique = matches.next()?;
        matches.next().is_none().then_some(unique)
    });
    let (source_path, source_text, start, end) =
        location.unwrap_or_else(|| (root.to_path_buf(), text.to_owned(), 0, 0));
    let lowered = message.to_ascii_lowercase();
    let code = if ["import", "file", "hash", "namespace", "cycle", "bend_hub"]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        "imports"
    } else if lowered.contains("expected : 'def'") {
        "parsing"
    } else {
        "checking"
    };
    vec![(source_path, diag(&source_text, start, end, message, code))]
}

fn cacheable_sources(sources: &[(PathBuf, String)]) -> bool {
    sources.iter().all(|(source_path, source)| {
        analysis::imports(source).into_iter().all(|import| {
            let path = import.path;
            if path == "Base" || Path::new(&path).is_absolute() || is_hub_import_path(&path) {
                return false;
            }
            source_path
                .parent()
                .is_some_and(|parent| normalize_path(&parent.join(path)).is_file())
        })
    })
}

fn compiler_stamp(path: &str) -> Option<(u64, SystemTime)> {
    let executable = Path::new(path);
    let metadata = if executable.is_absolute() || executable.components().count() > 1 {
        std::fs::metadata(executable).ok()?
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .find_map(|directory| std::fs::metadata(directory.join(executable)).ok())?
    };
    Some((metadata.len(), metadata.modified().ok()?))
}

fn source_line_range(text: &str, wanted: usize) -> Option<(usize, usize)> {
    let mut offset = 0;
    for (line, source) in text.split_inclusive('\n').enumerate() {
        let end = offset + source.trim_end_matches(['\r', '\n']).len();
        if line == wanted {
            return Some((offset, end));
        }
        offset += source.len();
    }
    (wanted == 0 && text.is_empty()).then_some((0, 0))
}
fn normalize_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
fn stage_path(root: &Path, staging: &Path) -> PathBuf {
    let mut result = staging.to_path_buf();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => {
                result.push(prefix.as_os_str().to_string_lossy().replace(':', "_"));
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => result.push("_parent"),
            Component::Normal(part) => result.push(part),
        }
    }
    result
}
fn hub_library_path() -> Option<PathBuf> {
    let path = std::env::var_os("BEND_LIB")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".bend").join("lib"))
        })?;
    Some(if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    })
}

fn is_hash_package(package: &str) -> bool {
    package.strip_prefix("0x").is_some_and(|hash| {
        hash.len() == 32
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn is_named_hub_package(package: &str) -> bool {
    let Some((name, version)) = package.split_once('@') else {
        return false;
    };
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return false;
    }
    let components: Vec<&str> = version.split('.').collect();
    components.len() == 4
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (*component == "0" || !component.starts_with('0'))
        })
}

fn is_hub_import_path(imported: &str) -> bool {
    if imported.starts_with("./") || imported.starts_with("../") {
        return false;
    }
    let Some((package, _)) = imported.split_once('/') else {
        return false;
    };
    let hash = package.strip_prefix("0x").unwrap_or("");
    is_hash_package(package)
        || !hash.is_empty()
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || package.contains('@')
}

fn hub_cached_path(imported: &str) -> Option<PathBuf> {
    let library = hub_library_path()?;
    let (package, relative) = imported.split_once('/')?;
    if relative.is_empty() || !relative.ends_with(".bend") {
        return None;
    }
    let package = if is_hash_package(package) {
        package.to_owned()
    } else if is_named_hub_package(package) {
        let hash = std::fs::read_to_string(library.join("names").join(package))
            .ok()?
            .trim()
            .to_owned();
        if !is_hash_package(&hash) {
            return None;
        }
        hash
    } else {
        return None;
    };
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(
        relative
            .components()
            .fold(library.join(package), |path, component| {
                path.join(component.as_os_str())
            }),
    )
}

fn import_target_path(source: &Path, imported: &str) -> Option<PathBuf> {
    if imported == "Base" || Path::new(imported).is_absolute() {
        return None;
    }
    if let Some(path) = hub_cached_path(imported) {
        return Some(path);
    }
    if is_hub_import_path(imported) {
        return None;
    }
    Some(normalize_path(&source.parent()?.join(imported)))
}
fn stage_import_graph(
    root: &Path,
    overlays: &HashMap<PathBuf, String>,
    staging: &Path,
) -> Option<(PathBuf, Vec<(PathBuf, String)>)> {
    let mut seen = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(path) = pending.pop() {
        let path = normalize_path(&path);
        if !seen.insert(path.clone()) {
            continue;
        }
        let text = if let Some(text) = overlays.get(&path) {
            text.clone()
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            text
        } else if path == root {
            return None;
        } else {
            continue;
        };
        sources.push((path.clone(), text.clone()));
        let staged = stage_path(&path, staging);
        std::fs::create_dir_all(staged.parent()?).ok()?;
        std::fs::write(&staged, &text).ok()?;
        for import in analysis::imports(&text) {
            let imported = import.path;
            if imported == "Base"
                || Path::new(&imported).is_absolute()
                || is_hub_import_path(&imported)
            {
                continue;
            }
            pending.push(path.parent()?.join(imported));
        }
    }
    Some((stage_path(root, staging), sources))
}
fn static_hover(token: &str) -> Option<String> {
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
fn token_at(text: &str, offset: usize) -> String {
    for token in ["{==}", "<&>", "->", "=>", "==", "!=", "&0", "&1", "&2"] {
        if let Some(start) = text[..floor_char_boundary(text, offset)].rfind(token)
            && start + token.len() >= offset
        {
            return token.into();
        }
    }
    let bytes = text.as_bytes();
    let offset = floor_char_boundary(text, offset);
    if bytes
        .get(offset)
        .is_some_and(|byte| matches!(byte, b'%' | b'!'))
        || offset > 0 && matches!(bytes[offset - 1], b'%' | b'!')
    {
        let operator = if bytes
            .get(offset)
            .is_some_and(|byte| matches!(byte, b'%' | b'!'))
        {
            bytes[offset]
        } else {
            bytes[offset - 1]
        };
        return char::from(operator).to_string();
    }
    let mut start = offset;
    let mut end = start;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || b"_./".contains(&bytes[start - 1]))
    {
        start -= 1;
    }
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || b"_./".contains(&bytes[end]))
    {
        end += 1;
    }
    text[start..end].into()
}
fn imported_paths(source: &Path, text: &str) -> Vec<PathBuf> {
    analysis::imports(text)
        .into_iter()
        .filter_map(|import| {
            let path = import.path;
            if path == "Base" || Path::new(&path).is_absolute() {
                return None;
            }
            if let Some(path) = hub_cached_path(&path) {
                return Some(path);
            }
            if is_hub_import_path(&path) {
                return None;
            }
            Some(normalize_path(&source.parent()?.join(path)))
        })
        .collect()
}

#[derive(Clone)]
struct Token {
    text: String,
    kind: u8,
    gap: bool,
}
fn format_bend(source: &str, tab_size: usize, spaces: bool) -> String {
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let final_eol = source.ends_with('\n');
    let mut raw: Vec<&str> = source.split('\n').collect();
    if final_eol {
        raw.pop();
    }
    let mut lines = Vec::new();
    for line in raw {
        let indent_len = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let indent = &line[..indent_len];
        let body = &line[indent_len..];
        let Some((code, comment)) = split_comment(body) else {
            return source.into();
        };
        lines.push((
            indent.to_string(),
            code.trim().to_string(),
            comment.to_string(),
        ));
    }
    let mut stack = vec![0usize];
    let mut depths = Vec::new();
    for (indent, code, comment) in &lines {
        if code.is_empty() && comment.is_empty() {
            depths.push(0);
            continue;
        }
        let width = indent
            .chars()
            .fold(0, |n, c| if c == '\t' { n + (8 - n % 8) } else { n + 1 });
        while stack.len() > 1 && width < stack.last().copied().unwrap_or_default() {
            stack.pop();
        }
        let previous_width = stack.last().copied().unwrap_or_default();
        if width > previous_width {
            stack.push(width);
        } else if width != previous_width
            && let Some(last_width) = stack.last_mut()
        {
            *last_width = width;
        }
        depths.push(stack.len() - 1);
    }
    let formatted: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, (_, code, comment))| {
            if code.is_empty() && comment.is_empty() {
                return String::new();
            }
            let prefix = if spaces {
                " ".repeat(depths[i] * tab_size.max(1))
            } else {
                "\t".repeat(depths[i])
            };
            let tokens = lex(code);
            let code = format_tokens(&tokens);
            if code.is_empty() {
                prefix + comment
            } else if comment.is_empty() {
                prefix + &code
            } else {
                prefix + &code + "  " + comment
            }
        })
        .collect();
    let result = formatted.join(eol) + if final_eol { eol } else { "" };
    if fingerprint(source) != fingerprint(&result) {
        return source.into();
    }
    result
}
fn fingerprint(source: &str) -> Option<Vec<(usize, Vec<String>)>> {
    let mut raw: Vec<&str> = source.split('\n').collect();
    if source.ends_with('\n') {
        raw.pop();
    }
    let mut stack = vec![0usize];
    let mut result = Vec::with_capacity(raw.len());
    for line in raw {
        let indent_len = line
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        let indent = &line[..indent_len];
        let (code, comment) = split_comment(&line[indent_len..])?;
        let code = code.trim();
        let tokens = lex(code);
        let width = indent
            .chars()
            .fold(0, |n, c| if c == '\t' { n + (8 - n % 8) } else { n + 1 });
        while stack.len() > 1 && width < *stack.last()? {
            stack.pop();
        }
        if width > *stack.last()? {
            stack.push(width);
        } else if width != *stack.last()? {
            *stack.last_mut()? = width;
        }
        let depth = if tokens.is_empty() && comment.is_empty() {
            0
        } else {
            stack.len() - 1
        };
        result.push((depth, tokens.into_iter().map(|token| token.text).collect()));
    }
    Some(result)
}
fn split_comment(s: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '#' {
            return Some((&s[..i], &s[i..]));
        }
    }
    quote.is_none().then_some((s, ""))
}
fn lex_import_path(
    code: &str,
    chars: &[(usize, char)],
    mut index: usize,
) -> Option<(Token, usize)> {
    while index < chars.len() && chars[index].1.is_whitespace() {
        index += 1;
    }
    let start = chars.get(index)?.0;
    while index < chars.len() && !chars[index].1.is_whitespace() {
        index += 1;
    }
    let end = chars.get(index).map_or(code.len(), |(offset, _)| *offset);
    Some((
        Token {
            text: code[start..end].into(),
            kind: 0,
            gap: true,
        },
        index,
    ))
}

fn lex_number(chars: &[(usize, char)], mut index: usize) -> usize {
    index += 1;
    while index < chars.len() && chars[index].1.is_ascii_digit() {
        index += 1;
    }
    if index + 1 < chars.len() && chars[index].1 == '.' && chars[index + 1].1.is_ascii_digit() {
        index += 1;
        while index < chars.len() && chars[index].1.is_ascii_digit() {
            index += 1;
        }
    }
    if index < chars.len() && matches!(chars[index].1, 'e' | 'E') {
        let next = index + 1;
        let exponent = next < chars.len() && chars[next].1.is_ascii_digit()
            || next + 1 < chars.len()
                && matches!(chars[next].1, '+' | '-')
                && chars[next + 1].1.is_ascii_digit();
        if exponent {
            index += 1;
            if index < chars.len() && matches!(chars[index].1, '+' | '-') {
                index += 1;
            }
            while index < chars.len() && chars[index].1.is_ascii_digit() {
                index += 1;
            }
        }
    }
    if index < chars.len() && chars[index].1 == 'n' {
        index += 1;
    }
    index
}

fn lex(code: &str) -> Vec<Token> {
    const MULTI: [&str; 17] = [
        "<&>", ".|.", ".^.", ".&.", "==", "!=", "->", "<-", "=>", "&&", "||", "++", "<>", "<=",
        ">=", "<<", ">>",
    ];
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let mut i = 0;
    let mut had_gap = false;
    let mut tokens = Vec::new();
    while i < chars.len() {
        if tokens
            .last()
            .is_some_and(|token: &Token| token.text == "import")
        {
            let Some((token, next_index)) = lex_import_path(code, &chars, i) else {
                break;
            };
            tokens.push(token);
            i = next_index;
            had_gap = false;
            continue;
        }
        let c = chars[i].1;
        if c.is_whitespace() {
            had_gap = true;
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let mut kind = 3;
        if c == '"' || c == '\'' {
            kind = 2;
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let next = chars[i].1;
                i += 1;
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == c {
                    break;
                }
            }
            if i == chars.len() && chars.last().is_some_and(|item| item.1 != c) {
                return Vec::new();
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            kind = 0;
            i += 1;
            while i < chars.len()
                && (chars[i].1.is_ascii_alphanumeric() || "_.".contains(chars[i].1))
            {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            kind = 1;
            i = lex_number(&chars, i);
        } else if c == '?'
            && chars
                .get(i + 1)
                .is_some_and(|(_, ch)| ch.is_ascii_alphabetic() || *ch == '_')
        {
            kind = 0;
            i += 1;
            while i < chars.len() && (chars[i].1.is_ascii_alphanumeric() || chars[i].1 == '_') {
                i += 1;
            }
        } else {
            let rest = &code[start..];
            let multi = MULTI.iter().find(|operator| rest.starts_with(**operator));
            i += multi.map_or(1, |operator| operator.chars().count());
        }
        let end = if i < chars.len() {
            chars[i].0
        } else {
            code.len()
        };
        tokens.push(Token {
            text: code[start..end].into(),
            kind,
            gap: had_gap,
        });
        had_gap = false;
    }
    tokens
}
fn format_tokens(tokens: &[Token]) -> String {
    let mut out = String::new();
    for i in 0..tokens.len() {
        if i > 0 && needs_space(tokens, i) {
            out.push(' ');
        }
        out.push_str(&tokens[i].text);
    }
    out
}
const BINARY: &[&str] = &[
    "=", "==", "!=", "->", "<-", "=>", "+", "-", "*", "/", "%", "&&", "||", "++", "<>", "<&>",
    "<=", ">=", "<<", ">>", ".|.", ".^.", ".&.", "&", "|",
];
fn unary(tokens: &[Token], index: usize) -> bool {
    let token = tokens[index].text.as_str();
    if !["+", "-", "~", "?", "@", "&", "%"].contains(&token) {
        return false;
    }
    let previous = index.checked_sub(1).map(|i| tokens[i].text.as_str());
    let Some(next) = tokens.get(index + 1) else {
        return false;
    };
    let at_prefix = previous.is_none_or(|p| {
        ["(", "{", "[", "<", ",", ":", "=", "for", "case", "~"].contains(&p) || BINARY.contains(&p)
    });
    match token {
        "~" | "%" => at_prefix,
        "+" | "-" => next.kind == 0 && (!next.gap || (index > 0 && at_prefix)),
        _ => (next.kind == 0 || next.kind == 1) && at_prefix,
    }
}
fn keep_angle_gap(left: &Token, right: &Token) -> Option<bool> {
    let angles = ["<", ">", "<<", ">>"];
    (angles.contains(&left.text.as_str()) || angles.contains(&right.text.as_str()))
        .then_some(right.gap)
}
fn needs_space(t: &[Token], i: usize) -> bool {
    let left = &t[i - 1];
    let right = &t[i];
    let a = left.text.as_str();
    let b = right.text.as_str();
    if [")", "]", "}", ",", ";"].contains(&b) {
        return false;
    }
    if b == ":" {
        return false;
    }
    if ["(", "[", "{"].contains(&a) {
        return false;
    }
    if a == "," {
        return true;
    }
    if b == "!" && (left.kind == 0 || [")", "]", "}"].contains(&a)) {
        return false;
    }
    if a == "!" && b == "(" {
        return false;
    }
    if b == "(" || b == "[" {
        let suffix = (left.kind == 0
            && ![
                "return", "match", "case", "do", "for", "exs", "where", "is", "import", "def",
                "type", "law",
            ]
            .contains(&a))
            || left.kind == 1
            || left.kind == 2
            || [")", "]", "}", ">", ">>"].contains(&a);
        return !suffix;
    }
    if b == "{"
        && ((left.kind == 0 && !["return", "case"].contains(&a)) || [">", ">>", "}"].contains(&a))
    {
        return false;
    }
    if a == "\\" && b == "{" {
        return false;
    }
    if a == "." || b == "." {
        return false;
    }
    if left.kind == 1 && left.text.ends_with('n') && ["+", "++"].contains(&b) {
        return right.gap;
    }
    if ["+", "++"].contains(&a)
        && t.get(i.wrapping_sub(2))
            .is_some_and(|token| token.kind == 1 && token.text.ends_with('n'))
    {
        return right.gap;
    }
    if unary(t, i - 1) {
        return false;
    }
    if unary(t, i) {
        return !["(", "[", "{", "<"].contains(&a);
    }
    if let Some(gap) = keep_angle_gap(left, right) {
        return gap;
    }
    if BINARY.contains(&a) || BINARY.contains(&b) || a == ":" {
        return true;
    }
    true
}
