// SPDX-License-Identifier: Apache-2.0
//! Procedurally recorded, genuinely interactive walkthroughs of the real
//! bend-lint feature inside a windowless Zed `Workspace`.
//!
//! Every pointer move, click, keystroke and menu selection below is dispatched
//! as a real GPUI input event: `Window::dispatch_event` for
//! `PlatformInput::MouseMove`/`MouseDown`/`MouseUp` and
//! `Window::dispatch_keystroke` for key presses and IME text insertion. The
//! module installs the minimum set of the official macOS editor key bindings
//! those inputs need, then verifies the observed buffer, caret, menu and
//! diagnostics after each step. No filmed edit is produced by
//! `Editor::edit`, `change_selections` or a direct action-handler call.

use super::*;

use editor::EditorStyle;
use editor::actions::{
    Backspace, ContextMenuNext, ContextMenuPrevious, Delete, MoveLeft, MoveRight,
    MoveToBeginningOfLine, MoveToEndOfLine, Newline, SelectLeft, SelectToEnd, SelectToEndOfLine,
    Tab,
};
use editor::display_map::ToDisplayPoint as _;
use gpui::{
    App, Entity, HeadlessAppContext, KeyBinding, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, WindowHandle,
};
use settings::Settings as _;

const POINTER_STEP_HOLD: f64 = 0.06;
const CLICK_HOLD: f64 = 0.24;
const KEY_HOLD: f64 = 0.65;
const TEXT_HOLD: f64 = 0.45;
const MENU_HOLD: f64 = 1.6;
const BASELINE_HOLD: f64 = 2.4;
const RESULT_HOLD: f64 = 3.0;
const POINTER_STEPS: usize = 6;
const MAX_CARET_NUDGES: usize = 16;

const CH1_BASELINE: &str = "Saved baseline: one real hint";
const CH1_REMOVE_PLUS: &str = "Remove the unnecessary +";
const CH1_INDENT: &str = "Re-indent the body";
const CH1_RESULT: &str = "Unsaved: a real format warning";

const CH2_PANES: &str = "Both panes: importer and dep";
const CH2_FOCUS_MAIN: &str = "Focus the importer pane";
const CH2_FOCUS_DEP: &str = "Focus the imported module";
const CH2_TYPE: &str = "Narrow the return type";
const CH2_BODY: &str = "Replace the recursive body";
const CH2_RESULT: &str = "The importer loses its semantic finding";

const CH3_BASELINE: &str = "Saved: one real hint";
const CH3_WARNING: &str = "Unsaved: a real warning";
const CH3_MENU: &str = "Open code actions with Cmd-.";
const CH3_SELECT: &str = "Select the dangerous fix";
const CH3_APPLY: &str = "Apply and verify";

pub(super) fn record_scene(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
) -> Result<Value> {
    cx.allow_parking();
    install_capture_keybindings(cx);

    match config.scene.as_str() {
        "clip1-unsaved-entry" => record_clip1(cx, window, config),
        "clip2-imported-type" => record_clip2(cx, window, config),
        "clip3-dangerous-action" => record_clip3(cx, window, config),
        other => bail!(
            "unknown scene {other:?}; expected clip1-unsaved-entry, clip2-imported-type or clip3-dangerous-action"
        ),
    }
}

// ---------------------------------------------------------------------------
// Official editor key bindings needed by the recorded inputs
// ---------------------------------------------------------------------------

/// Installs the minimum subset of Zed's official macOS editor keymap that the
/// recorded inputs use. Contexts and chord spellings mirror
/// `assets/keymaps/default-macos.json`; no personal keymap is read and no
/// action handler is called directly.
fn install_capture_keybindings(cx: &mut HeadlessAppContext) {
    cx.update(|cx| {
        cx.bind_keys([
            // "Editor" block.
            KeyBinding::new("backspace", Backspace, Some("Editor")),
            KeyBinding::new("delete", Delete, Some("Editor")),
            KeyBinding::new("tab", Tab, Some("Editor")),
            KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
            KeyBinding::new("left", MoveLeft, Some("Editor")),
            KeyBinding::new("right", MoveRight, Some("Editor")),
            KeyBinding::new("shift-left", SelectLeft, Some("Editor")),
            KeyBinding::new(
                "cmd-left",
                MoveToBeginningOfLine {
                    stop_at_soft_wraps: true,
                    stop_at_indent: true,
                },
                Some("Editor"),
            ),
            KeyBinding::new(
                "cmd-right",
                MoveToEndOfLine {
                    stop_at_soft_wraps: true,
                },
                Some("Editor"),
            ),
            KeyBinding::new(
                "cmd-shift-right",
                SelectToEndOfLine::default(),
                Some("Editor"),
            ),
            KeyBinding::new("cmd-shift-down", SelectToEnd, Some("Editor")),
            KeyBinding::new(
                "cmd-.",
                ToggleCodeActions {
                    deployed_from: None,
                    quick_launch: false,
                },
                Some("Editor"),
            ),
            // "Editor && mode == full" block.
            KeyBinding::new("enter", Newline, Some("Editor && mode == full")),
            // "Editor && showing_code_actions" block, which must win over the
            // generic newline binding while the menu is open.
            KeyBinding::new(
                "enter",
                ConfirmCodeAction { item_ix: None },
                Some("Editor && showing_code_actions"),
            ),
            // "Editor && (showing_code_actions || showing_completions)" block.
            KeyBinding::new(
                "down",
                ContextMenuNext,
                Some("Editor && (showing_code_actions || showing_completions)"),
            ),
            KeyBinding::new(
                "up",
                ContextMenuPrevious,
                Some("Editor && (showing_code_actions || showing_completions)"),
            ),
        ]);
    });
}

// ---------------------------------------------------------------------------
// Clip 1: saved source with one hint; a real keyboard edit replaces the hint
// with a layout/format warning while the file on disk stays untouched.
// ---------------------------------------------------------------------------

fn record_clip1(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
) -> Result<Value> {
    let started = Instant::now();
    let path = demo_file(config, "main.bend")?;
    ensure_fixture(&path, CLIP1_BASELINE_TEXT, "clip1 main.bend")?;

    let editor = open_file(cx, window, &path, false)?;
    wait_for_diagnostics(
        cx,
        &editor,
        config,
        "the saved analysis publishing the unneeded_plus hint",
        INITIAL_DIAGNOSTICS_TIMEOUT,
        |diagnostics| has_code(diagnostics, UNNEEDED_PLUS) && !has_code(diagnostics, FORMAT),
    )?;

    let before = read_editor_state(cx, &editor, &config.project_root)?;
    ensure_codes(
        &before.diagnostics,
        &[UNNEEDED_PLUS],
        &[FORMAT],
        "saved main.bend",
    )?;
    ensure!(
        before.text == CLIP1_BASELINE_TEXT,
        "main.bend buffer text does not match the prepared baseline: {:?}",
        before.text
    );
    ensure!(
        !before.dirty,
        "main.bend was unexpectedly dirty before the edit"
    );

    let mut scene = SceneRecorder::new(cx, window, config)?;
    let entry = gpui::point(scene.viewport.width * 0.5, scene.viewport.height * 0.5);
    scene.move_pointer_to(
        cx,
        window,
        config,
        entry,
        CH1_BASELINE,
        "Pointer enters the saved buffer",
    )?;
    scene.push(
        cx,
        window,
        config,
        "Saved and formatted: one genuine hint",
        BASELINE_HOLD,
        Interaction::result(CH1_BASELINE, "real bend2-lsp hint for the saved buffer"),
        json!({
            "phase": "saved-baseline",
            "editor": "main.bend",
            "buffer": before.json(),
            "expected": {
                "dirty": false,
                "present": [UNNEEDED_PLUS],
                "absent": [FORMAT],
                "disk_matches_buffer": true,
            },
        }),
    )?;

    // Drop the '+' binder modifier with a real click plus the Delete key.
    let plus_column = line_column(CLIP1_BASELINE_TEXT, 2, '+')?;
    let plus_target = editor_position_px(cx, window, &editor, Point::new(2, plus_column))?;
    scene.click_at(
        cx,
        window,
        config,
        plus_target,
        CH1_REMOVE_PLUS,
        "Click the binder modifier",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &editor,
        Point::new(2, plus_column),
        CH1_REMOVE_PLUS,
        "Nudge the caret onto the +",
    )?;
    let caret = caret_point(cx, window, &editor)?;
    ensure!(
        caret == Point::new(2, plus_column),
        "the genuine click left the caret at {caret:?} instead of the + at row 2 column {plus_column}"
    );
    scene.press(
        cx,
        window,
        config,
        "delete",
        CH1_REMOVE_PLUS,
        "Delete the + with the Delete key",
    )?;
    let after_delete = read_editor_state(cx, &editor, &config.project_root)?;
    let expected_after_delete = CLIP1_BASELINE_TEXT.replacen("+x", "x", 1);
    ensure!(
        after_delete.text == expected_after_delete,
        "the Delete key produced unexpected text: {:?}",
        after_delete.text
    );
    ensure!(
        after_delete.dirty,
        "main.bend did not become dirty after deleting the +"
    );

    // Re-indent the body with two real space keystrokes.
    let body_row = 3u32;
    let body_indent = line_indent(CLIP1_BASELINE_TEXT, body_row as usize)?;
    let body_target = editor_position_px(cx, window, &editor, Point::new(body_row, body_indent))?;
    scene.click_at(
        cx,
        window,
        config,
        body_target,
        CH1_INDENT,
        "Click the two-space body indentation",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &editor,
        Point::new(body_row, body_indent),
        CH1_INDENT,
        "Nudge the caret to the body indentation",
    )?;
    let body_caret = caret_point(cx, window, &editor)?;
    ensure!(
        body_caret == Point::new(body_row, body_indent),
        "the genuine click left the body caret at {body_caret:?} instead of row {body_row} column {body_indent}"
    );
    scene.rest_pointer(
        cx,
        window,
        config,
        CH1_INDENT,
        "Rest the pointer away from the edited line",
    )?;
    for _ in 0..2 {
        scene.type_char(
            cx,
            window,
            config,
            "space",
            CH1_INDENT,
            "Type one more indentation space",
        )?;
    }

    let typed = read_editor_state(cx, &editor, &config.project_root)?;
    ensure!(
        typed.text == CLIP1_UNSAVED_TEXT,
        "the typed indentation produced {:?} instead of the storyboard text",
        typed.text
    );
    ensure!(
        typed.dirty,
        "main.bend did not stay dirty after the typed edits"
    );

    wait_for_diagnostics(
        cx,
        &editor,
        config,
        "the real layout/format warning for the unsaved buffer",
        UPDATE_DIAGNOSTICS_TIMEOUT,
        |diagnostics| has_code(diagnostics, FORMAT) && !has_code(diagnostics, UNNEEDED_PLUS),
    )?;

    let after = read_editor_state(cx, &editor, &config.project_root)?;
    ensure_codes(
        &after.diagnostics,
        &[FORMAT],
        &[UNNEEDED_PLUS],
        "unsaved main.bend",
    )?;
    ensure!(
        after.text == CLIP1_UNSAVED_TEXT,
        "unsaved main.bend text differs from the storyboard: {:?}",
        after.text
    );
    ensure!(
        after.dirty,
        "main.bend did not stay dirty after the unsaved edits"
    );
    ensure!(
        after.on_disk_text.as_deref() == Some(CLIP1_BASELINE_TEXT),
        "main.bend on disk changed while recording; refusing to present this as an unsaved edit"
    );

    scene.push(
        cx,
        window,
        config,
        "Unsaved edit: the hint is gone, a real format warning appears",
        RESULT_HOLD,
        Interaction::result(
            CH1_RESULT,
            "no input in this frame; verified unsaved buffer, real FORMAT diagnostic and unchanged disk bytes",
        ),
        json!({
            "phase": "unsaved-edit",
            "editor": "main.bend",
            "buffer": after.json(),
            "expected": {
                "dirty": true,
                "present": [FORMAT],
                "absent": [UNNEEDED_PLUS],
                "disk_matches_buffer": false,
            },
        }),
    )?;

    let pointer_moves = scene.pointer_moves;
    let mouse_downs = scene.mouse_downs;
    let mouse_ups = scene.mouse_ups;
    let keystrokes = scene.keystrokes;
    let text_inputs = scene.text_inputs;
    let handled_events = scene.handled_events;
    let frames = scene.frames;
    let logical_width = f32::from(scene.viewport.width);
    let logical_height = f32::from(scene.viewport.height);
    let device_scale = scene.device_scale;

    let diagnostics_match =
        has_code(&after.diagnostics, FORMAT) && !has_code(&after.diagnostics, UNNEEDED_PLUS);
    let final_text_matches = after.text == CLIP1_UNSAVED_TEXT;
    let disk_unchanged = after.on_disk_text.as_deref() == Some(CLIP1_BASELINE_TEXT);

    Ok(json!({
        "scene": config.scene,
        "title": "Live lint, before you save",
        "capture": capture_metadata(config),
        "logical_width": logical_width,
        "logical_height": logical_height,
        "device_scale": device_scale,
        "frames": frames,
        "before": before.json(),
        "after": after.json(),
        "invariants": {
            "final_text_matches_expected": final_text_matches,
            "diagnostics_match_expected": diagnostics_match,
            "disk_files_unchanged": disk_unchanged,
            "buffer_dirty_after_edit": after.dirty,
            "save_commands_invoked": false,
            "pointer_moves_dispatched": pointer_moves,
            "mouse_downs_dispatched": mouse_downs,
            "mouse_ups_dispatched": mouse_ups,
            "keystrokes_dispatched": keystrokes,
            "text_inputs_dispatched": text_inputs,
            "handled_input_events": handled_events,
            "disk": {
                "main.bend": {
                    "path": path.display().to_string(),
                    "on_disk_text": after.on_disk_text,
                },
            },
        },
        "scene_elapsed_ms": started.elapsed().as_millis() as u64,
    }))
}

// ---------------------------------------------------------------------------
// Clip 2: both panes visible; a genuine click focuses the imported module and
// real keyboard selection/typing narrows its return type and body. The
// unchanged importer loses its semantic finding.
// ---------------------------------------------------------------------------

fn record_clip2(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
) -> Result<Value> {
    let started = Instant::now();
    let main_path = demo_file(config, "main.bend")?;
    let dep_path = demo_file(config, "dep.bend")?;
    ensure_fixture(&main_path, CLIP2_MAIN_TEXT, "clip2 main.bend")?;
    ensure_fixture(&dep_path, CLIP2_DEP_BASELINE_TEXT, "clip2 dep.bend")?;

    let main_editor = open_file(cx, window, &main_path, false)?;
    wait_for_diagnostics(
        cx,
        &main_editor,
        config,
        "the saved importer analysis (unused_result plus the text findings)",
        INITIAL_DIAGNOSTICS_TIMEOUT,
        |diagnostics| {
            has_code(diagnostics, UNUSED_RESULT)
                && has_code(diagnostics, UNNEEDED_PLUS)
                && has_code(diagnostics, FORMAT)
        },
    )?;
    let before = read_editor_state(cx, &main_editor, &config.project_root)?;
    ensure_codes(
        &before.diagnostics,
        &[UNUSED_RESULT, UNNEEDED_PLUS, FORMAT],
        &[],
        "saved main.bend importer",
    )?;
    ensure!(
        before.text == CLIP2_MAIN_TEXT,
        "main.bend buffer text does not match the prepared baseline: {:?}",
        before.text
    );

    let dep_editor = open_file(cx, window, &dep_path, true)?;
    let dep_before = read_editor_state(cx, &dep_editor, &config.project_root)?;
    ensure!(
        dep_before.text == CLIP2_DEP_BASELINE_TEXT && !dep_before.dirty,
        "dep.bend is not the prepared saved baseline: dirty={} text={:?}",
        dep_before.dirty,
        dep_before.text
    );

    let mut scene = SceneRecorder::new(cx, window, config)?;
    let entry = gpui::point(scene.viewport.width * 0.5, scene.viewport.height * 0.5);
    scene.move_pointer_to(
        cx,
        window,
        config,
        entry,
        CH2_PANES,
        "Pointer enters the split workspace",
    )?;
    scene.push(
        cx,
        window,
        config,
        "Both real panes: the importer and its imported module",
        BASELINE_HOLD,
        Interaction::result(
            CH2_PANES,
            "both real editors are visible; the importer already shows unused_result plus the text findings",
        ),
        json!({
            "phase": "importer-baseline",
            "editor": "main.bend",
            "buffer": before.json(),
            "imported_buffer": dep_before.json(),
            "expected": {
                "dirty": false,
                "present": [UNUSED_RESULT, UNNEEDED_PLUS, FORMAT],
                "imported_buffer_dirty": false,
            },
        }),
    )?;

    // Move focus between the two real panes with actual mouse clicks.
    let main_focus_row = 8u32;
    let main_focus_target =
        editor_position_px(cx, window, &main_editor, Point::new(main_focus_row, 4))?;
    scene.click_at(
        cx,
        window,
        config,
        main_focus_target,
        CH2_FOCUS_MAIN,
        "Click the importer body",
    )?;
    ensure!(
        editor_focused(cx, window, &main_editor)?,
        "the click did not focus the importer pane"
    );
    let main_caret = caret_point(cx, window, &main_editor)?;
    ensure!(
        main_caret.row == main_focus_row,
        "the importer click landed on buffer row {} instead of {main_focus_row}",
        main_caret.row
    );

    let dep_def_row = 2u32;
    let dep_line = CLIP2_DEP_BASELINE_TEXT
        .lines()
        .nth(dep_def_row as usize)
        .context("clip2 dep.bend baseline has no function signature line")?;
    let return_start = dep_line
        .rfind("List")
        .context("clip2 dep.bend signature has no return-type List")? as u32;
    let return_end = dep_line.len() as u32;
    let dep_target = editor_position_px(
        cx,
        window,
        &dep_editor,
        Point::new(dep_def_row, return_start),
    )?;
    scene.click_at(
        cx,
        window,
        config,
        dep_target,
        CH2_FOCUS_DEP,
        "Click the imported return type",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &dep_editor,
        Point::new(dep_def_row, return_start),
        CH2_FOCUS_DEP,
        "Nudge the caret onto the return type",
    )?;
    ensure!(
        editor_focused(cx, window, &dep_editor)?,
        "the click did not focus the imported module pane"
    );
    scene.rest_pointer(
        cx,
        window,
        config,
        CH2_TYPE,
        "Rest the pointer away from the return type",
    )?;

    // Select only the return type with the real SelectToEndOfLine chord, then
    // replace it with the narrow scalar type using genuine IME text input.
    scene.press(
        cx,
        window,
        config,
        "cmd-shift-right",
        CH2_TYPE,
        "Select the return type to the end of the line",
    )?;
    let (tail, head) = selection_points(cx, window, &dep_editor)?;
    ensure!(
        tail == Point::new(dep_def_row, return_start)
            && head == Point::new(dep_def_row, return_end),
        "the keyboard selection covers {tail:?}..{head:?} instead of the return type at row {dep_def_row} columns {return_start}..{return_end}"
    );
    for source in ["U", "3", "2", ":"] {
        scene.type_char(
            cx,
            window,
            config,
            source,
            CH2_TYPE,
            "Type the narrow U32 return type",
        )?;
    }

    let mut expected_after_type = CLIP2_DEP_BASELINE_TEXT.to_owned();
    let signature_line_start = CLIP2_DEP_BASELINE_TEXT
        .find(dep_line)
        .context("clip2 dep.bend function signature is not in the baseline text")?;
    expected_after_type.replace_range(
        signature_line_start + return_start as usize..signature_line_start + return_end as usize,
        "U32:",
    );
    let after_type = read_editor_state(cx, &dep_editor, &config.project_root)?;
    ensure!(
        after_type.text == expected_after_type,
        "typing the return type produced {:?} instead of {:?}",
        after_type.text,
        expected_after_type
    );
    // Word/LSP suggestions can pop up while typing; dismiss any real popup with
    // the official Escape binding before the next click so the click lands in
    // the editor instead of on a suggestion list.
    dismiss_editor_suggestions(
        &mut scene,
        cx,
        window,
        config,
        &dep_editor,
        CH2_TYPE,
        "Dismiss the editor suggestion popup",
    )?;

    // Preserve the final newline while replacing the body through real typing.
    // The isolated capture profile uses Bend's two-space indentation.
    let body_row = 3u32;
    let body_indent = line_indent(&expected_after_type, body_row as usize)?;
    let body_target =
        editor_position_px(cx, window, &dep_editor, Point::new(body_row, body_indent))?;
    scene.click_at(
        cx,
        window,
        config,
        body_target,
        CH2_BODY,
        "Click the recursive body",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &dep_editor,
        Point::new(body_row, body_indent),
        CH2_BODY,
        "Nudge the caret to the body start",
    )?;
    scene.rest_pointer(
        cx,
        window,
        config,
        CH2_BODY,
        "Rest the pointer away from the recursive body",
    )?;
    scene.press(
        cx,
        window,
        config,
        "cmd-shift-down",
        CH2_BODY,
        "Select the recursive body to the end of the buffer",
    )?;
    let newline_count = expected_after_type.matches('\n').count() as u32;
    let last_line_length = expected_after_type
        .lines()
        .last()
        .map(|line| line.len() as u32)
        .context("clip2 dep.bend text has no lines")?;
    let (body_tail, body_head) = selection_points(cx, window, &dep_editor)?;
    ensure!(
        body_tail == Point::new(body_row, body_indent) && body_head == Point::new(newline_count, 0),
        "SelectToEnd covers {body_tail:?}..{body_head:?} instead of the body from row {body_row} column {body_indent} to the buffer end"
    );
    scene.press(
        cx,
        window,
        config,
        "shift-left",
        CH2_BODY,
        "Keep the final newline by shrinking the selection",
    )?;
    let (shrunk_tail, shrunk_head) = selection_points(cx, window, &dep_editor)?;
    ensure!(
        shrunk_tail == Point::new(body_row, body_indent)
            && shrunk_head == Point::new(newline_count - 1, last_line_length),
        "the shrunk body selection is {shrunk_tail:?}..{shrunk_head:?} instead of ending before the final newline"
    );
    scene.type_char(cx, window, config, "0", CH2_BODY, "Type the scalar result")?;

    let dep_after = read_editor_state(cx, &dep_editor, &config.project_root)?;
    ensure!(
        dep_after.text == CLIP2_DEP_UNSAVED_TEXT,
        "the keyboard body replacement produced {:?} instead of {:?}",
        dep_after.text,
        CLIP2_DEP_UNSAVED_TEXT
    );
    ensure!(dep_after.dirty, "dep.bend did not become dirty");
    ensure!(
        dep_after.on_disk_text.as_deref() == Some(CLIP2_DEP_BASELINE_TEXT),
        "dep.bend on disk changed while recording; refusing to present this as an unsaved overlay"
    );
    dismiss_editor_suggestions(
        &mut scene,
        cx,
        window,
        config,
        &dep_editor,
        CH2_BODY,
        "Dismiss the editor suggestion popup",
    )?;

    wait_for_diagnostics(
        cx,
        &main_editor,
        config,
        "the importer republish without unused_result",
        UPDATE_DIAGNOSTICS_TIMEOUT,
        |diagnostics| {
            !has_code(diagnostics, UNUSED_RESULT)
                && has_code(diagnostics, UNNEEDED_PLUS)
                && has_code(diagnostics, FORMAT)
        },
    )?;

    let after = read_editor_state(cx, &main_editor, &config.project_root)?;
    ensure_codes(
        &after.diagnostics,
        &[UNNEEDED_PLUS, FORMAT],
        &[UNUSED_RESULT],
        "importer after the imported overlay",
    )?;
    ensure!(
        after.text == CLIP2_MAIN_TEXT,
        "main.bend text changed; the clip must leave the importer untouched: {:?}",
        after.text
    );
    ensure!(
        !after.dirty,
        "main.bend became dirty outside the dep overlay"
    );
    ensure!(
        after.on_disk_text.as_deref() == Some(CLIP2_MAIN_TEXT),
        "main.bend on disk changed while recording"
    );

    let final_dep = read_editor_state(cx, &dep_editor, &config.project_root)?;
    ensure!(
        final_dep.dirty && final_dep.text == CLIP2_DEP_UNSAVED_TEXT,
        "dep.bend is no longer the unsaved scalar overlay at the closing frame"
    );

    // Return focus to the importer with a real click before the closing frame.
    let focus_target = editor_position_px(cx, window, &main_editor, Point::new(main_focus_row, 4))?;
    scene.click_at(
        cx,
        window,
        config,
        focus_target,
        CH2_RESULT,
        "Click back to the importer",
    )?;
    ensure!(
        editor_focused(cx, window, &main_editor)?,
        "the closing click did not focus the importer pane"
    );

    scene.push(
        cx,
        window,
        config,
        "The semantic finding is gone; the text findings stay",
        RESULT_HOLD,
        Interaction::result(
            CH2_RESULT,
            "no input in this frame; verified importer diagnostics, both buffers and unchanged disk bytes",
        ),
        json!({
            "phase": "importer-after-overlay",
            "editor": "main.bend",
            "buffer": after.json(),
            "imported_buffer": final_dep.json(),
            "expected": {
                "dirty": false,
                "present": [UNNEEDED_PLUS, FORMAT],
                "absent": [UNUSED_RESULT],
                "imported_buffer_dirty": true,
            },
        }),
    )?;

    let pointer_moves = scene.pointer_moves;
    let mouse_downs = scene.mouse_downs;
    let mouse_ups = scene.mouse_ups;
    let keystrokes = scene.keystrokes;
    let text_inputs = scene.text_inputs;
    let handled_events = scene.handled_events;
    let frames = scene.frames;
    let logical_width = f32::from(scene.viewport.width);
    let logical_height = f32::from(scene.viewport.height);
    let device_scale = scene.device_scale;

    let importer_codes_match =
        has_code(&after.diagnostics, UNNEEDED_PLUS) && has_code(&after.diagnostics, FORMAT);
    let importer_lost_semantic = !has_code(&after.diagnostics, UNUSED_RESULT);
    let importer_unchanged = after.text == CLIP2_MAIN_TEXT;
    let imported_text_matches = final_dep.text == CLIP2_DEP_UNSAVED_TEXT;
    let disk_unchanged = after.on_disk_text.as_deref() == Some(CLIP2_MAIN_TEXT)
        && final_dep.on_disk_text.as_deref() == Some(CLIP2_DEP_BASELINE_TEXT);

    Ok(json!({
        "scene": config.scene,
        "title": "Unsaved imports update the importer",
        "capture": capture_metadata(config),
        "logical_width": logical_width,
        "logical_height": logical_height,
        "device_scale": device_scale,
        "frames": frames,
        "before": before.json(),
        "after": after.json(),
        "imported_buffer_before": dep_before.json(),
        "imported_buffer_after": final_dep.json(),
        "invariants": {
            "both_panes_visible": true,
            "importer_unchanged": importer_unchanged,
            "imported_buffer_text_matches_expected": imported_text_matches,
            "imported_buffer_dirty": final_dep.dirty,
            "importer_lost_semantic_finding": importer_lost_semantic,
            "importer_kept_text_findings": importer_codes_match,
            "disk_files_unchanged": disk_unchanged,
            "save_commands_invoked": false,
            "pointer_moves_dispatched": pointer_moves,
            "mouse_downs_dispatched": mouse_downs,
            "mouse_ups_dispatched": mouse_ups,
            "keystrokes_dispatched": keystrokes,
            "text_inputs_dispatched": text_inputs,
            "handled_input_events": handled_events,
            "disk": {
                "main.bend": {
                    "path": main_path.display().to_string(),
                    "on_disk_text": after.on_disk_text,
                },
                "dep.bend": {
                    "path": dep_path.display().to_string(),
                    "on_disk_text": final_dep.on_disk_text,
                },
            },
        },
        "scene_elapsed_ms": started.elapsed().as_millis() as u64,
    }))
}

// ---------------------------------------------------------------------------
// Clip 3: the same genuine keyboard edit, then a real Cmd-. key press opens
// the real code action menu, real navigation keys select the dangerous fix and
// Enter confirms it through editor::ConfirmCodeAction.
// ---------------------------------------------------------------------------

fn record_clip3(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
) -> Result<Value> {
    let started = Instant::now();
    let path = demo_file(config, "main.bend")?;
    ensure_fixture(&path, CLIP1_BASELINE_TEXT, "clip3 main.bend")?;

    let editor = open_file(cx, window, &path, false)?;
    wait_for_diagnostics(
        cx,
        &editor,
        config,
        "the saved unneeded_plus baseline before the edit",
        INITIAL_DIAGNOSTICS_TIMEOUT,
        |diagnostics| has_code(diagnostics, UNNEEDED_PLUS) && !has_code(diagnostics, FORMAT),
    )?;

    let before = read_editor_state(cx, &editor, &config.project_root)?;
    ensure_codes(
        &before.diagnostics,
        &[UNNEEDED_PLUS],
        &[FORMAT],
        "saved main.bend",
    )?;
    ensure!(
        before.text == CLIP1_BASELINE_TEXT && !before.dirty,
        "clip3 baseline buffer is not the prepared saved text: dirty={} text={:?}",
        before.dirty,
        before.text
    );

    let mut scene = SceneRecorder::new(cx, window, config)?;
    let entry = gpui::point(scene.viewport.width * 0.5, scene.viewport.height * 0.5);
    scene.move_pointer_to(
        cx,
        window,
        config,
        entry,
        CH3_BASELINE,
        "Pointer enters the saved buffer",
    )?;
    scene.push(
        cx,
        window,
        config,
        "Saved and formatted: one genuine hint",
        BASELINE_HOLD,
        Interaction::result(CH3_BASELINE, "real bend2-lsp hint before the edit"),
        json!({
            "phase": "saved-baseline",
            "editor": "main.bend",
            "buffer": before.json(),
        }),
    )?;

    // The same real keyboard edit as clip 1: click, Delete, click, two spaces.
    let plus_column = line_column(CLIP1_BASELINE_TEXT, 2, '+')?;
    let plus_target = editor_position_px(cx, window, &editor, Point::new(2, plus_column))?;
    scene.click_at(
        cx,
        window,
        config,
        plus_target,
        CH1_REMOVE_PLUS,
        "Click the binder modifier",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &editor,
        Point::new(2, plus_column),
        CH1_REMOVE_PLUS,
        "Nudge the caret onto the +",
    )?;
    scene.press(
        cx,
        window,
        config,
        "delete",
        CH1_REMOVE_PLUS,
        "Delete the + with the Delete key",
    )?;

    let body_row = 3u32;
    let body_indent = line_indent(CLIP1_BASELINE_TEXT, body_row as usize)?;
    let body_target = editor_position_px(cx, window, &editor, Point::new(body_row, body_indent))?;
    scene.click_at(
        cx,
        window,
        config,
        body_target,
        CH1_INDENT,
        "Click the two-space body indentation",
    )?;
    scene.settle_caret(
        cx,
        window,
        config,
        &editor,
        Point::new(body_row, body_indent),
        CH1_INDENT,
        "Nudge the caret to the body indentation",
    )?;
    scene.rest_pointer(
        cx,
        window,
        config,
        CH1_INDENT,
        "Rest the pointer away from the edited line",
    )?;
    for _ in 0..2 {
        scene.type_char(
            cx,
            window,
            config,
            "space",
            CH1_INDENT,
            "Type one more indentation space",
        )?;
    }

    wait_for_diagnostics(
        cx,
        &editor,
        config,
        "the unsaved layout/format warning that the fix is requested for",
        UPDATE_DIAGNOSTICS_TIMEOUT,
        |diagnostics| has_code(diagnostics, FORMAT) && !has_code(diagnostics, UNNEEDED_PLUS),
    )?;

    let warning_state = read_editor_state(cx, &editor, &config.project_root)?;
    ensure!(
        warning_state.text == CLIP1_UNSAVED_TEXT && warning_state.dirty,
        "clip3 buffer is not the unsaved warning text: dirty={} text={:?}",
        warning_state.dirty,
        warning_state.text
    );
    ensure_codes(
        &warning_state.diagnostics,
        &[FORMAT],
        &[UNNEEDED_PLUS],
        "clip3 unsaved main.bend",
    )?;
    let warning_caret = caret_point(cx, window, &editor)?;
    ensure!(
        caret_inside_code(&warning_state.diagnostics, FORMAT, warning_caret),
        "the caret {warning_caret:?} is not inside the real FORMAT diagnostic range"
    );

    scene.push(
        cx,
        window,
        config,
        "A real warning from the unsaved buffer",
        MENU_HOLD,
        Interaction::result(
            CH3_WARNING,
            "caret is inside the real layout/format diagnostic range",
        ),
        json!({
            "phase": "unsaved-warning",
            "editor": "main.bend",
            "buffer": warning_state.json(),
            "caret": {"line": warning_caret.row + 1, "column": warning_caret.column + 1},
            "expected": {"dirty": true, "present": [FORMAT], "absent": [UNNEEDED_PLUS]},
        }),
    )?;

    // Cmd-. is a real key press routed through the installed official binding.
    let (toggle_key, toggle_handled) = dispatch_key(cx, window, "cmd-.")?;
    scene.keystroke_dispatched(toggle_handled);
    ensure!(
        toggle_handled,
        "the Cmd-. keystroke was not accepted by the focused window"
    );
    let entries = wait_for_code_action_menu(cx, &editor)?;
    let matching = find_dangerous_format_action(&entries)?;
    scene.push(
        cx,
        window,
        config,
        "Explicitly asked: the real code action menu is open",
        MENU_HOLD,
        Interaction::keystroke(
            CH3_MENU,
            toggle_key.unparse(),
            toggle_handled,
            "menu opened by the keymap-routed editor::ToggleCodeActions action",
        ),
        json!({
            "phase": "code-action-menu",
            "editor": "main.bend",
            "buffer": warning_state.json(),
            "menu_entries": entries.iter().map(MenuEntry::json).collect::<Vec<_>>(),
            "matching_entry": matching.json(),
        }),
    )?;

    let mut selected = selected_code_action_item(&editor, cx)
        .context("the code action menu disappeared before selection")?;
    let mut guard = 0usize;
    while selected != matching.index {
        ensure!(
            guard <= entries.len(),
            "the menu selection never reached the dangerous fix at index {}",
            matching.index
        );
        let key = if selected < matching.index {
            "down"
        } else {
            "up"
        };
        scene.press(
            cx,
            window,
            config,
            key,
            CH3_SELECT,
            "Move the menu selection toward the dangerous fix",
        )?;
        let next = selected_code_action_item(&editor, cx)
            .context("the code action menu closed during keyboard navigation")?;
        ensure!(
            next != selected,
            "the dispatched {key} key did not move the code action menu selection"
        );
        selected = next;
        guard += 1;
    }
    ensure!(
        selected == matching.index,
        "the menu selection is {selected} instead of the dangerous fix at {}",
        matching.index
    );

    scene.push(
        cx,
        window,
        config,
        "The dangerous fix is selected by real keys",
        MENU_HOLD,
        Interaction::result(
            CH3_SELECT,
            "no input in this frame; verified the real menu selection index",
        ),
        json!({
            "phase": "code-action-selected",
            "editor": "main.bend",
            "selected_index": selected,
            "matching_entry": matching.json(),
            "menu_entries": entries.iter().map(MenuEntry::json).collect::<Vec<_>>(),
        }),
    )?;

    let (confirm_key, confirm_handled) = dispatch_key(cx, window, "enter")?;
    scene.keystroke_dispatched(confirm_handled);
    ensure!(
        confirm_handled,
        "the Enter keystroke was not accepted by the focused window"
    );
    let menu_closed = cx.update(|app| code_action_menu(&editor, app).is_none());
    ensure!(
        menu_closed,
        "Enter did not confirm the menu through editor::ConfirmCodeAction; the code action menu is still open"
    );
    scene.push(
        cx,
        window,
        config,
        "Confirm the selected dangerous fix with Enter",
        MENU_HOLD,
        Interaction::keystroke(
            CH3_APPLY,
            confirm_key.unparse(),
            confirm_handled,
            "menu closed synchronously by the real editor::ConfirmCodeAction handler",
        ),
        json!({
            "phase": "confirm-code-action",
            "editor": "main.bend",
            "selected_index": matching.index,
        }),
    )?;

    let canonical_text = CLIP3_CANONICAL_TEXT;
    let wait_buffer = cx.update(|app| editor_buffer(&editor, app))?;
    let wait_result = crate::wait_for(cx, APPLY_TIMEOUT, move |app| {
        let text = wait_buffer.read(app).snapshot().text();
        let diagnostics = read_buffer_diagnostics(&wait_buffer, app);
        text == canonical_text
            && !has_code(&diagnostics, FORMAT)
            && !has_code(&diagnostics, UNNEEDED_PLUS)
    });

    let applied = read_editor_state(cx, &editor, &config.project_root)?;
    wait_result.with_context(|| {
        format!(
            "waiting for the confirmed dangerous fix to land in the buffer; observed text {:?} with codes {:?}",
            applied.text,
            codes(&applied.diagnostics)
        )
    })?;

    ensure!(
        applied.text == CLIP3_CANONICAL_TEXT,
        "the applied fix produced unexpected text: {:?}",
        applied.text
    );
    ensure!(
        applied.dirty,
        "the applied fix should leave the buffer unsaved (dirty); it is clean, so something saved it"
    );
    ensure_codes(
        &applied.diagnostics,
        &[],
        &[FORMAT, UNNEEDED_PLUS],
        "main.bend after applying the dangerous fix",
    )?;
    ensure!(
        applied.on_disk_text.as_deref() == Some(CLIP1_BASELINE_TEXT),
        "main.bend on disk changed while recording; no save may happen in this clip"
    );

    scene.push(
        cx,
        window,
        config,
        "The versioned edit landed in the buffer; the warning is gone",
        RESULT_HOLD,
        Interaction::result(
            CH3_APPLY,
            "no input in this frame; verified applied WorkspaceEdit, cleared diagnostics, dirty buffer and unchanged disk bytes",
        ),
        json!({
            "phase": "dangerous-fix-applied",
            "editor": "main.bend",
            "buffer": applied.json(),
            "applied_action": {
                "index": matching.index,
                "label": matching.label,
                "kind": matching.kind,
            },
            "expected": {
                "dirty": true,
                "absent": [FORMAT, UNNEEDED_PLUS],
                "disk_matches_buffer": false,
            },
        }),
    )?;

    let pointer_moves = scene.pointer_moves;
    let mouse_downs = scene.mouse_downs;
    let mouse_ups = scene.mouse_ups;
    let keystrokes = scene.keystrokes;
    let text_inputs = scene.text_inputs;
    let handled_events = scene.handled_events;
    let frames = scene.frames;
    let logical_width = f32::from(scene.viewport.width);
    let logical_height = f32::from(scene.viewport.height);
    let device_scale = scene.device_scale;

    let applied_text_matches = applied.text == CLIP3_CANONICAL_TEXT;
    let diagnostics_cleared =
        !has_code(&applied.diagnostics, FORMAT) && !has_code(&applied.diagnostics, UNNEEDED_PLUS);
    let disk_unchanged = applied.on_disk_text.as_deref() == Some(CLIP1_BASELINE_TEXT);
    let menu_entries_json = entries.iter().map(MenuEntry::json).collect::<Vec<_>>();

    Ok(json!({
        "scene": config.scene,
        "title": "Apply a fix with Cmd-. and Enter",
        "capture": capture_metadata(config),
        "logical_width": logical_width,
        "logical_height": logical_height,
        "device_scale": device_scale,
        "frames": frames,
        "before_edit": warning_state.json(),
        "after_apply": applied.json(),
        "code_actions": {
            "toggle_keystroke": toggle_key.unparse(),
            "toggle_handled": toggle_handled,
            "confirm_keystroke": confirm_key.unparse(),
            "confirm_handled": confirm_handled,
            "menu_entries": menu_entries_json,
            "selected_index": selected,
            "applied": matching.json(),
        },
        "invariants": {
            "applied_text_matches_expected": applied_text_matches,
            "diagnostics_cleared": diagnostics_cleared,
            "buffer_dirty_after_apply": applied.dirty,
            "disk_files_unchanged": disk_unchanged,
            "menu_confirmed_by_real_enter_key": confirm_handled,
            "menu_opened_by_real_cmd_period_key": toggle_handled,
            "save_commands_invoked": false,
            "linter_cli_fix_invoked": false,
            "pointer_moves_dispatched": pointer_moves,
            "mouse_downs_dispatched": mouse_downs,
            "mouse_ups_dispatched": mouse_ups,
            "keystrokes_dispatched": keystrokes,
            "text_inputs_dispatched": text_inputs,
            "handled_input_events": handled_events,
            "disk": {
                "main.bend": {
                    "path": path.display().to_string(),
                    "on_disk_text": applied.on_disk_text,
                },
            },
        },
        "scene_elapsed_ms": started.elapsed().as_millis() as u64,
    }))
}

// ---------------------------------------------------------------------------
// Genuine GPUI input dispatch
// ---------------------------------------------------------------------------

fn dispatch_mouse_move(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    position: gpui::Point<Pixels>,
) -> Result<(gpui::Point<Pixels>, bool)> {
    let result = cx.update_window(window.into(), |_root, window, cx| {
        let result = window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers: Modifiers::default(),
            }),
            cx,
        );
        (window.mouse_position(), !result.propagate)
    })?;
    Ok(result)
}

fn dispatch_mouse_button(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    position: gpui::Point<Pixels>,
    down: bool,
) -> Result<(gpui::Point<Pixels>, bool)> {
    let result = cx.update_window(window.into(), |_root, window, cx| {
        let input = if down {
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            })
        } else {
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            })
        };
        let result = window.dispatch_event(input, cx);
        (window.mouse_position(), !result.propagate)
    })?;
    Ok(result)
}

fn dispatch_key(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    source: &str,
) -> Result<(Keystroke, bool)> {
    let keystroke = Keystroke::parse(source)
        .map_err(|error| anyhow::anyhow!("invalid capture keystroke {source:?}: {error}"))?;
    let handled = cx.update_window(window.into(), |_root, window, cx| {
        window.dispatch_keystroke(keystroke.clone(), cx)
    })?;
    Ok((keystroke, handled))
}

// ---------------------------------------------------------------------------
// Scene recorder
// ---------------------------------------------------------------------------

struct Interaction {
    kind: &'static str,
    keys: Vec<String>,
    text: Option<String>,
    handled: bool,
    chapter: &'static str,
    note: Option<String>,
}

impl Interaction {
    fn result(chapter: &'static str, note: &str) -> Self {
        Self {
            kind: "result",
            keys: Vec::new(),
            text: None,
            handled: false,
            chapter,
            note: Some(note.to_owned()),
        }
    }

    fn keystroke(chapter: &'static str, key: String, handled: bool, note: &str) -> Self {
        Self {
            kind: "keystroke",
            keys: vec![key],
            text: None,
            handled,
            chapter,
            note: Some(note.to_owned()),
        }
    }
}

struct SceneRecorder {
    scene_id: String,
    frames: Vec<Value>,
    cursor: gpui::Point<Pixels>,
    viewport: gpui::Size<Pixels>,
    device_scale: f32,
    next_index: usize,
    pointer_moves: u64,
    mouse_downs: u64,
    mouse_ups: u64,
    keystrokes: u64,
    text_inputs: u64,
    handled_events: u64,
}

impl SceneRecorder {
    fn new(
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
    ) -> Result<Self> {
        let (cursor, viewport, device_scale) = window.update(cx, |_workspace, window, _cx| {
            (
                window.mouse_position(),
                window.viewport_size(),
                window.scale_factor(),
            )
        })?;
        Ok(Self {
            scene_id: config.scene.clone(),
            frames: Vec::new(),
            cursor,
            viewport,
            device_scale,
            next_index: 0,
            pointer_moves: 0,
            mouse_downs: 0,
            mouse_ups: 0,
            keystrokes: 0,
            text_inputs: 0,
            handled_events: 0,
        })
    }

    fn keystroke_dispatched(&mut self, handled: bool) {
        self.keystrokes += 1;
        if handled {
            self.handled_events += 1;
        }
    }

    fn push(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        caption: &str,
        hold_seconds: f64,
        interaction: Interaction,
        state: Value,
    ) -> Result<()> {
        self.next_index += 1;
        let id = format!("{}-{:02}", self.scene_id, self.next_index);
        let Value::Object(mut state_object) = state else {
            bail!("frame state must be a JSON object");
        };
        state_object.insert(
            "interaction".to_owned(),
            interaction_json(self.cursor, self.viewport, &interaction),
        );
        let frame = crate::capture_frame(
            cx,
            window,
            config,
            &id,
            caption,
            hold_seconds,
            Value::Object(state_object),
        )
        .with_context(|| format!("capturing frame {id}"))?;
        self.frames.push(frame);
        Ok(())
    }

    fn move_pointer_to(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        target: gpui::Point<Pixels>,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        let start = self.cursor;
        let dx = f32::from(target.x) - f32::from(start.x);
        let dy = f32::from(target.y) - f32::from(start.y);
        let steps = if dx.abs() + dy.abs() < 3.0 {
            1
        } else {
            POINTER_STEPS
        };
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let eased = 1.0 - (1.0 - t).powi(3);
            let position = gpui::point(
                gpui::px(f32::from(start.x) + dx * eased),
                gpui::px(f32::from(start.y) + dy * eased),
            );
            let (observed, handled) = dispatch_mouse_move(cx, window, position)?;
            self.cursor = observed;
            self.pointer_moves += 1;
            if handled {
                self.handled_events += 1;
            }
            self.push(
                cx,
                window,
                config,
                caption,
                POINTER_STEP_HOLD,
                Interaction {
                    kind: "pointer_move",
                    keys: Vec::new(),
                    text: None,
                    handled,
                    chapter,
                    note: Some(format!(
                        "dispatched GPUI MouseMove step {step}/{steps} to ({:.1}, {:.1})",
                        f32::from(observed.x),
                        f32::from(observed.y)
                    )),
                },
                json!({"phase": "pointer-move", "step": step, "steps": steps}),
            )?;
        }
        Ok(())
    }

    fn click_at(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        position: gpui::Point<Pixels>,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        self.move_pointer_to(cx, window, config, position, chapter, caption)?;

        let (down_cursor, down_handled) = dispatch_mouse_button(cx, window, position, true)?;
        self.cursor = down_cursor;
        self.mouse_downs += 1;
        if down_handled {
            self.handled_events += 1;
        }
        self.push(
            cx,
            window,
            config,
            caption,
            CLICK_HOLD,
            Interaction {
                kind: "mouse_down",
                keys: Vec::new(),
                text: None,
                handled: down_handled,
                chapter,
                note: Some(format!(
                    "dispatched GPUI MouseDown (left) at ({:.1}, {:.1})",
                    f32::from(position.x),
                    f32::from(position.y)
                )),
            },
            json!({"phase": "mouse-down", "button": "left"}),
        )?;

        let (up_cursor, up_handled) = dispatch_mouse_button(cx, window, position, false)?;
        self.cursor = up_cursor;
        self.mouse_ups += 1;
        if up_handled {
            self.handled_events += 1;
        }
        self.push(
            cx,
            window,
            config,
            caption,
            CLICK_HOLD,
            Interaction {
                kind: "mouse_up",
                keys: Vec::new(),
                text: None,
                handled: up_handled,
                chapter,
                note: Some(format!(
                    "dispatched GPUI MouseUp (left) at ({:.1}, {:.1})",
                    f32::from(position.x),
                    f32::from(position.y)
                )),
            },
            json!({"phase": "mouse-up", "button": "left"}),
        )?;

        Ok(())
    }

    /// Parks the real pointer on the status bar so long diagnostic waits cannot
    /// open an editor hover popover over the filmed code.
    fn rest_pointer(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        let target = gpui::point(
            gpui::px(f32::from(self.viewport.width) - 40.0),
            gpui::px(f32::from(self.viewport.height) - 14.0),
        );
        self.move_pointer_to(cx, window, config, target, chapter, caption)
    }

    fn settle_caret(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        editor: &Entity<Editor>,
        target: Point,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        let mut nudges = 0usize;
        loop {
            let caret = caret_point(cx, window, editor)?;
            ensure!(
                caret.row == target.row,
                "the genuine click landed on buffer row {} instead of row {}; refusing to guess a text position",
                caret.row,
                target.row
            );
            if caret.column == target.column {
                return Ok(());
            }
            ensure!(
                nudges < MAX_CARET_NUDGES,
                "could not move the caret to column {} on row {} within {MAX_CARET_NUDGES} nudges (currently {})",
                target.column,
                target.row,
                caret.column
            );
            let key = if caret.column < target.column {
                "right"
            } else {
                "left"
            };
            let previous = caret.column;
            self.press(cx, window, config, key, chapter, caption)?;
            let moved = caret_point(cx, window, editor)?;
            ensure!(
                moved.column != previous || moved.row != caret.row,
                "the dispatched {key} key did not move the caret"
            );
            ensure!(
                moved.row == target.row,
                "the caret left row {} while nudging to the target column",
                target.row
            );
            nudges += 1;
        }
    }

    fn press(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        source: &str,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        let (keystroke, handled) = dispatch_key(cx, window, source)?;
        self.keystroke_dispatched(handled);
        ensure!(
            handled,
            "the dispatched keystroke {source:?} was not accepted by the focused window"
        );
        self.push(
            cx,
            window,
            config,
            caption,
            KEY_HOLD,
            Interaction {
                kind: "keystroke",
                keys: vec![keystroke.unparse()],
                text: None,
                handled,
                chapter,
                note: Some(format!("dispatched real GPUI KeyDown for {source:?}")),
            },
            json!({"phase": "keystroke"}),
        )?;
        Ok(())
    }

    fn type_char(
        &mut self,
        cx: &mut HeadlessAppContext,
        window: WindowHandle<Workspace>,
        config: &DemoConfig,
        source: &str,
        chapter: &'static str,
        caption: &str,
    ) -> Result<()> {
        let (keystroke, handled) = dispatch_key(cx, window, source)?;
        let text = keystroke
            .clone()
            .with_simulated_ime()
            .key_char
            .clone()
            .with_context(|| format!("the capture keystroke {source:?} has no IME text"))?;
        self.keystroke_dispatched(handled);
        ensure!(handled, "the IME text path rejected the character {text:?}");
        self.text_inputs += 1;
        self.push(
            cx,
            window,
            config,
            caption,
            TEXT_HOLD,
            Interaction {
                kind: "text_input",
                keys: vec![keystroke.unparse()],
                text: Some(text.clone()),
                handled,
                chapter,
                note: Some(format!(
                    "typed {text:?} through the editor's real IME input handler"
                )),
            },
            json!({"phase": "text-input"}),
        )?;
        Ok(())
    }
}

fn interaction_json(
    cursor: gpui::Point<Pixels>,
    viewport: gpui::Size<Pixels>,
    interaction: &Interaction,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("kind".to_owned(), json!(interaction.kind));
    map.insert(
        "cursor".to_owned(),
        json!({"x": f32::from(cursor.x), "y": f32::from(cursor.y)}),
    );
    map.insert(
        "viewport".to_owned(),
        json!({
            "width": f32::from(viewport.width),
            "height": f32::from(viewport.height),
        }),
    );
    map.insert("keys".to_owned(), json!(interaction.keys));
    map.insert(
        "text".to_owned(),
        match &interaction.text {
            Some(text) => json!(text),
            None => Value::Null,
        },
    );
    map.insert("handled".to_owned(), json!(interaction.handled));
    map.insert("chapter".to_owned(), json!(interaction.chapter));
    if let Some(note) = &interaction.note {
        map.insert("note".to_owned(), json!(note));
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Coordinates, caret and selection probes
// ---------------------------------------------------------------------------

/// Converts a real buffer point into absolute logical-window pixels for a
/// genuine mouse event.
///
/// `Editor::display_to_pixel_point` is relative to the editor's text area, so
/// the editor's own bounds, the real gutter width from
/// `EditorSnapshot::gutter_dimensions` and the text line height are added here.
fn editor_position_px(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    buffer_point: Point,
) -> Result<gpui::Point<Pixels>> {
    let position = window.update(
        cx,
        |_workspace, window, cx| -> Result<gpui::Point<Pixels>> {
            editor.update(cx, |editor, cx| -> Result<gpui::Point<Pixels>> {
                let style = capture_editor_style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                let snapshot = editor.snapshot(&*window, cx);
                let gutter =
                    snapshot.gutter_dimensions(font_id, font_size, &style, &mut *window, cx);
                let line_height = style.text.line_height_in_pixels(window.rem_size());
                let bounds = editor
                    .last_bounds()
                    .copied()
                    .context("the editor element has not been laid out yet")?;
                let display_point = buffer_point.to_display_point(&snapshot);
                let relative = editor
                    .display_to_pixel_point(display_point, &snapshot, &mut *window, cx)
                    .context("the requested buffer point is above the visible scroll region")?;
                Ok(gpui::point(
                    bounds.origin.x + gutter.width + relative.x + gpui::px(1.0),
                    bounds.origin.y + relative.y + line_height * 0.5,
                ))
            })
        },
    )??;
    Ok(position)
}

/// Reconstructs the editor text style from the real theme settings so the
/// gutter width matches the painted gutter.
fn capture_editor_style(cx: &App) -> EditorStyle {
    let settings = theme_settings::ThemeSettings::get_global(cx);
    let mut style = EditorStyle::default();
    style.text = gpui::TextStyle {
        font_family: settings.buffer_font.family.clone(),
        font_features: settings.buffer_font.features.clone(),
        font_fallbacks: settings.buffer_font.fallbacks.clone(),
        font_size: settings.buffer_font_size(cx).into(),
        font_weight: settings.buffer_font.weight,
        ..Default::default()
    };
    style
}

fn caret_point(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    editor: &Entity<Editor>,
) -> Result<Point> {
    let (_, head) = selection_points(cx, window, editor)?;
    Ok(head)
}

fn selection_points(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    editor: &Entity<Editor>,
) -> Result<(Point, Point)> {
    let points = window.update(cx, |_workspace, window, cx| -> Result<(Point, Point)> {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let selection = editor.selections.newest_display(&snapshot);
            Ok((
                selection.tail().to_point(&snapshot),
                selection.head().to_point(&snapshot),
            ))
        })
    })??;
    Ok(points)
}

fn editor_focused(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    editor: &Entity<Editor>,
) -> Result<bool> {
    let focused = window.update(cx, |_workspace, window, cx| {
        editor.read(cx).focus_handle(cx).is_focused(window)
    })?;
    Ok(focused)
}

fn selected_code_action_item(
    editor: &Entity<Editor>,
    cx: &mut HeadlessAppContext,
) -> Option<usize> {
    cx.update(|app| {
        let editor = editor.read(app);
        let menu_state = editor.context_menu().borrow();
        match menu_state.as_ref() {
            Some(CodeContextMenu::CodeActions(menu)) if menu.visible() => Some(menu.selected_item),
            _ => None,
        }
    })
}

/// Dismisses a genuinely displayed editor context menu (for example a
/// completions popup opened by typed input) with the official Escape binding so
/// the next recorded click is delivered to the editor, not to the popup.
fn dismiss_editor_suggestions(
    scene: &mut SceneRecorder,
    cx: &mut HeadlessAppContext,
    window: WindowHandle<Workspace>,
    config: &DemoConfig,
    editor: &Entity<Editor>,
    chapter: &'static str,
    caption: &str,
) -> Result<()> {
    let visible = cx.update(|app| editor.read(app).context_menu_visible());
    if visible {
        scene.press(cx, window, config, "escape", chapter, caption)?;
        let still_visible = cx.update(|app| editor.read(app).context_menu_visible());
        ensure!(
            !still_visible,
            "Escape did not dismiss the editor context menu; refusing to click through a live popup"
        );
    }
    Ok(())
}

fn caret_inside_code(diagnostics: &[DiagnosticInfo], code: &str, caret: Point) -> bool {
    diagnostics.iter().any(|diagnostic| {
        if !diagnostic.code.ends_with(code) {
            return false;
        }
        let after_start = (caret.row, caret.column) >= diagnostic.start;
        let before_end = (caret.row, caret.column) <= diagnostic.end;
        after_start && before_end
    })
}

fn line_column(text: &str, row: usize, needle: char) -> Result<u32> {
    let line = text
        .lines()
        .nth(row)
        .with_context(|| format!("fixture text has no line {row}"))?;
    let column = line
        .find(needle)
        .with_context(|| format!("fixture line {row} contains no {needle:?}"))?;
    Ok(column as u32)
}

fn line_indent(text: &str, row: usize) -> Result<u32> {
    let line = text
        .lines()
        .nth(row)
        .with_context(|| format!("fixture text has no line {row}"))?;
    Ok((line.len() - line.trim_start().len()) as u32)
}

fn capture_metadata(config: &DemoConfig) -> Value {
    json!({
        "kind": "GPUI headless Metal render of a real windowless Workspace, captured with HeadlessAppContext::capture_screenshot",
        "provenance": "real Workspace, real editor buffers, real filesystem worktree, real native bend2-lsp diagnostics and code actions",
        "clock": "GPUI TestDispatcher drives tasks; waits are real wall-clock polls of actual language-server publications",
        "input_provenance": "every filmed pointer move, click and key press was dispatched as a real GPUI PlatformInput/KeyDown event into the focused window; no Editor::edit, change_selections or direct action-handler call was used for a filmed edit",
        "key_bindings": [
            "cmd-. editor::ToggleCodeActions",
            "enter editor::ConfirmCodeAction in Editor && showing_code_actions",
            "enter editor::Newline in Editor && mode == full",
            "down/up editor::ContextMenuNext/Previous in Editor && (showing_code_actions || showing_completions)",
            "backspace/delete/tab/escape/left/right/shift-left/cmd-left/cmd-right/cmd-shift-right/cmd-shift-down in Editor",
        ],
        "timing_note": "Scripted walkthrough of real editor state, not a latency benchmark. hold_seconds are presentation holds.",
        "project_root": config.project_root.display().to_string(),
        "output_dir": config.output_dir.display().to_string(),
        "save_commands_invoked": false,
        "scene": config.scene,
    })
}
