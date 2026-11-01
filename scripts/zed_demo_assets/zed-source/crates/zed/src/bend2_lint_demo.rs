// SPDX-License-Identifier: Apache-2.0
//! Windowless capture driver for the real bend-lint feature.
//!
//! Runs a genuine Zed `Workspace` on the headless Metal renderer, attaches an
//! isolated `RealFs` project and the real `bend2-lsp` child process configured
//! by that project's `.zed/settings.json`, and hands the workspace to the scene
//! recorder. Nothing here synthesizes diagnostics, editor pixels, or language
//! server responses.

use anyhow::{Context as _, Result, bail};
use assets::Assets;
use async_trait::async_trait;
use collections::HashMap;
use futures::future::FutureExt as _;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, HeadlessAppContext, WindowHandle, px, size,
};
use language::{
    LanguageConfig, LanguageLoader, LanguageName, LanguageQueries, LanguageRegistry,
    LanguageServerName, LoadedLanguage, LspAdapter, LspAdapterDelegate, LspInstaller, QueryFile,
    QueryFileContents, Toolchain,
};
use lsp::{LanguageServerBinary, Uri};
use project::{LocalProjectFlags, Project};
use serde_json::{Value, json};
use settings::{LocalSettingsKind, LocalSettingsPath, ParseStatus, Settings as _, SettingsStore};
use std::collections::HashMap as StdHashMap;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use theme::{ActiveTheme as _, Appearance};
use util::rel_path::RelPath;
use workspace::{AppState, PaneSearchBarCallbacks, Workspace, WorkspaceStore};

#[path = "bend2_lint_demo_scenes.rs"]
mod scenes;

const SCENE_VALUES: [&str; 3] = [
    "clip1-unsaved-entry",
    "clip2-imported-type",
    "clip3-dangerous-action",
];

/// In-memory presentation settings for a clearly readable dark capture.
/// These never touch the personal settings file.
const CAPTURE_USER_SETTINGS: &str = r#"{
  "theme": "One Dark",
  "ui_font_size": 15.0,
  "buffer_font_size": 20.0,
  "buffer_font_family": "Lilex",
  "tab_size": 2,
  "autosave": "off",
  "format_on_save": "off",
  "diagnostics": {
    "inline": {
      "enabled": true,
      "update_debounce_ms": 0,
      "max_severity": "hint",
      "padding": 4,
      "min_column": 0
    }
  }
}"#;

/// Shared interface with `bend2_lint_demo_scenes`.
#[derive(Clone, Debug)]
pub struct DemoConfig {
    pub scene: String,
    pub project_root: PathBuf,
    pub output_dir: PathBuf,
    pub extension_root: PathBuf,
}

/// Monotonic basis for `observed_wall_ms` in frame records. It is forced once
/// at driver start so the reported values span the whole capture run.
static SCENE_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Values taken verbatim from the demo project's `.zed/settings.json`.
struct DemoLspConfig {
    binary_path: PathBuf,
    arguments: Vec<OsString>,
    env: StdHashMap<String, String>,
    workspace_configuration: Value,
}

impl DemoLspConfig {
    fn load(config: &DemoConfig) -> Result<Self> {
        let settings_path = config.project_root.join(".zed/settings.json");
        let settings_text = std::fs::read_to_string(&settings_path).with_context(|| {
            format!(
                "reading the demo project settings at {}",
                settings_path.display()
            )
        })?;
        let settings: Value = serde_json::from_str(&settings_text).with_context(|| {
            format!(
                "parsing the demo project settings at {}",
                settings_path.display()
            )
        })?;

        let lsp = settings
            .get("lsp")
            .and_then(|lsp| lsp.get("bend2"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no lsp.bend2 section; the driver only runs the fixture-configured server",
                    settings_path.display()
                )
            })?;

        let binary = lsp.get("binary").ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no lsp.bend2.binary; refusing to guess a language server",
                settings_path.display()
            )
        })?;
        let binary_path = binary
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no lsp.bend2.binary.path string",
                    settings_path.display()
                )
            })?;
        if !binary_path.is_absolute() {
            bail!(
                "lsp.bend2.binary.path must be absolute, got {}",
                binary_path.display()
            );
        }
        validate_executable(&binary_path, "lsp.bend2.binary.path")?;

        let mut arguments = Vec::new();
        if let Some(values) = binary.get("arguments") {
            let values = values.as_array().ok_or_else(|| {
                anyhow::anyhow!("lsp.bend2.binary.arguments must be an array of strings")
            })?;
            for value in values {
                let value = value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("lsp.bend2.binary.arguments must contain only strings")
                })?;
                arguments.push(OsString::from(value));
            }
        }

        let mut env = StdHashMap::new();
        if let Some(values) = binary.get("env") {
            let values = values
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("lsp.bend2.binary.env must be an object"))?;
            for (key, value) in values {
                let value = value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("lsp.bend2.binary.env.{} must be a string", key)
                })?;
                env.insert(key.clone(), value.to_owned());
            }
        }

        let workspace_configuration = lsp.get("settings").cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no lsp.bend2.settings; the real linter/compiler consent must come from the fixture",
                settings_path.display()
            )
        })?;
        validate_workspace_configuration(&workspace_configuration)?;

        Ok(Self {
            binary_path,
            arguments,
            env,
            workspace_configuration,
        })
    }
}

/// Native adapter for the real `bend2-lsp` binary. Binary discovery is limited
/// to the fixture settings; every download path fails explicitly.
struct DemoBend2LspAdapter {
    config: Arc<DemoLspConfig>,
}

impl DemoBend2LspAdapter {
    fn new(config: Arc<DemoLspConfig>) -> Self {
        Self { config }
    }
}

#[async_trait(?Send)]
impl LspAdapter for DemoBend2LspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName::new_static("bend2")
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        let mut ids = HashMap::default();
        ids.insert(LanguageName::new("Bend 2"), "bend2".to_owned());
        ids
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _toolchain: Option<Toolchain>,
        _requested_uri: Option<Uri>,
        _cx: &mut AsyncApp,
    ) -> Result<Value> {
        Ok(self.config.workspace_configuration.clone())
    }
}

impl LspInstaller for DemoBend2LspAdapter {
    type BinaryVersion = ();

    async fn fetch_latest_server_version(
        &self,
        _delegate: &Arc<dyn LspAdapterDelegate>,
        _pre_release: bool,
        _cx: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        bail!(
            "bend2-lsp downloads are disabled: the capture driver runs only the real binary from the demo project's .zed/settings.json"
        )
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        _container_dir: PathBuf,
        _delegate: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async {
            bail!(
                "bend2-lsp downloads are disabled: the capture driver runs only the real binary from the demo project's .zed/settings.json"
            )
        }
    }

    async fn cached_server_binary(
        &self,
        _container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        None
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _toolchain: Option<Toolchain>,
        _cx: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let mut env = HashMap::default();
        env.extend(delegate.shell_env().await);
        env.extend(
            self.config
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        Some(LanguageServerBinary {
            path: self.config.binary_path.clone(),
            arguments: self.config.arguments.clone(),
            env: Some(env),
        })
    }
}

fn validate_executable(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("{label} is unavailable at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("{label} is not executable: {}", path.display());
        }
    }
    Ok(())
}

fn validate_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("{label} is unavailable at {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(())
}

fn validate_workspace_configuration(settings: &Value) -> Result<()> {
    let lint = settings
        .get("bend")
        .and_then(|bend| bend.get("lint"))
        .ok_or_else(|| anyhow::anyhow!("lsp.bend2.settings has no bend.lint configuration"))?;
    match lint.get("enabled") {
        Some(Value::Bool(true)) => {}
        _ => bail!("lsp.bend2.settings must explicitly enable bend.lint"),
    }

    let lint_path = lint
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("bend.lint.path must be a string"))?;
    validate_executable(Path::new(lint_path), "bend.lint.path")?;

    let arguments = lint
        .get("arguments")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("bend.lint.arguments must be an array"))?;
    for (index, argument) in arguments.iter().enumerate() {
        let argument = argument
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("bend.lint.arguments[{index}] must be a string"))?;
        let argument_path = Path::new(argument);
        if argument_path.is_absolute() {
            validate_regular_file(argument_path, &format!("bend.lint.arguments[{index}]"))?;
        }
    }

    let compiler = settings
        .get("bend2-lsp")
        .and_then(|server| server.get("compilerPath"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("bend2-lsp.compilerPath must be a string"))?;
    validate_executable(Path::new(compiler), "bend2-lsp.compilerPath")?;

    Ok(())
}

fn load_bend_language(extension_root: &Path) -> Result<LanguageConfig> {
    let language_dir = extension_root.join("languages").join("bend2");
    let config_path = language_dir.join("config.toml");
    let config = LanguageConfig::load(&config_path).with_context(|| {
        format!(
            "loading the Bend 2 language config at {}",
            config_path.display()
        )
    })?;

    let grammar = config
        .grammar
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{} does not declare a grammar", config_path.display()))?;
    let grammar_path = extension_root
        .join("grammars")
        .join(format!("{grammar}.wasm"));
    validate_regular_file(&grammar_path, "the Bend 2 WASM grammar")?;

    for query in ["highlights.scm", "brackets.scm", "outline.scm"] {
        validate_regular_file(
            &language_dir.join(query),
            &format!("the Bend 2 {query} query"),
        )?;
    }

    Ok(config)
}

fn register_bend_language(
    languages: &LanguageRegistry,
    extension_root: &Path,
    config: LanguageConfig,
) -> Result<LanguageName> {
    let grammar = config
        .grammar
        .clone()
        .ok_or_else(|| anyhow::anyhow!("the Bend 2 language config has no grammar name"))?;
    let grammar_path = extension_root
        .join("grammars")
        .join(format!("{grammar}.wasm"));
    languages.register_wasm_grammars(vec![(grammar.clone(), grammar_path)]);

    let queries_dir = extension_root.join("languages").join("bend2");
    let config_for_loader = config.clone();
    let language_name = config.name.clone();
    let loader: LanguageLoader = Arc::new(move || {
        let config = config_for_loader.clone();
        let queries_dir = queries_dir.clone();
        async move {
            let mut files = Vec::with_capacity(3);
            for (query_file, file_name) in [
                (QueryFile::Highlights, "highlights.scm"),
                (QueryFile::Brackets, "brackets.scm"),
                (QueryFile::Outline, "outline.scm"),
            ] {
                let contents = std::fs::read_to_string(queries_dir.join(file_name))
                    .with_context(|| format!("reading the Bend 2 {file_name} query"))?;
                files.push(QueryFileContents::new(query_file, contents.into()));
            }
            Ok(LoadedLanguage {
                config,
                queries: LanguageQueries::from_files(files),
                context_provider: None,
                toolchain_provider: None,
                manifest_name: None,
            })
        }
        .boxed()
    });

    languages.register_language(
        language_name.clone(),
        Some(grammar),
        config.matcher.clone(),
        false,
        None,
        loader,
    );

    Ok(language_name)
}

/// Captures one real GPU-rendered frame of the workspace window.
///
/// Parking is never changed here; the scene owns the parking policy for real
/// LSP and filesystem I/O.
pub(crate) fn capture_frame(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
    id: &str,
    caption: &str,
    hold_seconds: f64,
    state: Value,
) -> Result<Value> {
    let file_name = frame_file_name(id)?;
    let path = config.output_dir.join(&file_name);

    cx.run_until_parked();
    cx.advance_clock(Duration::from_millis(16));
    cx.run_until_parked();
    let handle: AnyWindowHandle = window.into();
    let (logical_width, logical_height, device_scale) = cx
        .update_window(handle, |_, window, _| {
            window.refresh();
            let viewport = window.viewport_size();
            (
                f32::from(viewport.width),
                f32::from(viewport.height),
                window.scale_factor(),
            )
        })
        .context("refreshing the windowless workspace window")?;
    cx.run_until_parked();

    let screenshot = cx
        .capture_screenshot(handle)
        .context("capturing a real headless Metal frame")?;
    screenshot
        .save(&path)
        .with_context(|| format!("writing the captured frame to {}", path.display()))?;
    if !path.is_file() {
        bail!("captured frame was not written: {}", path.display());
    }

    let observed_wall_ms = SCENE_START.elapsed().as_millis() as u64;
    Ok(json!({
        "file": path.display().to_string(),
        "caption": caption,
        "hold_seconds": hold_seconds,
        "observed_wall_ms": observed_wall_ms,
        "state": state,
        "width": screenshot.width(),
        "height": screenshot.height(),
        "logical_width": logical_width,
        "logical_height": logical_height,
        "device_scale": device_scale,
    }))
}

/// Polls real application state against a real wall-clock deadline. Parking is
/// left enabled afterwards because the scene continues to depend on real I/O.
pub(crate) fn wait_for(
    cx: &mut HeadlessAppContext,
    timeout: Duration,
    mut predicate: impl FnMut(&mut App) -> bool,
) -> Result<()> {
    cx.allow_parking();
    let deadline = Instant::now() + timeout;
    loop {
        cx.run_until_parked();
        cx.advance_clock(Duration::from_millis(20));
        if cx.update(|app| predicate(app)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {:?} waiting for the observed editor/language-server state",
                timeout
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn frame_file_name(id: &str) -> Result<String> {
    if id.is_empty() || id == "." || id == ".." || id.contains('/') || id.contains('\\') {
        bail!("frame id must be a simple file name, got {id:?}");
    }
    Ok(format!("{id}.png"))
}

fn validate_report(report: &Value, output_dir: &Path) -> Result<()> {
    let frames = report
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("the scene report has no frames array"))?;
    if frames.is_empty() {
        bail!("the scene report has no captured frames");
    }
    for frame in frames {
        let file = frame
            .get("file")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("a scene frame has no file path"))?;
        let path = if Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            output_dir.join(file)
        };
        let metadata = std::fs::metadata(&path).with_context(|| {
            format!(
                "a scene frame references a missing image: {}",
                path.display()
            )
        })?;
        if metadata.len() == 0 {
            bail!("a scene frame image is empty: {}", path.display());
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("{label} is unavailable at {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn run(demo: DemoConfig) -> Result<()> {
    LazyLock::force(&SCENE_START);

    // Validate the external tool configuration and the real grammar before
    // paying for any GPU or application initialization.
    let lsp_config = Arc::new(DemoLspConfig::load(&demo)?);
    let language_config = load_bend_language(&demo.extension_root)?;
    let project_settings_text =
        std::fs::read_to_string(demo.project_root.join(".zed").join("settings.json"))
            .context("reading the isolated project settings for worktree-local application")?;

    if gpui_platform::current_headless_renderer().is_none() {
        bail!(
            "no headless Metal renderer is available; refusing to substitute pseudo-pixels for the real Zed UI"
        );
    }

    let platform = gpui_platform::current_platform(true);
    let text_system = platform.text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    let _platform = platform;

    cx.update(|cx| Assets.load_fonts(cx))
        .context("loading the embedded Zed fonts")?;

    let (app_state, project, language_name, languages) = cx
        .update(|cx| -> Result<(Arc<AppState>, gpui::Entity<Project>, LanguageName, Arc<LanguageRegistry>)> {
            settings::init(cx);
            SettingsStore::update(cx, |store, cx| {
                let result = store.set_user_settings(CAPTURE_USER_SETTINGS, cx);
                if let ParseStatus::Failed { error } = result.parse_status {
                    bail!("failed to apply the in-memory capture preferences: {error}");
                }
                Ok::<(), anyhow::Error>(())
            })?;

            theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
            if cx.theme().appearance() != Appearance::Dark {
                bail!("the capture theme did not resolve to a dark appearance");
            }
            let buffer_font_size =
                theme_settings::ThemeSettings::get_global(cx).buffer_font_size(cx);
            if buffer_font_size < px(18.0) {
                bail!(
                    "the buffer font size resolved to {buffer_font_size:?}, which is below the readable capture minimum"
                );
            }

            gpui_tokio::init(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);

            let clock = Arc::new(clock::FakeSystemClock::new());
            let http_client = http_client::FakeHttpClient::with_404_response();
            let client = client::Client::new(clock, http_client, cx);
            client::init(&client, cx);
            audio::init(cx);

            let session =
                cx.new(|cx| session::AppSession::new(session::Session::test(), cx));
            let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
            let workspace_store =
                cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
            let fs: Arc<dyn fs::Fs> =
                fs::RealFs::new(None, cx.background_executor().clone());
            <dyn fs::Fs>::set_global(fs.clone(), cx);
            let node_runtime = node_runtime::NodeRuntime::unavailable();

            let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
            languages.set_theme(cx.theme().clone());
            let language_name =
                register_bend_language(&languages, &demo.extension_root, language_config)?;
            languages.register_lsp_adapter(
                language_name.clone(),
                Arc::new(DemoBend2LspAdapter::new(lsp_config.clone())),
            );

            let app_state = Arc::new(workspace::AppState {
                client: client.clone(),
                fs: fs.clone(),
                languages: languages.clone(),
                user_store: user_store.clone(),
                workspace_store,
                node_runtime: node_runtime.clone(),
                build_window_options: |_, _| Default::default(),
                session,
            });
            workspace::AppState::set_global(app_state.clone(), cx);

            workspace::init(app_state.clone(), cx);
            command_palette::init(cx);
            editor::init(cx);
            diagnostics::init(cx);
            call::init(client.clone(), user_store.clone(), cx);
            title_bar::init(cx);
            project_panel::init(cx);
            outline_panel::init(cx);
            terminal_view::init(cx);
            image_viewer::init(cx);
            search::init(cx);
            lsp_locations::init(cx);
            cx.set_global(PaneSearchBarCallbacks {
                setup_search_bar: |languages, toolbar, window, cx| {
                    let search_bar =
                        cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
                    toolbar.update(cx, |toolbar, cx| {
                        toolbar.add_item(search_bar, window, cx);
                    });
                },
                wrap_div_with_search_actions: search::buffer_search::register_pane_search_actions,
            });

            let project = Project::local(
                client.clone(),
                node_runtime,
                user_store.clone(),
                languages.clone(),
                fs,
                None,
                LocalProjectFlags {
                    init_worktree_trust: false,
                    watch_global_configs: false,
                },
                cx,
            );

            Ok((app_state, project, language_name, languages))
        })?;

    cx.background_executor().allow_parking();
    let loaded_language = cx
        .foreground_executor()
        .block_test(languages.clone().language_for_name(language_name.as_ref()));
    cx.background_executor().forbid_parking();
    loaded_language.with_context(|| {
        format!(
            "loading the real Bend 2 language (wasm grammar + queries) from {}",
            demo.extension_root.display()
        )
    })?;

    if languages.lsp_adapters(&language_name).is_empty() {
        bail!(
            "the Bend 2 language has no registered language server adapter; refusing to render without the real bend2-lsp"
        );
    }

    let capture_size = if demo.scene == "clip2-imported-type" {
        size(px(1800.0), px(660.0))
    } else {
        size(px(1200.0), px(480.0))
    };
    let workspace_window = cx
        .open_window(capture_size, |window, cx| {
            cx.new(|cx| Workspace::new(None, project.clone(), app_state.clone(), window, cx))
        })
        .context("opening the windowless Workspace")?;
    cx.run_until_parked();

    let add_worktree_task = workspace_window
        .update(&mut cx, |workspace, _window, cx| {
            let project = workspace.project().clone();
            project.update(cx, |project, cx| {
                project.find_or_create_worktree(&demo.project_root, true, cx)
            })
        })
        .context("starting the isolated worktree")?;
    cx.background_executor().allow_parking();
    let worktree_result = cx.foreground_executor().block_test(add_worktree_task);
    cx.background_executor().forbid_parking();
    let (worktree, _) = worktree_result.context("creating the isolated worktree")?;

    let worktree_id = cx.update(|app| worktree.read(app).id());
    cx.update(|app| -> Result<()> {
        SettingsStore::update(app, |store, cx| {
            store.set_local_settings(
                worktree_id,
                LocalSettingsPath::InWorktree(RelPath::empty_arc()),
                LocalSettingsKind::Settings,
                Some(&project_settings_text),
                cx,
            )
        })
        .map_err(|error| {
            anyhow::anyhow!(
                "applying the isolated project .zed/settings.json to worktree {worktree_id:?}: {error:?}"
            )
        })
    })?;
    cx.run_until_parked();

    let (weak_workspace, async_window_cx) = workspace_window
        .update(&mut cx, |workspace, window, cx| {
            (workspace.weak_handle(), window.to_async(cx))
        })
        .context("preparing the project panel context")?;
    cx.background_executor().allow_parking();
    let panel = cx
        .foreground_executor()
        .block_test(project_panel::ProjectPanel::load(
            weak_workspace,
            async_window_cx,
        ))
        .context("loading the real project panel")?;
    cx.background_executor().forbid_parking();
    workspace_window
        .update(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx);
        })
        .context("adding the project panel")?;
    workspace_window
        .update(&mut cx, |workspace, window, cx| {
            workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
        })
        .context("opening the project panel")?;
    cx.run_until_parked();

    let report_result =
        scenes::record_scene(&mut cx, workspace_window, &demo).context("the scene recorder failed");
    cx.update(|app| app.shutdown());
    cx.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    let report = report_result?;
    validate_report(&report, &demo.output_dir)?;

    let report_path = demo.output_dir.join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("writing the scene report to {}", report_path.display()))?;
    println!(
        "bend2_lint_demo: report written to {}",
        report_path.display()
    );

    Ok(())
}

fn run_from_env() -> Result<()> {
    if std::env::var("ZED_STATELESS").as_deref() != Ok("1") {
        bail!(
            "ZED_STATELESS=1 must be set by the parent process; this driver never mutates its own environment"
        );
    }

    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let mut arguments = std::env::args_os().skip(1);
    let scene = arguments
        .next()
        .context("usage: bend2_lint_demo SCENE PROJECT_ROOT OUTPUT_DIR EXTENSION_ROOT")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("SCENE must be valid UTF-8"))?;
    if !SCENE_VALUES.contains(&scene.as_str()) {
        bail!(
            "unknown scene {scene:?}; expected one of {}",
            SCENE_VALUES.join(", ")
        );
    }
    let project_root = PathBuf::from(
        arguments
            .next()
            .context("missing PROJECT_ROOT positional argument")?,
    );
    let output_dir = PathBuf::from(
        arguments
            .next()
            .context("missing OUTPUT_DIR positional argument")?,
    );
    let extension_root = PathBuf::from(
        arguments
            .next()
            .context("missing EXTENSION_ROOT positional argument")?,
    );
    if arguments.next().is_some() {
        bail!("expected exactly four positional arguments");
    }

    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "creating the capture output directory at {}",
            output_dir.display()
        )
    })?;

    let demo = DemoConfig {
        scene,
        project_root: canonical_directory(&project_root, "PROJECT_ROOT")?,
        output_dir: canonical_directory(&output_dir, "OUTPUT_DIR")?,
        extension_root: canonical_directory(&extension_root, "EXTENSION_ROOT")?,
    };

    run(demo)
}

fn main() {
    if let Err(error) = run_from_env() {
        eprintln!("bend2_lint_demo: {error:#}");
        std::process::exit(1);
    }
}
