// SPDX-License-Identifier: Apache-2.0
//! Scripted walkthroughs of real, live `bend2-lsp` state inside a windowless
//! Zed `Workspace`.
//!
//! Every captured frame is rendered by the real GPUI/Metal offscreen renderer
//! from a real `Workspace`/`Editor`, and every diagnostic and code action in
//! those frames was produced by the real `bend2-lsp` process configured by the
//! demo project. Only the *clock* (GPUI's `TestDispatcher`) and the command
//! sequence are scripted: the module edits real editor buffers without saving,
//! polls real language-server publications with a wall-clock deadline, and
//! never synthesizes diagnostics, menus or pixels.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use editor::Editor;
use editor::actions::{ConfirmCodeAction, ToggleCodeActions};
use editor::code_context_menus::{CodeActionsItem, CodeContextMenu};
use gpui::{App, Entity, Focusable as _, HeadlessAppContext, WindowHandle};
use language::{Buffer, Point, ToPoint as _};
use project::LspAction;
use serde_json::{Value, json};
use workspace::{OpenOptions, Workspace};

use crate::DemoConfig;

#[path = "bend2_lint_demo_interactions.rs"]
mod interactions;

const UNNEEDED_PLUS: &str = "bend-lint-rules/language/unneeded_plus";
const FORMAT: &str = "bend-lint-rules/layout/format";
const UNUSED_RESULT: &str = "bend-lint-rules/performance/unused_result";

/// Opening a file includes real filesystem work on the worktree.
const OPEN_TIMEOUT: Duration = Duration::from_secs(90);
/// First analysis by the real server includes process start plus a compiler run.
const INITIAL_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(240);
/// Republishing after an unsaved buffer overlay.
const UPDATE_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(180);
/// Explicitly requested code actions are analysis-backed and can be slow.
const CODE_ACTION_TIMEOUT: Duration = Duration::from_secs(180);
/// Applying a versioned `WorkspaceEdit` through the editor.
const APPLY_TIMEOUT: Duration = Duration::from_secs(90);

const CLIP1_BASELINE_TEXT: &str =
    "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
const CLIP1_UNSAVED_TEXT: &str =
    "import Base\n\ndef f(x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
const CLIP3_CANONICAL_TEXT: &str =
    "import Base\n\ndef f(x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";

const CLIP2_MAIN_TEXT: &str = "import Base\nimport dep.bend as Dep\n\ndef f(n: Nat, acc: U32) -> U32:\n  match n:\n    case 0n:\n      acc\n    case 1n+m:\n      xs = Dep.build(3n, [])\n      f(m, acc)\n\ndef g(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
const CLIP2_DEP_BASELINE_TEXT: &str = "import Base\n\ndef build(n: Nat, acc: List<&2, U32>) -> List<&2, U32>:\n  match n:\n    case 0n:\n      acc\n    case 1n+m:\n      build(m, 1 <> acc)\n";
const CLIP2_DEP_UNSAVED_TEXT: &str =
    "import Base\n\ndef build(n: Nat, acc: List<&2, U32>) -> U32:\n  0\n";

/// Records one demo scene and returns its report.
///
/// `cx` was built with a real headless renderer and `window` hosts a real
/// `Workspace` with a local `Project`, a real worktree at
/// `config.project_root`, the Bend language and the native `bend2-lsp`
/// adapter. No file is open when this is called.
pub fn record_scene(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
) -> Result<Value> {
    // Real language-server and filesystem I/O complete on other threads. Parking
    // lets their wake-ups reach the deterministic test dispatcher instead of
    // being rejected as non-deterministic activity.
    cx.allow_parking();

    ensure!(
        config.project_root.is_dir(),
        "demo project root {} is not a directory",
        config.project_root.display()
    );

    interactions::record_scene(cx, window, config)
}

// ---------------------------------------------------------------------------
// Editor/buffer plumbing
// ---------------------------------------------------------------------------

fn demo_file(config: &DemoConfig, name: &str) -> Result<PathBuf> {
    let path = config.project_root.join(name);
    ensure!(
        path.starts_with(&config.project_root),
        "resolved demo file {} escapes the demo project root",
        path.display()
    );
    ensure!(
        path.is_file(),
        "demo fixture {} does not exist",
        path.display()
    );
    Ok(path)
}

fn ensure_fixture(path: &Path, expected: &str, label: &str) -> Result<()> {
    let actual = std::fs::read_to_string(path)
        .with_context(|| format!("reading demo fixture {}", path.display()))?;
    ensure!(
        actual == expected,
        "{label} no longer matches the prepared demo baseline; refusing to record a video from changed fixtures"
    );
    Ok(())
}

/// Opens `path` in the workspace, waiting for its editor to appear.
///
/// The open task runs on the foreground executor; this function only polls real
/// workspace state with a bounded wall-clock deadline.
fn open_file(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    path: &Path,
    split_right: bool,
) -> Result<Entity<Editor>> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("demo file {} has a non-UTF-8 name", path.display()))?
        .to_owned();
    let path = path.to_path_buf();

    let open_result: Rc<RefCell<Option<Result<()>>>> = Rc::new(RefCell::new(None));
    let open_task = if split_right {
        window.update(cx, |workspace, window, cx| {
            workspace.split_abs_path(path.clone(), true, window, cx)
        })?
    } else {
        window.update(cx, |workspace, window, cx| {
            workspace.open_abs_path(path.clone(), OpenOptions::default(), window, cx)
        })?
    };

    let open_result_slot = open_result.clone();
    cx.foreground_executor()
        .spawn(async move {
            let result = open_task.await.map(|_| ());
            *open_result_slot.borrow_mut() = Some(result);
        })
        .detach();

    let found: Rc<RefCell<Option<Entity<Editor>>>> = Rc::new(RefCell::new(None));
    let found_slot = found.clone();
    let result_slot = open_result.clone();
    let wait_file_name = file_name.clone();
    let wait_result = crate::wait_for(cx, OPEN_TIMEOUT, move |app| {
        if matches!(result_slot.borrow().as_ref(), Some(Err(_))) {
            return true;
        }
        let Ok(workspace) = window.read(app) else {
            return false;
        };
        match find_editor_for_file(workspace, app, &wait_file_name) {
            Some(editor) => {
                *found_slot.borrow_mut() = Some(editor);
                true
            }
            None => false,
        }
    });

    if let Some(Err(error)) = open_result.borrow_mut().take() {
        return Err(error).with_context(|| format!("opening {file_name} in the demo workspace"));
    }
    wait_result
        .with_context(|| format!("waiting for {file_name} to appear in the demo workspace"))?;
    found
        .borrow_mut()
        .take()
        .with_context(|| format!("{file_name} opened but no editor for it was found"))
}

fn find_editor_for_file(
    workspace: &Workspace,
    cx: &App,
    file_name: &str,
) -> Option<Entity<Editor>> {
    for pane in workspace.panes() {
        let pane = pane.read(cx);
        for item in pane.items() {
            let Some(editor) = item.act_as::<Editor>(cx) else {
                continue;
            };
            let multi_buffer = editor.read(cx).buffer().clone();
            let Some(buffer) = multi_buffer.read(cx).as_singleton() else {
                continue;
            };
            let matches = buffer
                .read(cx)
                .file()
                .map(|file| file.file_name(cx) == file_name)
                .unwrap_or(false);
            if matches {
                return Some(editor);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Real diagnostics state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DiagnosticInfo {
    code: String,
    source: Option<String>,
    severity: String,
    severity_number: Option<u8>,
    message: String,
    start: (u32, u32),
    end: (u32, u32),
}

impl DiagnosticInfo {
    fn json(&self) -> Value {
        json!({
            "code": self.code,
            "source": self.source,
            "severity": self.severity,
            "severity_number": self.severity_number,
            "message": self.message,
            "start": {"line": self.start.0 + 1, "column": self.start.1 + 1},
            "end": {"line": self.end.0 + 1, "column": self.end.1 + 1},
            "zero_based_start": {"line": self.start.0, "character": self.start.1},
            "zero_based_end": {"line": self.end.0, "character": self.end.1},
        })
    }
}

struct BufferState {
    path: PathBuf,
    text: String,
    dirty: bool,
    on_disk_text: Option<String>,
    diagnostics: Vec<DiagnosticInfo>,
}

impl BufferState {
    fn json(&self) -> Value {
        json!({
            "buffer_path": self.path.display().to_string(),
            "buffer_text": self.text,
            "dirty": self.dirty,
            "on_disk_text": self.on_disk_text,
            "disk_matches_buffer": self.on_disk_text.as_deref() == Some(self.text.as_str()),
            "diagnostic_codes": codes(&self.diagnostics),
            "diagnostics": self.diagnostics.iter().map(DiagnosticInfo::json).collect::<Vec<_>>(),
        })
    }
}

fn editor_buffer(editor: &Entity<Editor>, cx: &App) -> Result<Entity<Buffer>> {
    let multi_buffer = editor.read(cx).buffer().clone();
    multi_buffer
        .read(cx)
        .as_singleton()
        .context("the open editor is not backed by a single buffer")
}

fn read_buffer_diagnostics(buffer: &Entity<Buffer>, cx: &App) -> Vec<DiagnosticInfo> {
    let buffer = buffer.read(cx);
    let snapshot = buffer.snapshot();
    buffer
        .buffer_diagnostics(None)
        .into_iter()
        .map(|entry| {
            let diagnostic = &entry.diagnostic;
            let start = entry.range.start.to_point(&snapshot);
            let end = entry.range.end.to_point(&snapshot);
            // The code's type is only re-exported indirectly by `language`, so go
            // through its serialized form instead of naming the LSP type here.
            let code = match diagnostic.code.as_ref() {
                Some(code) => serde_json::to_value(code)
                    .map(|value| match value {
                        Value::String(code) => code,
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|_| format!("{code:?}")),
                None => String::new(),
            };
            let severity = format!("{:?}", diagnostic.severity);
            let severity_number = match severity.as_str() {
                "Error" => Some(1),
                "Warning" => Some(2),
                "Information" => Some(3),
                "Hint" => Some(4),
                _ => None,
            };
            DiagnosticInfo {
                code,
                source: diagnostic.source.clone(),
                severity,
                severity_number,
                message: diagnostic.message.to_string(),
                start: (start.row, start.column),
                end: (end.row, end.column),
            }
        })
        .collect()
}

fn read_editor_state(
    cx: &mut HeadlessAppContext,
    editor: &Entity<Editor>,
    project_root: &Path,
) -> Result<BufferState> {
    let buffer = cx.update(|app| editor_buffer(editor, app))?;
    let (path, text, dirty) = cx.update(|app| {
        let buffer = buffer.read(app);
        let text = buffer.snapshot().text();
        let dirty = buffer.is_dirty();
        let path = buffer
            .file()
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(app))
            .unwrap_or_default();
        (path, text, dirty)
    });
    let diagnostics = cx.update(|app| read_buffer_diagnostics(&buffer, app));
    ensure!(
        path_is_inside(&path, project_root),
        "refusing to read disk state for {} outside the demo project root {}",
        path.display(),
        project_root.display()
    );
    let on_disk_text = std::fs::read_to_string(&path).ok();
    Ok(BufferState {
        path,
        text,
        dirty,
        on_disk_text,
        diagnostics,
    })
}

/// Whether `path` resolves inside `root`, tolerating symlinked temp directories.
fn path_is_inside(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

fn wait_for_diagnostics(
    cx: &mut HeadlessAppContext,
    editor: &Entity<Editor>,
    config: &DemoConfig,
    description: &str,
    timeout: Duration,
    mut predicate: impl FnMut(&[DiagnosticInfo]) -> bool,
) -> Result<()> {
    let buffer = cx.update(|app| editor_buffer(editor, app))?;
    let wait_buffer = buffer.clone();
    let wait_result = crate::wait_for(cx, timeout, move |app| {
        predicate(&read_buffer_diagnostics(&wait_buffer, app))
    });
    let observed = read_editor_state(cx, editor, &config.project_root)?;
    wait_result.with_context(|| {
        format!(
            "waiting for {description}; codes observed at return: {:?}",
            codes(&observed.diagnostics)
        )
    })
}

fn has_code(diagnostics: &[DiagnosticInfo], code: &str) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code.ends_with(code))
}

fn codes(diagnostics: &[DiagnosticInfo]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect()
}

fn ensure_codes(
    diagnostics: &[DiagnosticInfo],
    present: &[&str],
    absent: &[&str],
    context: &str,
) -> Result<()> {
    for code in present {
        ensure!(
            has_code(diagnostics, code),
            "expected {code} in {context} but observed {:?}",
            codes(diagnostics)
        );
    }
    for code in absent {
        ensure!(
            !has_code(diagnostics, code),
            "did not expect {code} in {context} but observed {:?}",
            codes(diagnostics)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Real code action menu and invocation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MenuEntry {
    index: usize,
    label: String,
    kind: Option<String>,
}

impl MenuEntry {
    fn json(&self) -> Value {
        json!({
            "index": self.index,
            "label": self.label,
            "kind": self.kind,
        })
    }
}

/// Reads the real code action menu currently shown by the editor, if any.
fn code_action_menu(editor: &Entity<Editor>, cx: &App) -> Option<Vec<MenuEntry>> {
    let editor = editor.read(cx);
    let menu = editor.context_menu().borrow();
    let CodeContextMenu::CodeActions(menu) = menu.as_ref()? else {
        return None;
    };
    if !menu.visible() {
        return None;
    }
    let mut entries = Vec::new();
    let mut index = 0;
    while let Some(item) = menu.actions.get(index) {
        entries.push(MenuEntry {
            index,
            label: item.label(),
            kind: code_action_kind(&item),
        });
        index += 1;
        if index > 256 {
            break;
        }
    }
    Some(entries)
}

fn code_action_kind(item: &CodeActionsItem) -> Option<String> {
    let CodeActionsItem::CodeAction { action, .. } = item else {
        return None;
    };
    let LspAction::Action(lsp_action) = &action.lsp_action else {
        return None;
    };
    lsp_action
        .kind
        .as_ref()
        .map(|kind| kind.as_str().to_owned())
}

fn wait_for_code_action_menu(
    cx: &mut HeadlessAppContext,
    editor: &Entity<Editor>,
) -> Result<Vec<MenuEntry>> {
    let wait_editor = editor.clone();
    let wait_result = crate::wait_for(cx, CODE_ACTION_TIMEOUT, move |app| {
        code_action_menu(&wait_editor, app).is_some_and(|entries| !entries.is_empty())
    });
    let observed = cx.update(|app| code_action_menu(editor, app));
    wait_result.with_context(|| {
        format!("waiting for the real code action menu; menu observed at return: {observed:?}")
    })?;
    observed.context("the code action menu disappeared immediately after it was shown")
}

/// The server offers run/status controls next to its fixes, so this selects the
/// single entry that is actually the dangerous bend-lint format fix.
fn find_dangerous_format_action(entries: &[MenuEntry]) -> Result<MenuEntry> {
    let matches = entries
        .iter()
        .filter(|entry| {
            let title_matches = entry.label.contains("Dangerous bend-lint fix")
                && entry.label.contains("Format the file");
            let kind_matches = entry
                .kind
                .as_deref()
                .map_or(true, |kind| kind.contains("bend-lint.dangerous"));
            title_matches && kind_matches
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [matching] => Ok(matching.clone()),
        [] => bail!(
            "the code action menu did not offer the expected dangerous bend-lint format fix; entries: {:?}",
            entries.iter().map(MenuEntry::json).collect::<Vec<_>>()
        ),
        _ => bail!(
            "the code action menu offered several matching dangerous bend-lint format fixes: {:?}",
            matches.iter().map(MenuEntry::json).collect::<Vec<_>>()
        ),
    }
}
