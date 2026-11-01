#!/usr/bin/env python3
"""Compose genuine headless-Zed capture journals into honest H.264 movies.

Only ``RUN/interactive-captures/SCENE/report.json`` and the capture PNGs that
report names are read.  Every pointer position, click cue, key cap, typed
string, chapter and handled state comes from the recorded input journal;
nothing is inferred from a before/after diff and no editor UI is drawn.

``verify_capture`` is the stdlib-only authority check.  ``render_run`` composes
the stills with Pillow (optional, lazily imported; see
``zed_demo_requirements.txt``) and encodes H.264/yuv420p at 30 fps with
faststart, an explicit CFR frame sequence and ``-t`` equal to the journal hold
sum, then probes the result, fully decodes it and records a proof.
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import json
import math
import os
import re
import shutil
import struct
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence

PROOF_SCHEMA = "bend2-zed-demo/video-proof/1"
INDEX_SCHEMA = "bend2-zed-demo/video-index/1"

SCENES = (
    "clip1-unsaved-entry",
    "clip2-imported-type",
    "clip3-dangerous-action",
)
SCENE_ORDER = {scene: index for index, scene in enumerate(SCENES)}

FPS = 30
FRAME_ADVANCE_TOLERANCE = 1.0 / FPS + 1e-6
EDITOR_DISPLAY_WIDTH = 1600
MARGIN = 24
HEADER_HEIGHT = 64
PANEL_PAD = 18
PANEL_RADIUS = 10
CURSOR_TOLERANCE = 2.0

DEFAULT_SANS_FONTS = (
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
)
DEFAULT_MONO_FONTS = (
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
)

BACKGROUND = "#171a21"
PANEL_FILL = "#1b202a"
HAIRLINE = "#2c3340"
FOREGROUND = "#eef2f8"
MUTED = "#9aa5b6"
DIM = "#7f8a9b"
ACCENT = "#f0a05f"
KEY_FILL = "#2b3240"
KEY_BORDER = "#4a5568"
KEY_TEXT = "#f6f8fb"
PRESSED_FILL = "#302a26"
PRESSED_BORDER = "#cf8a52"
PRESSED_TEXT = "#ffe0c0"
TYPED_FILL = "#262c37"
TYPED_BORDER = "#3f4a5c"
ROUTED_FILL = "#20302a"
ROUTED_TEXT = "#a9dcb3"
PROGRESS_TRACK = "#2b3240"
PROGRESS_FILL = "#d7955a"
CLICK_RING = (255, 178, 106)

FOOTER_TEXT = (
    "Genuine headless Zed window and bend-lint LSP frames. Pointer, click ring "
    "and key HUD annotate the recorded input journal; holds are scripted for "
    "readability, not measured latency."
)

KIND_LABELS = {
    "pointer_move": "Pointer move",
    "mouse_down": "Mouse down",
    "mouse_up": "Mouse up",
    "keystroke": "Key press",
    "text_input": "Text input",
    "wait": "Wait",
    "result": "Result",
}
KEY_KINDS = ("keystroke", "text_input")

MODIFIER_LABELS = {
    "cmd": "Cmd",
    "command": "Cmd",
    "shift": "Shift",
    "alt": "Alt",
    "option": "Alt",
    "ctrl": "Ctrl",
    "control": "Ctrl",
    "fn": "Fn",
}

NAMED_KEYS = {
    "enter": "Enter",
    "return": "Enter",
    "backspace": "Backspace",
    "delete": "Delete",
    "space": "Space",
    "tab": "Tab",
    "escape": "Escape",
    "up": "Up",
    "down": "Down",
    "left": "Left",
    "right": "Right",
    "home": "Home",
    "end": "End",
    "pageup": "Page Up",
    "pagedown": "Page Down",
}

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
SCENE_PATTERN = re.compile(r"[A-Za-z0-9._-]+")


class RenderError(ValueError):
    """A capture report or encoding step cannot be turned into an honest movie."""


def fail(message: str) -> None:
    raise RenderError(message)


# ---------------------------------------------------------------------------
# Small utilities


def run_tool(arguments: Sequence[Any], *, label: str) -> str:
    command = [str(argument) for argument in arguments]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        fail(f"{label}: could not run {command[0]!r}: {error}")
    if result.returncode:
        detail = (result.stderr or result.stdout or "").strip()
        fail(f"{label} failed (exit {result.returncode}): {detail}")
    return result.stdout


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        fail(f"could not hash {path}: {error}")
    return digest.hexdigest()


def read_json(path: Path, label: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        fail(f"{label}: could not read {path}: {error}")
    except json.JSONDecodeError as error:
        fail(f"{label}: {path} is not valid JSON: {error}")


def atomic_write_json(path: Path, payload: Any) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.partial")
    try:
        temporary.write_text(
            json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        os.replace(temporary, path)
    except OSError as error:
        fail(f"could not write {path}: {error}")


def is_number(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def positive_number(value: Any, where: str) -> float:
    if not is_number(value):
        fail(f"{where}: expected a finite number, got {value!r}")
    number = float(value)
    if number <= 0:
        fail(f"{where}: expected a positive number, got {value!r}")
    return number


def optional_nonnegative_int(value: Any, where: str) -> int | None:
    if value is None:
        return None
    if isinstance(value, float) and value.is_integer():
        value = int(value)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        fail(f"{where}: expected a non-negative integer or null, got {value!r}")
    return value


def required_text(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value.strip():
        fail(f"{where}: expected a non-empty string, got {value!r}")
    return value.strip()


def within(path: Path, root: Path) -> bool:
    return path == root or root in path.parents


def png_size(path: Path, where: str) -> tuple[int, int]:
    """Read a PNG's dimensions from IHDR without decoding pixels."""
    try:
        with path.open("rb") as handle:
            header = handle.read(24)
    except OSError as error:
        fail(f"{where}: could not read PNG {path}: {error}")
    if (
        len(header) < 24
        or header[:8] != PNG_SIGNATURE
        or header[8:12] != struct.pack(">I", 13)
        or header[12:16] != b"IHDR"
    ):
        fail(f"{where}: {path} is not a PNG image (missing IHDR header)")
    width, height = struct.unpack(">II", header[16:24])
    if width <= 0 or height <= 0:
        fail(f"{where}: PNG {path} declares invalid size {width}x{height}")
    return width, height


# ---------------------------------------------------------------------------
# Capture authority


def _capture_location(run: Path, scene: str) -> tuple[Path, Path]:
    if not isinstance(scene, str) or not SCENE_PATTERN.fullmatch(scene):
        fail(f"invalid scene name {scene!r}")
    run = Path(run)
    if not run.is_dir():
        fail(f"run directory does not exist: {run}")
    captures_root = run / "interactive-captures"
    if not captures_root.is_dir():
        fail(
            f"{run}: no interactive-captures directory; record the scenes before "
            "rendering them"
        )
    resolved_root = captures_root.resolve()
    scene_dir = captures_root / scene
    report_path = scene_dir / "report.json"
    if not report_path.is_file():
        fail(f"missing capture report for {scene}: {report_path}")
    resolved_scene = scene_dir.resolve()
    if not within(resolved_scene, resolved_root):
        fail(
            f"{scene}: capture directory {scene_dir} resolves to {resolved_scene}, "
            f"outside {resolved_root}; refusing to read captures from elsewhere"
        )
    resolved_report = report_path.resolve()
    if not within(resolved_report, resolved_scene):
        fail(
            f"{scene}: capture report {report_path} resolves to {resolved_report}, "
            f"outside {resolved_scene}"
        )
    return resolved_scene, resolved_report


def _resolve_frame_file(
    raw: Any, report_dir: Path, scene_dir: Path, where: str
) -> Path:
    if not isinstance(raw, str) or not raw.strip():
        fail(f"{where}.file: expected a non-empty string, got {raw!r}")
    candidate = Path(raw.strip()).expanduser()
    if not candidate.is_absolute():
        candidate = report_dir / candidate
    try:
        resolved = candidate.resolve(strict=True)
    except OSError:
        fail(f"{where}.file: {raw!r} does not exist (looked for {candidate})")
    if not resolved.is_file():
        fail(f"{where}.file: {raw!r} is not a regular file ({resolved})")
    if not within(resolved, scene_dir):
        fail(
            f"{where}.file: {raw!r} resolves to {resolved}, outside the capture "
            f"scene directory {scene_dir}; only recorded capture pixels may be used"
        )
    return resolved


def _parse_interaction(raw: Any, where: str) -> dict[str, Any]:
    if not isinstance(raw, dict):
        fail(
            f"{where}: missing; every filmed frame must carry its input journal "
            "so cues are never inferred from image diffs"
        )
    kind = required_text(raw.get("kind"), f"{where}.kind")
    if kind not in KIND_LABELS:
        fail(
            f"{where}.kind: {kind!r} has no honest presentation; supported kinds "
            f"are {', '.join(sorted(KIND_LABELS))}"
        )

    cursor = raw.get("cursor")
    if not isinstance(cursor, dict):
        fail(f"{where}.cursor: missing; the pointer position must persist in every frame")
    cursor_x = cursor.get("x")
    cursor_y = cursor.get("y")
    if not is_number(cursor_x) or not is_number(cursor_y):
        fail(f"{where}.cursor: expected finite x/y logical coordinates, got {cursor!r}")

    viewport = raw.get("viewport")
    if not isinstance(viewport, dict):
        fail(f"{where}.viewport: missing; cursor mapping needs the logical viewport")
    viewport_width = viewport.get("width")
    viewport_height = viewport.get("height")
    if not is_number(viewport_width) or not is_number(viewport_height):
        fail(f"{where}.viewport: expected finite width/height, got {viewport!r}")
    if float(viewport_width) <= 0 or float(viewport_height) <= 0:
        fail(f"{where}.viewport: width/height must be positive, got {viewport!r}")

    keys_raw = raw.get("keys")
    if not isinstance(keys_raw, list):
        fail(f"{where}.keys: expected a list of journaled key strings, got {keys_raw!r}")
    keys: list[str] = []
    for key_index, key in enumerate(keys_raw):
        if not isinstance(key, str) or not key.strip():
            fail(f"{where}.keys[{key_index}]: expected a non-empty string, got {key!r}")
        keys.append(key.strip())

    if "text" not in raw:
        fail(f"{where}.text: required (null when nothing was typed)")
    text_raw = raw["text"]
    if text_raw is not None and not isinstance(text_raw, str):
        fail(f"{where}.text: expected a string or null, got {text_raw!r}")
    text = text_raw if isinstance(text_raw, str) and text_raw != "" else None

    if "handled" not in raw:
        fail(f"{where}.handled: required; acceptance cannot be assumed")
    handled = raw["handled"]
    if not isinstance(handled, bool):
        fail(f"{where}.handled: expected a boolean, got {handled!r}")

    chapter = required_text(raw.get("chapter"), f"{where}.chapter")

    note_raw = raw.get("note")
    if note_raw is not None and not isinstance(note_raw, str):
        fail(f"{where}.note: expected a string or null, got {note_raw!r}")
    note = note_raw.strip() if isinstance(note_raw, str) and note_raw.strip() else None

    if kind in KEY_KINDS:
        if not keys:
            fail(
                f"{where}: {kind} journals no key, so no key cap could be drawn "
                "without inventing one"
            )
        if handled is not True:
            fail(
                f"{where}: {kind} was not handled by the editor; filming it would "
                "claim input the demo did not receive"
            )
        if kind == "text_input" and text is None:
            fail(f"{where}: text_input journals no typed text")
    else:
        if keys:
            fail(
                f"{where}: {kind} carries keys {keys}; key caps are drawn only for "
                "journaled keystroke/text_input events"
            )
        if text is not None:
            fail(
                f"{where}: {kind} carries typed text {text!r}; typed badges are drawn "
                "only for journaled text_input events"
            )

    return {
        "kind": kind,
        "cursor": {"x": float(cursor_x), "y": float(cursor_y)},
        "viewport": {
            "width": float(viewport_width),
            "height": float(viewport_height),
        },
        "keys": keys,
        "text": text,
        "handled": handled,
        "chapter": chapter,
        "note": note,
    }


def _logic_viewport(report: dict[str, Any], frames: list[dict[str, Any]], scene: str) -> tuple[float, float]:
    for width_key, height_key in (
        ("logical_width", "logical_height"),
        ("viewport_width", "viewport_height"),
    ):
        if width_key in report or height_key in report:
            return (
                positive_number(report.get(width_key), f"{scene} report.{width_key}"),
                positive_number(report.get(height_key), f"{scene} report.{height_key}"),
            )
    if frames:
        viewport = frames[0]["interaction"]["viewport"]
        return (viewport["width"], viewport["height"])
    capture = report.get("capture")
    if isinstance(capture, dict):
        nested = capture.get("logical_size") or capture.get("viewport")
        if isinstance(nested, dict):
            return (
                positive_number(nested.get("width"), f"{scene} report.capture viewport width"),
                positive_number(nested.get("height"), f"{scene} report.capture viewport height"),
            )
    fail(
        f"{scene} report: no logical viewport; cursor coordinates cannot be mapped "
        "honestly without Window::viewport_size"
    )


def verify_capture(run: Path, scene: str) -> dict[str, Any]:
    """Validate one scene's recorded journal, pixels and hashes.

    Stdlib only: no Pillow import is needed to prove that a capture is
    genuine.  Rejects journal entries that could only be filmed by inventing
    cues, frame images outside the scene directory (including symlink
    escapes), non-finite or out-of-bounds cursor positions and invalid
    durations.  Returns the authoritative per-frame journal plus hashes.
    """
    scene_dir, report_path = _capture_location(Path(run), scene)
    report = read_json(report_path, f"{scene} report")
    if not isinstance(report, dict):
        fail(f"{scene} report: expected a JSON object at {report_path}")
    warnings: list[str] = []

    scene_value = report.get("scene")
    if isinstance(scene_value, str) and scene_value and scene_value != scene:
        warnings.append(f"report.scene is {scene_value!r}, directory is {scene!r}")
    title = required_text(report.get("title"), f"{scene} report.title")

    frames_raw = report.get("frames")
    if not isinstance(frames_raw, list) or not frames_raw:
        fail(f"{scene} report: frames must be a non-empty list")

    report_dir = report_path.parent
    parsed: list[dict[str, Any]] = []
    for index, raw in enumerate(frames_raw, start=1):
        where = f"{scene} frame {index}"
        if not isinstance(raw, dict):
            fail(f"{where}: expected an object, got {raw!r}")
        state = raw.get("state")
        interaction = _parse_interaction(
            state.get("interaction") if isinstance(state, dict) else None,
            f"{where} state.interaction",
        )
        source = _resolve_frame_file(raw.get("file"), report_dir, scene_dir, where)
        width, height = png_size(source, where)
        declared_width = raw.get("width")
        declared_height = raw.get("height")
        for declared, actual, label in (
            (declared_width, width, "width"),
            (declared_height, height, "height"),
        ):
            if declared is None:
                continue
            if isinstance(declared, bool) or not isinstance(declared, int) or declared <= 0:
                fail(f"{where}.{label}: expected a positive integer or null, got {declared!r}")
            if declared != actual:
                fail(
                    f"{where}.{label}: report declares {declared} but the PNG is "
                    f"{width}x{height}; geometry would be misreported"
                )
        caption = raw.get("caption")
        if not isinstance(caption, str):
            fail(f"{where}.caption: expected a string, got {caption!r}")
        hold_seconds = positive_number(raw.get("hold_seconds"), f"{where}.hold_seconds")
        observed = optional_nonnegative_int(raw.get("observed_wall_ms"), f"{where}.observed_wall_ms")
        if observed is None:
            warnings.append(f"frame {index}: observed_wall_ms is absent")
        source_sha256 = sha256_file(source)
        declared_sha = raw.get("sha256")
        if declared_sha is not None and declared_sha != source_sha256:
            fail(
                f"{where}.sha256: report declares {declared_sha!r} but the PNG digest "
                f"is {source_sha256}; the pixels are not the recorded ones"
            )
        parsed.append(
            {
                "index": index,
                "source": source,
                "source_sha256": source_sha256,
                "width": width,
                "height": height,
                "caption": caption,
                "hold_seconds": hold_seconds,
                "observed_wall_ms": observed,
                "interaction": interaction,
                "frame_logical": _frame_logical(raw, where),
                "device_scale": _device_scale(raw, where),
            }
        )

    logical_width, logical_height = _logic_viewport(report, parsed, scene)
    logical_aspect = logical_width / logical_height

    canonical_aspect = parsed[0]["width"] / parsed[0]["height"]
    frames: list[dict[str, Any]] = []
    start_seconds = 0.0
    observed_total = 0
    for record in parsed:
        index = record["index"]
        where = f"{scene} frame {index}"
        interaction = record["interaction"]
        viewport = interaction["viewport"]
        if (
            abs(viewport["width"] - logical_width) > 0.75
            or abs(viewport["height"] - logical_height) > 0.75
        ):
            fail(
                f"{where}: interaction.viewport {viewport['width']}x{viewport['height']} "
                f"disagrees with the scene logical viewport {logical_width:g}x"
                f"{logical_height:g}; one consistent cursor transform is required"
            )
        frame_logical = record["frame_logical"]
        if frame_logical is not None and (
            abs(frame_logical[0] - logical_width) > 0.75
            or abs(frame_logical[1] - logical_height) > 0.75
        ):
            fail(
                f"{where}: frame logical size {frame_logical[0]}x{frame_logical[1]} "
                f"disagrees with the scene logical viewport {logical_width:g}x"
                f"{logical_height:g}; one consistent cursor transform is required"
            )
        image_aspect = record["width"] / record["height"]
        if abs(image_aspect / logical_aspect - 1.0) > 0.02:
            fail(
                f"{where}: PNG aspect {record['width']}x{record['height']} does not "
                f"match logical viewport {logical_width:g}x{logical_height:g}; the "
                "cursor transform would be wrong"
            )
        if abs(image_aspect / canonical_aspect - 1.0) > 0.005:
            fail(
                f"{where}: PNG aspect {record['width']}x{record['height']} differs "
                "from frame 1; one consistent framing is required"
            )
        device_scale = record["device_scale"]
        if device_scale is not None:
            png_scale = record["width"] / logical_width
            if abs(png_scale / device_scale - 1.0) > 0.02:
                fail(
                    f"{where}: PNG width {record['width']} over logical width "
                    f"{logical_width:g} gives scale {png_scale:g}, but the capture "
                    f"reports device_scale {device_scale:g}; cursor mapping would be wrong"
                )
        cursor = interaction["cursor"]
        for axis, value, bound in (
            ("x", cursor["x"], logical_width),
            ("y", cursor["y"], logical_height),
        ):
            if value < -CURSOR_TOLERANCE or value > bound + CURSOR_TOLERANCE:
                fail(
                    f"{where}: cursor {axis}={value:g} is outside the logical viewport "
                    f"{logical_width:g}x{logical_height:g}; it is not in the coordinate "
                    "space the report declares"
                )
        frames.append(
            {
                "index": index,
                "source_file": str(record["source"]),
                "source_sha256": record["source_sha256"],
                "width": record["width"],
                "height": record["height"],
                "logical_width": logical_width,
                "logical_height": logical_height,
                "device_scale": device_scale,
                "caption": record["caption"],
                "hold_seconds": record["hold_seconds"],
                "observed_wall_ms": record["observed_wall_ms"],
                "start_seconds": start_seconds,
                "interaction": interaction,
            }
        )
        if record["observed_wall_ms"] is not None:
            observed_total += record["observed_wall_ms"]
        start_seconds += record["hold_seconds"]

    total_seconds = sum(frame["hold_seconds"] for frame in frames)
    if not math.isfinite(total_seconds) or total_seconds <= 0:
        fail(f"{scene}: journal hold sum must be positive")
    quantized = sum(count for _, count in _frame_counts(frames))
    quantized_seconds = quantized / FPS
    if abs(quantized_seconds - total_seconds) > FRAME_ADVANCE_TOLERANCE:
        fail(
            f"{scene}: hold sum {total_seconds:.3f}s cannot be represented at {FPS} fps "
            f"within one frame (would be {quantized_seconds:.3f}s); journal holds are "
            "too small or too finely quantized"
        )

    report_device_scale = report.get("device_scale")
    if (
        report_device_scale is not None
        and is_number(report_device_scale)
        and float(report_device_scale) <= 0
    ):
        fail(
            f"{scene} report.device_scale: expected a positive number, "
            f"got {report_device_scale!r}"
        )

    capture = report.get("capture")
    if not isinstance(capture, dict):
        capture = {}
        warnings.append("report.capture provenance block is absent")

    final = frames[-1]
    return {
        "scene": scene,
        "title": title,
        "report": str(report_path),
        "report_sha256": sha256_file(report_path),
        "logical_viewport": {"width": logical_width, "height": logical_height},
        "device_scale": (
            float(report_device_scale)
            if is_number(report_device_scale)
            else frames[0]["device_scale"]
        ),
        "capture": capture,
        "frames": frames,
        "input_summary": _input_summary(frames),
        "chapters": _chapters(frames),
        "final_chapter": final["interaction"]["chapter"],
        "final_state": _final_state(frames, total_seconds),
        "presentation_seconds": round(total_seconds, 6),
        "observed_wall_ms_total": observed_total,
        "warnings": warnings,
    }


def _frame_logical(raw: dict[str, Any], where: str) -> tuple[float, float] | None:
    width = raw.get("logical_width")
    height = raw.get("logical_height")
    if width is None and height is None:
        return None
    return (
        positive_number(width, f"{where}.logical_width"),
        positive_number(height, f"{where}.logical_height"),
    )


def _device_scale(raw: dict[str, Any], where: str) -> float | None:
    value = raw.get("device_scale")
    if value is None:
        return None
    if not is_number(value) or float(value) <= 0:
        fail(f"{where}.device_scale: expected a positive finite number or null, got {value!r}")
    return float(value)


def _frame_counts(frames: Sequence[dict[str, Any]]) -> list[tuple[int, int]]:
    """Cumulative 30 fps frame counts; returns (frame index, displayed frames)."""
    counts: list[tuple[int, int]] = []
    cumulative = 0.0
    previous_end = 0
    for frame in frames:
        cumulative += frame["hold_seconds"]
        end_frame = max(previous_end + 1, round(cumulative * FPS))
        counts.append((frame["index"], end_frame - previous_end))
        previous_end = end_frame
    return counts


def _input_summary(frames: Sequence[dict[str, Any]]) -> dict[str, Any]:
    kinds: dict[str, int] = {}
    keys: list[str] = []
    for frame in frames:
        kind = frame["interaction"]["kind"]
        kinds[kind] = kinds.get(kind, 0) + 1
        for key in frame["interaction"]["keys"]:
            if key not in keys:
                keys.append(key)
    return {
        "kinds": kinds,
        "distinct_keys": keys,
        "chapters": _chapters(frames),
        "pointer_moves": kinds.get("pointer_move", 0),
        "mouse_downs": kinds.get("mouse_down", 0),
        "mouse_ups": kinds.get("mouse_up", 0),
        "keystrokes": kinds.get("keystroke", 0),
        "text_inputs": kinds.get("text_input", 0),
        "waits": kinds.get("wait", 0),
        "results": kinds.get("result", 0),
    }


def _chapters(frames: Sequence[dict[str, Any]]) -> list[str]:
    chapters: list[str] = []
    for frame in frames:
        chapter = frame["interaction"]["chapter"]
        if chapter not in chapters:
            chapters.append(chapter)
    return chapters


def _final_state(frames: Sequence[dict[str, Any]], total_seconds: float) -> dict[str, Any]:
    final = frames[-1]
    return {
        "frame": final["index"],
        "kind": final["interaction"]["kind"],
        "chapter": final["interaction"]["chapter"],
        "caption": final["caption"],
        "handled": final["interaction"]["handled"],
        "cursor": dict(final["interaction"]["cursor"]),
        "source_file": final["source_file"],
        "source_sha256": final["source_sha256"],
        "presentation_seconds": round(total_seconds, 6),
    }


# ---------------------------------------------------------------------------
# Optional Pillow, fonts and external tools


@dataclass(frozen=True)
class _Imaging:
    Image: Any
    ImageDraw: Any
    ImageFilter: Any
    ImageFont: Any


_IMAGING: _Imaging | None = None
_FONTS: tuple[Path, Path] | None = None


def _imaging() -> _Imaging:
    global _IMAGING
    if _IMAGING is None:
        try:
            from PIL import Image, ImageDraw, ImageFilter, ImageFont
        except ImportError as error:
            requirement = Path(__file__).with_name("zed_demo_requirements.txt")
            fail(
                "Pillow is required to compose frames but is not installed: "
                f"{error}. Install the pinned optional requirement with "
                f"'python3 -m pip install -r {requirement}'."
            )
        _IMAGING = _Imaging(Image, ImageDraw, ImageFilter, ImageFont)
    return _IMAGING


def _resolve_font(value: Path | str | None, fallbacks: Sequence[str], label: str) -> Path:
    if value is not None:
        path = Path(value).expanduser()
        if not path.is_file():
            fail(f"{label} font does not exist: {path}")
        return path.resolve()
    for candidate in fallbacks:
        path = Path(candidate)
        if path.is_file():
            return path
    fail(
        f"no {label} font found (tried {', '.join(fallbacks)}); pass an explicit "
        "--font/--mono-font path"
    )


def _resolve_tool(value: str, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        fail(f"{label} executable must be a non-empty path or command name")
    candidate = Path(value).expanduser()
    has_separator = os.sep in value or (os.altsep is not None and os.altsep in value)
    if candidate.is_absolute() or has_separator:
        if not candidate.is_file():
            fail(f"{label} executable does not exist: {candidate}")
        if not os.access(candidate, os.X_OK):
            fail(f"{label} executable is not executable: {candidate}")
        return str(candidate.resolve())
    found = shutil.which(value)
    if not found:
        fail(f"{label} executable {value!r} was not found on PATH; pass an explicit path")
    return found


_font_cache: dict[tuple[str, int, str], Any] = {}


def font(size: int, weight: str = "Regular", *, mono: bool = False) -> Any:
    if _FONTS is None:
        fail("internal: fonts are not configured")
    path = _FONTS[1] if mono else _FONTS[0]
    key = (str(path), size, weight)
    cached = _font_cache.get(key)
    if cached is not None:
        return cached
    try:
        loaded = _imaging().ImageFont.truetype(str(path), size)
    except OSError as error:
        fail(f"could not load font {path} for size {size}: {error}")
    if weight != "Regular" and not mono:
        try:
            loaded.set_variation_by_name(weight)
        except (AttributeError, OSError, ValueError):
            pass
    _font_cache[key] = loaded
    return loaded


def text_width(draw: Any, value: str, typeface: Any) -> float:
    return float(draw.textlength(value, font=typeface))


def wrap_text(
    draw: Any,
    value: str,
    typeface: Any,
    width: float,
    *,
    first_width: float | None = None,
) -> list[str]:
    """Wrap without dropping characters; long words are split to fit."""
    if not value:
        return []
    lines: list[str] = []
    for paragraph_index, paragraph in enumerate(value.split("\n")):
        available = first_width if paragraph_index == 0 and first_width else width
        words = paragraph.split(" ")
        current = ""
        for word in words:
            if current:
                trial = f"{current} {word}"
                if text_width(draw, trial, typeface) <= available:
                    current = trial
                    continue
                lines.append(current)
                current = ""
                available = width
            if text_width(draw, word, typeface) <= available:
                current = word
                continue
            chunk = ""
            for character in word:
                if chunk and text_width(draw, chunk + character, typeface) > available:
                    lines.append(chunk)
                    chunk = character
                    available = width
                else:
                    chunk += character
            current = chunk
        lines.append(current)
    return lines


def display_text(value: str) -> str:
    return (
        value.replace("\\", "\\\\")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )


def prettify_key(raw: str) -> str:
    """Present the journal's keystroke string without changing its identity."""
    value = raw.strip()
    if not value:
        return ""
    lowered = value.lower()
    if lowered in NAMED_KEYS:
        return NAMED_KEYS[lowered]
    if "-" not in value:
        if len(value) == 1 and value.isalpha():
            return value.upper()
        if len(value) > 1 and value.isalpha():
            return value[0].upper() + value[1:]
        return value
    parts = value.split("-")
    if not parts[-1]:
        return value
    labels: list[str] = []
    for part in parts[:-1]:
        labels.append(MODIFIER_LABELS.get(part.lower(), part[:1].upper() + part[1:]))
    key = parts[-1]
    if key.lower() in NAMED_KEYS:
        key = NAMED_KEYS[key.lower()]
    elif len(key) == 1 and key.isalpha():
        key = key.upper()
    elif len(key) > 1 and key.isalpha():
        key = key[0].upper() + key[1:]
    labels.append(key)
    return "-".join(labels)


def clip_text_to_width(draw: Any, value: str, typeface: Any, width: float) -> str:
    if text_width(draw, value, typeface) <= width:
        return value
    ellipsis = "…"
    candidate = value
    while candidate and text_width(draw, candidate + ellipsis, typeface) > width:
        candidate = candidate[:-1]
    return (candidate + ellipsis) if candidate else ellipsis


# ---------------------------------------------------------------------------
# Journal-driven composition


@dataclass
class Frame:
    index: int
    source: Path
    source_sha256: str
    caption: str
    hold_seconds: float
    observed_wall_ms: int | None
    width: int
    height: int
    kind: str
    cursor_x: float
    cursor_y: float
    keys: list[str]
    text: str | None
    handled: bool
    chapter: str
    note: str | None
    start_seconds: float


@dataclass
class Scene:
    scene: str
    title: str
    report_path: Path
    report_sha256: str
    logical_width: float
    logical_height: float
    observed_wall_total_ms: int
    frames: list[Frame]
    input_summary: dict[str, Any]
    final_state: dict[str, Any]
    presentation_seconds: float
    warnings: list[str] = field(default_factory=list)
    capture: dict[str, Any] = field(default_factory=dict)


def _scene_from_verified(verified: dict[str, Any]) -> Scene:
    viewport = verified["logical_viewport"]
    frames = [
        Frame(
            index=record["index"],
            source=Path(record["source_file"]),
            source_sha256=record["source_sha256"],
            caption=record["caption"],
            hold_seconds=record["hold_seconds"],
            observed_wall_ms=record["observed_wall_ms"],
            width=record["width"],
            height=record["height"],
            kind=record["interaction"]["kind"],
            cursor_x=record["interaction"]["cursor"]["x"],
            cursor_y=record["interaction"]["cursor"]["y"],
            keys=list(record["interaction"]["keys"]),
            text=record["interaction"]["text"],
            handled=record["interaction"]["handled"],
            chapter=record["interaction"]["chapter"],
            note=record["interaction"]["note"],
            start_seconds=record["start_seconds"],
        )
        for record in verified["frames"]
    ]
    return Scene(
        scene=verified["scene"],
        title=verified["title"],
        report_path=Path(verified["report"]),
        report_sha256=verified["report_sha256"],
        logical_width=viewport["width"],
        logical_height=viewport["height"],
        observed_wall_total_ms=verified["observed_wall_ms_total"],
        frames=frames,
        input_summary=verified["input_summary"],
        final_state=verified["final_state"],
        presentation_seconds=verified["presentation_seconds"],
        warnings=list(verified["warnings"]),
        capture=verified["capture"],
    )


class MetricsCache:
    def __init__(self) -> None:
        self.draw = _imaging().ImageDraw.Draw(_imaging().Image.new("RGB", (8, 8)))

    def width(self, value: str, typeface: Any) -> float:
        return text_width(self.draw, value, typeface)


@dataclass
class Layout:
    canvas_width: int
    canvas_height: int
    editor_width: int
    editor_height: int
    header_height: int
    panel_y: int
    panel_height: int
    footer_y: int
    footer_lines: list[str]
    inner_width: int
    image_scale: float
    display_scale: float
    display_scale_y: float
    chapter_lines_max: int
    caption_lines_max: int
    hud_rows_max: int


def kind_label(kind: str) -> str:
    return KIND_LABELS[kind]


def key_chip_width(metrics: MetricsCache, label: str) -> float:
    return metrics.width(label, font(20, "Semibold")) + 24.0


def typed_chip_label(value: str) -> str:
    return f'"{display_text(value)}"'


def handled_presentation(frame: Frame) -> bool:
    """Acceptance feedback is meaningful only for journaled keyboard input.

    ``verify_capture`` rejects keystroke/text_input frames the editor did not
    handle, so a filmed key cap is always a genuinely routed key press.
    """
    return frame.kind in KEY_KINDS


def handled_chip_width(metrics: MetricsCache, frame: Frame) -> float:
    if not handled_presentation(frame):
        return 0.0
    return metrics.width("routed", font(12, "Medium")) + 18.0


def hud_flow(metrics: MetricsCache, frame: Frame, inner_width: int) -> list[list[dict[str, Any]]]:
    """Flow descriptor, key caps and a typed chip into HUD rows."""
    handled_width = handled_chip_width(metrics, frame)
    reserved = (handled_width + 14.0) if handled_width else 0.0
    available = inner_width - reserved
    label_width = metrics.width("Input", font(12, "Regular")) + 12.0
    start_x = label_width

    rows: list[list[dict[str, Any]]] = []
    row: list[dict[str, Any]] = []
    x = start_x
    gap = 8.0

    def new_row() -> None:
        nonlocal row, x
        if row:
            rows.append(row)
        row = []
        x = 0.0

    descriptor = kind_label(frame.kind)
    descriptor_width = metrics.width(descriptor, font(15, "Semibold"))
    if x + descriptor_width > available and x > 0:
        new_row()
    row.append({"type": "descriptor", "text": descriptor, "x": x, "width": descriptor_width})
    x += descriptor_width + gap

    for raw_key in frame.keys:
        label = prettify_key(raw_key)
        if not label:
            continue
        if text_width(metrics.draw, label, font(20, "Semibold")) + 24.0 > available:
            label = clip_text_to_width(
                metrics.draw, label, font(20, "Semibold"), max(40.0, available - 24.0)
            )
        width = key_chip_width(metrics, label)
        if x + width > available and row:
            new_row()
        row.append({"type": "key", "text": label, "x": x, "width": width})
        x += width + gap

    if frame.text:
        value = typed_chip_label(frame.text)
        if row and available - x < 120.0:
            new_row()
        maximum = max(80.0, available - x)
        clipped = clip_text_to_width(
            metrics.draw, value, font(14, "Regular", mono=True), maximum
        )
        width = metrics.width(clipped, font(14, "Regular", mono=True)) + 20.0
        if x + width > available and row:
            new_row()
        row.append({"type": "typed", "text": clipped, "x": x, "width": width})
        x += width + gap

    if row:
        rows.append(row)
    return rows


def chapter_lines(metrics: MetricsCache, value: str, inner_width: int) -> list[str]:
    label = "Chapter"
    label_width = metrics.width(label, font(12, "Regular")) + 10.0
    return wrap_text(
        metrics.draw,
        value,
        font(17, "Semibold"),
        float(inner_width),
        first_width=max(120.0, inner_width - label_width),
    )


def caption_lines(metrics: MetricsCache, value: str, inner_width: int) -> list[str]:
    if not value.strip():
        return []
    return wrap_text(metrics.draw, value.strip(), font(20, "Medium"), float(inner_width))


def footer_lines(metrics: MetricsCache, width: int) -> list[str]:
    return wrap_text(metrics.draw, FOOTER_TEXT, font(13, "Regular"), float(width))


def build_layout(scene: Scene, canonical_size: tuple[int, int]) -> Layout:
    width_px, height_px = canonical_size
    image_scale = EDITOR_DISPLAY_WIDTH / width_px
    display_scale = EDITOR_DISPLAY_WIDTH / scene.logical_width
    editor_height = max(2, round(height_px * image_scale))
    editor_height += editor_height % 2
    display_scale_y = editor_height / scene.logical_height
    if abs(display_scale_y / display_scale - 1.0) > 0.02:
        fail(
            f"{scene.scene}: scaled editor aspect {EDITOR_DISPLAY_WIDTH}x{editor_height} "
            f"does not match logical viewport {scene.logical_width:g}x"
            f"{scene.logical_height:g}"
        )

    canvas_width = EDITOR_DISPLAY_WIDTH + 2 * MARGIN
    canvas_width += canvas_width % 2
    inner_width = EDITOR_DISPLAY_WIDTH - 2 * PANEL_PAD
    metrics = MetricsCache()

    chapter_max = 0
    caption_max = 0
    hud_rows_max = 0
    for frame in scene.frames:
        chapter_max = max(chapter_max, len(chapter_lines(metrics, frame.chapter, inner_width)))
        caption_max = max(caption_max, len(caption_lines(metrics, frame.caption, inner_width)))
        hud_rows_max = max(hud_rows_max, len(hud_flow(metrics, frame, inner_width)))

    chapter_height = chapter_max * 24
    caption_height = caption_max * 27
    hud_height = hud_rows_max * 36 + max(0, hud_rows_max - 1) * 8

    content_height = chapter_height
    if chapter_height and caption_height:
        content_height += 6
    content_height += caption_height
    if (chapter_height or caption_height) and hud_height:
        content_height += 10
    content_height += hud_height
    panel_height = PANEL_PAD + content_height + 12 + 3 + 14

    panel_y = HEADER_HEIGHT + editor_height + 14
    footer_y = panel_y + panel_height + 10
    footer = footer_lines(metrics, canvas_width - 2 * MARGIN)
    footer_height = len(footer) * 18
    canvas_height = footer_y + footer_height + 16
    canvas_height += canvas_height % 2

    return Layout(
        canvas_width=canvas_width,
        canvas_height=canvas_height,
        editor_width=EDITOR_DISPLAY_WIDTH,
        editor_height=editor_height,
        header_height=HEADER_HEIGHT,
        panel_y=panel_y,
        panel_height=panel_height,
        footer_y=footer_y,
        footer_lines=footer,
        inner_width=inner_width,
        image_scale=image_scale,
        display_scale=display_scale,
        display_scale_y=display_scale_y,
        chapter_lines_max=chapter_max,
        caption_lines_max=caption_max,
        hud_rows_max=hud_rows_max,
    )


@functools.lru_cache(maxsize=4)
def _pointer_patch(kind: str) -> Any:
    """A crisp arrow at (70, 60) plus a short click ring when journaled."""
    Image = _imaging().Image
    ImageDraw = _imaging().ImageDraw
    ImageFilter = _imaging().ImageFilter
    supersample = 4
    patch_size = 140
    tip_x, tip_y = 70.0, 60.0
    patch = Image.new(
        "RGBA", (patch_size * supersample, patch_size * supersample), (0, 0, 0, 0)
    )
    draw = ImageDraw.Draw(patch)
    points = [
        (0.0, 0.0),
        (0.0, 23.0),
        (5.6, 17.4),
        (9.2, 25.2),
        (12.9, 23.5),
        (9.4, 15.9),
        (16.2, 15.9),
    ]
    scaled = [((tip_x + x) * supersample, (tip_y + y) * supersample) for x, y in points]

    shadow = Image.new(
        "RGBA", (patch_size * supersample, patch_size * supersample), (0, 0, 0, 0)
    )
    shadow_draw = ImageDraw.Draw(shadow)
    shadow_draw.polygon(
        [
            ((tip_x + x + 2.2) * supersample, (tip_y + y + 3.0) * supersample)
            for x, y in points
        ],
        fill=(0, 0, 0, 120),
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(1.6 * supersample))
    patch.alpha_composite(shadow)

    if kind in ("mouse_down", "mouse_up"):
        down = kind == "mouse_down"
        radius = 18.0 if down else 24.0
        alpha = 240 if down else 140
        center_x = tip_x * supersample
        center_y = tip_y * supersample
        outer_radius = radius + 7.0
        draw.ellipse(
            [
                (tip_x - outer_radius) * supersample,
                (tip_y - outer_radius) * supersample,
                (tip_x + outer_radius) * supersample,
                (tip_y + outer_radius) * supersample,
            ],
            outline=(20, 14, 10, 70),
            width=int(3.4 * supersample),
        )
        draw.ellipse(
            [
                (tip_x - outer_radius) * supersample,
                (tip_y - outer_radius) * supersample,
                (tip_x + outer_radius) * supersample,
                (tip_y + outer_radius) * supersample,
            ],
            outline=(*CLICK_RING, max(50, alpha // 3)),
            width=int(1.8 * supersample),
        )
        draw.ellipse(
            [
                center_x - radius * supersample,
                center_y - radius * supersample,
                center_x + radius * supersample,
                center_y + radius * supersample,
            ],
            outline=(*CLICK_RING, alpha),
            width=int((3.0 if down else 2.0) * supersample),
        )
        if down:
            dot = 3.2
            draw.ellipse(
                [
                    center_x - dot * supersample,
                    center_y - dot * supersample,
                    center_x + dot * supersample,
                    center_y + dot * supersample,
                ],
                fill=(*CLICK_RING, 255),
            )

    draw.polygon(scaled, fill=(255, 255, 255, 255), outline=(6, 8, 12, 255))
    draw.line(scaled + [scaled[0]], fill=(6, 8, 12, 255), width=int(2.4 * supersample))
    return patch.resize((patch_size, patch_size), Image.LANCZOS)


def annotate_editor(editor: Any, frame: Frame, layout: Layout) -> Any:
    tip_x = frame.cursor_x * layout.display_scale
    tip_y = frame.cursor_y * layout.display_scale_y
    overlay = editor.convert("RGBA")
    patch = _pointer_patch(frame.kind if frame.kind in ("mouse_down", "mouse_up") else "pointer")
    overlay.paste(patch, (round(tip_x - 70.0), round(tip_y - 60.0)), patch)
    return overlay.convert("RGB")


def draw_key_chip(draw: Any, x: float, y: float, width: float, label: str, *, pressed: bool) -> None:
    draw.rounded_rectangle(
        [x, y, x + width - 1, y + 36 - 1],
        radius=7,
        fill=PRESSED_FILL if pressed else KEY_FILL,
        outline=PRESSED_BORDER if pressed else KEY_BORDER,
        width=1,
    )
    draw.text(
        (x + 12, y + 7),
        label,
        font=font(20, "Semibold"),
        fill=PRESSED_TEXT if pressed else KEY_TEXT,
    )


def draw_typed_chip(draw: Any, x: float, y: float, width: float, label: str) -> None:
    draw.rounded_rectangle(
        [x, y, x + width - 1, y + 30 - 1],
        radius=7,
        fill=TYPED_FILL,
        outline=TYPED_BORDER,
        width=1,
    )
    draw.text((x + 10, y + 7), label, font=font(14, "Regular", mono=True), fill="#dbe4ef")


def draw_handled_chip(draw: Any, x: float, y: float, frame: Frame) -> float:
    if not handled_presentation(frame):
        return 0.0
    label = "routed"
    width = float(draw.textlength(label, font=font(12, "Medium"))) + 18.0
    draw.rounded_rectangle(
        [x, y + 3, x + width - 1, y + 27],
        radius=12,
        fill=ROUTED_FILL,
        outline="#3b5a45",
        width=1,
    )
    draw.text((x + 9, y + 8), label, font=font(12, "Medium"), fill=ROUTED_TEXT)
    return width


def compose_frame(scene: Scene, frame: Frame, layout: Layout) -> Any:
    Image = _imaging().Image
    ImageDraw = _imaging().ImageDraw
    metrics = MetricsCache()
    canvas = Image.new("RGB", (layout.canvas_width, layout.canvas_height), BACKGROUND)
    draw = ImageDraw.Draw(canvas)

    # Header.
    right_label = "Bend · live lint"
    right_font = font(12, "Regular")
    available_title = (
        layout.canvas_width
        - 2 * MARGIN
        - float(draw.textlength(right_label, font=right_font))
        - 28
    )
    title_size = 27
    while (
        title_size > 20
        and float(draw.textlength(scene.title, font=font(title_size, "Semibold")))
        > available_title
    ):
        title_size -= 1
    draw.text((MARGIN, 18), scene.title, font=font(title_size, "Semibold"), fill=FOREGROUND)
    draw.text(
        (layout.canvas_width - MARGIN, 22),
        right_label,
        font=right_font,
        fill=DIM,
        anchor="ra",
    )

    # Editor pixels, resized through one transform, then annotated.
    try:
        opened = Image.open(frame.source)
    except OSError as error:
        fail(f"{scene.scene} frame {frame.index}: cannot read {frame.source}: {error}")
    with opened:
        if opened.size != (frame.width, frame.height):
            fail(
                f"{scene.scene} frame {frame.index}: PNG changed after verification "
                f"({opened.size[0]}x{opened.size[1]} instead of {frame.width}x"
                f"{frame.height}); refusing to publish mismatched evidence"
            )
        source = opened.convert("RGB")
    if source.size != (layout.editor_width, layout.editor_height):
        source = source.resize((layout.editor_width, layout.editor_height), Image.LANCZOS)
    editor = annotate_editor(source, frame, layout)
    editor_y = layout.header_height
    canvas.paste(editor, (MARGIN, editor_y))
    draw.rectangle(
        [
            MARGIN,
            editor_y,
            MARGIN + layout.editor_width - 1,
            editor_y + layout.editor_height - 1,
        ],
        outline=HAIRLINE,
        width=1,
    )

    # HUD panel.
    panel_x = MARGIN
    panel_y = layout.panel_y
    panel_bottom = panel_y + layout.panel_height
    draw.rounded_rectangle(
        [panel_x, panel_y, panel_x + layout.editor_width, panel_bottom],
        radius=PANEL_RADIUS,
        fill=PANEL_FILL,
        outline=HAIRLINE,
        width=1,
    )
    inner_x = panel_x + PANEL_PAD
    inner_y = panel_y + PANEL_PAD

    # Chapter, from the journal only.
    chapter_height = layout.chapter_lines_max * 24
    lines = chapter_lines(metrics, frame.chapter, layout.inner_width)
    label = "Chapter"
    label_width = metrics.width(label, font(12, "Regular")) + 10.0
    draw.text((inner_x, inner_y + 4), label, font=font(12, "Regular"), fill=DIM)
    for line_index, line in enumerate(lines):
        x = inner_x + label_width if line_index == 0 else inner_x
        draw.text((x, inner_y + line_index * 24), line, font=font(17, "Semibold"), fill=ACCENT)

    # Caption.
    caption_y = inner_y + chapter_height + (6 if chapter_height else 0)
    caption = caption_lines(metrics, frame.caption, layout.inner_width)
    for line_index, line in enumerate(caption):
        draw.text(
            (inner_x, caption_y + line_index * 27),
            line,
            font=font(20, "Medium"),
            fill=FOREGROUND,
        )
    caption_height = layout.caption_lines_max * 27

    hud_y = caption_y + caption_height
    if caption_height or chapter_height:
        hud_y += 10
    rows = hud_flow(metrics, frame, layout.inner_width)
    pressed = bool(frame.keys)
    row_height = 36.0
    for row_index, row in enumerate(rows):
        row_y = hud_y + row_index * (row_height + 8)
        for element in row:
            element_x = inner_x + float(element["x"])
            if element["type"] == "descriptor":
                draw.text(
                    (element_x, row_y + 5),
                    element["text"],
                    font=font(15, "Semibold"),
                    fill=MUTED,
                )
            elif element["type"] == "key":
                draw_key_chip(
                    draw,
                    element_x,
                    row_y,
                    float(element["width"]),
                    element["text"],
                    pressed=pressed,
                )
            elif element["type"] == "typed":
                draw_typed_chip(
                    draw, element_x, row_y, float(element["width"]), element["text"]
                )
        if row_index == 0:
            draw.text((inner_x, row_y + 8), "Input", font=font(12, "Regular"), fill=DIM)
            handled_width = handled_chip_width(metrics, frame)
            if handled_width:
                draw_handled_chip(
                    draw, inner_x + layout.inner_width - handled_width, row_y, frame
                )

    # Progress comes from the journaled holds, not from wall clock.
    total = sum(item.hold_seconds for item in scene.frames)
    end_time = frame.start_seconds + frame.hold_seconds
    progress = 0.0 if total <= 0 else min(1.0, max(0.0, end_time / total))
    track_y = panel_bottom - 14 - 3
    draw.rounded_rectangle(
        [inner_x, track_y, inner_x + layout.inner_width, track_y + 2],
        radius=1,
        fill=PROGRESS_TRACK,
    )
    fill_width = max(2.0, layout.inner_width * progress)
    draw.rounded_rectangle(
        [inner_x, track_y, inner_x + fill_width, track_y + 2],
        radius=1,
        fill=PROGRESS_FILL,
    )

    # Footer: one honest, unobtrusive line.
    for line_index, line in enumerate(layout.footer_lines):
        draw.text((MARGIN, layout.footer_y + line_index * 18), line, font=font(13, "Regular"), fill=DIM)

    return canvas


# ---------------------------------------------------------------------------
# CFR encoding and verification


def numeric_float(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value) if math.isfinite(float(value)) else None
    if isinstance(value, str):
        try:
            parsed = float(value)
        except ValueError:
            return None
        return parsed if math.isfinite(parsed) else None
    return None


def parse_rate(value: str) -> float:
    if "/" in value:
        numerator, denominator = value.split("/", 1)
        try:
            return float(numerator) / float(denominator)
        except (ValueError, ZeroDivisionError):
            return float("nan")
    try:
        return float(value)
    except ValueError:
        return float("nan")


def moov_before_mdat(path: Path) -> bool:
    """Read top-level MP4 boxes; faststart puts moov before mdat."""
    moov_offset: int | None = None
    mdat_offset: int | None = None
    with path.open("rb") as handle:
        offset = 0
        while True:
            header = handle.read(8)
            if len(header) < 8:
                break
            size = int.from_bytes(header[0:4], "big")
            box_type = header[4:8].decode("latin-1", "replace")
            if size == 1:
                extended = handle.read(8)
                if len(extended) < 8:
                    break
                size = int.from_bytes(extended, "big")
                header_size = 16
            else:
                header_size = 8
            if size < header_size:
                break
            if box_type == "moov" and moov_offset is None:
                moov_offset = offset
            if box_type == "mdat" and mdat_offset is None:
                mdat_offset = offset
            if moov_offset is not None and mdat_offset is not None:
                break
            offset += size
            handle.seek(offset)
    if moov_offset is None:
        fail(f"{path.name}: no moov box found; faststart cannot be verified")
    if mdat_offset is None:
        fail(f"{path.name}: no mdat box found")
    return moov_offset < mdat_offset


def probe_movie(ffprobe: str, path: Path) -> dict[str, Any]:
    output = run_tool(
        [
            ffprobe,
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt,width,height,r_frame_rate,avg_frame_rate,"
            "nb_frames,duration:format=duration,size",
            "-of",
            "json",
            str(path),
        ],
        label=f"ffprobe {path.name}",
    )
    try:
        return json.loads(output)
    except json.JSONDecodeError as error:
        fail(f"ffprobe {path.name}: invalid JSON: {error}")


def _chapter_timeline(scene: Scene, counts: list[tuple[int, int]]) -> list[dict[str, Any]]:
    """Map each journal chapter to the CFR frame range that carries it."""
    timeline: list[dict[str, Any]] = []
    movie_frame = 0
    for frame, (_, count) in zip(scene.frames, counts):
        entry = timeline[-1] if timeline and timeline[-1]["chapter"] == frame.chapter else None
        if entry is None:
            entry = {
                "chapter": frame.chapter,
                "first_frame": frame.index,
                "start_seconds": round(frame.start_seconds, 6),
                "first_movie_frame": movie_frame + 1,
                "frames": 0,
            }
            timeline.append(entry)
        entry["last_frame"] = frame.index
        entry["frames"] += count
        entry["last_movie_frame"] = movie_frame + count
        entry["end_seconds"] = round(frame.start_seconds + frame.hold_seconds, 6)
        movie_frame += count
    return timeline


def encode_scene(
    scene: Scene,
    layout: Layout,
    counts: list[tuple[int, int]],
    frames_dir: Path,
    output_path: Path,
    *,
    ffmpeg: str,
    ffprobe: str,
) -> dict[str, Any]:
    total_seconds = scene.presentation_seconds
    if total_seconds <= 0:
        fail(f"{scene.scene}: total hold_seconds must be positive")

    try:
        frames_dir.mkdir(parents=True, exist_ok=True)
        for stale in frames_dir.glob("step-*.png"):
            stale.unlink()
        sequence_dir = frames_dir / "sequence"
        if sequence_dir.exists():
            shutil.rmtree(sequence_dir)
        sequence_dir.mkdir()
    except OSError as error:
        fail(f"{scene.scene}: could not prepare the render workspace {frames_dir}: {error}")

    # Render each journal frame once, then expose exact 30 fps frame counts.
    # Cumulative rounding keeps each hold within half a frame of the journal.
    if len(counts) != len(scene.frames):
        fail(f"{scene.scene}: internal frame count mismatch")
    entries: list[tuple[Path, int]] = []
    for frame, (_, count) in zip(scene.frames, counts):
        destination = frames_dir / f"step-{frame.index:04d}.png"
        composed = compose_frame(scene, frame, layout)
        try:
            composed.save(destination, format="PNG", optimize=False, compress_level=6)
        except OSError as error:
            fail(f"{scene.scene} frame {frame.index}: could not write {destination}: {error}")
        entries.append((destination.resolve(), count))

    total_frames = sum(count for _, count in entries)
    frame_duration = total_frames / FPS
    if abs(frame_duration - total_seconds) > FRAME_ADVANCE_TOLERANCE:
        fail(
            f"{scene.scene}: hold sum {total_seconds:.3f}s cannot be represented at "
            f"{FPS} fps within one frame (would be {frame_duration:.3f}s)"
        )

    sequence_index = 0
    for source, count in entries:
        for _ in range(count):
            sequence_index += 1
            link = sequence_dir / f"{sequence_index:06d}.png"
            try:
                os.symlink(source, link)
            except OSError:
                shutil.copyfile(source, link)
    if sequence_index != total_frames:
        fail(f"{scene.scene}: internal sequence count mismatch")

    temporary_movie = output_path.with_name(f".{output_path.stem}.partial.mp4")
    if temporary_movie.exists():
        temporary_movie.unlink()

    try:
        run_tool(
            [
                ffmpeg,
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-framerate",
                str(FPS),
                "-start_number",
                "1",
                "-i",
                sequence_dir / "%06d.png",
                "-frames:v",
                str(total_frames),
                "-t",
                f"{total_seconds:.6f}",
                "-r",
                str(FPS),
                "-fps_mode",
                "cfr",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                temporary_movie,
            ],
            label=f"encode {scene.scene}",
        )

        probe = probe_movie(ffprobe, temporary_movie)
        streams = probe.get("streams") or []
        if not streams:
            fail(f"{scene.scene}: encoded movie has no video stream")
        stream = streams[0]
        codec = stream.get("codec_name")
        pixel_format = stream.get("pix_fmt")
        width_value = numeric_float(stream.get("width"))
        height_value = numeric_float(stream.get("height"))
        width = int(width_value) if width_value is not None else None
        height = int(height_value) if height_value is not None else None
        rate = parse_rate(stream.get("r_frame_rate") or stream.get("avg_frame_rate") or "")
        duration_value = stream.get("duration")
        format_duration = (probe.get("format") or {}).get("duration")
        parsed_duration = numeric_float(duration_value)
        if parsed_duration is None:
            parsed_duration = numeric_float(format_duration)
        duration = parsed_duration if parsed_duration is not None else float("nan")
        frame_count = stream.get("nb_frames")
        if isinstance(frame_count, str) and frame_count.isdigit():
            frame_count_int = int(frame_count)
        elif isinstance(frame_count, int):
            frame_count_int = frame_count
        else:
            frame_count_int = None

        if codec != "h264":
            fail(f"{scene.scene}: expected H.264, got {codec!r}")
        if pixel_format != "yuv420p":
            fail(f"{scene.scene}: expected yuv420p, got {pixel_format!r}")
        if width != layout.canvas_width or height != layout.canvas_height:
            fail(
                f"{scene.scene}: encoded size {width}x{height} does not match "
                f"composed canvas {layout.canvas_width}x{layout.canvas_height}"
            )
        if not math.isfinite(rate) or abs(rate - FPS) > 0.01:
            fail(f"{scene.scene}: expected {FPS} fps, got {rate!r}")
        if not math.isfinite(duration) or abs(duration - total_seconds) > FRAME_ADVANCE_TOLERANCE:
            fail(
                f"{scene.scene}: encoded duration {duration!r}s is not within one "
                f"frame of the journal hold sum {total_seconds:.3f}s"
            )
        if frame_count_int is None or abs(frame_count_int - total_frames) > 1:
            fail(
                f"{scene.scene}: encoded {frame_count_int!r} frames, expected "
                f"{total_frames} at {FPS} fps for {total_seconds:.3f}s"
            )
        if not moov_before_mdat(temporary_movie):
            fail(f"{scene.scene}: moov is after mdat; faststart is missing")

        # Full decode: the deliverable must not contain decode errors.
        run_tool(
            [ffmpeg, "-v", "error", "-xerror", "-i", temporary_movie, "-f", "null", "-"],
            label=f"decode verification {scene.scene}",
        )

        movie_bytes = temporary_movie.stat().st_size
        movie_sha256 = sha256_file(temporary_movie)
        os.replace(temporary_movie, output_path)
    finally:
        if temporary_movie.exists():
            temporary_movie.unlink()
        if sequence_dir.exists():
            shutil.rmtree(sequence_dir)

    return {
        "codec": "h264",
        "pixel_format": "yuv420p",
        "fps": FPS,
        "width": layout.canvas_width,
        "height": layout.canvas_height,
        "duration_seconds": round(float(duration), 6),
        "duration_target_seconds": round(total_seconds, 6),
        "duration_delta_seconds": round(abs(float(duration) - total_seconds), 6),
        "frames": frame_count_int,
        "expected_frames": total_frames,
        "faststart": True,
        "full_decode": "passed",
        "bytes": movie_bytes,
        "sha256": movie_sha256,
    }


# ---------------------------------------------------------------------------
# Proof and index


def _proof_payload(
    run: Path,
    scene: Scene,
    layout: Layout,
    counts: list[tuple[int, int]],
    output: dict[str, Any],
    movie_path: Path,
    frames_dir: Path,
) -> dict[str, Any]:
    frames = []
    for frame, (_, count) in zip(scene.frames, counts):
        display_x = frame.cursor_x * layout.display_scale
        display_y = frame.cursor_y * layout.display_scale_y
        frames.append(
            {
                "index": frame.index,
                "kind": frame.kind,
                "chapter": frame.chapter,
                "caption": frame.caption,
                "keys": list(frame.keys),
                "text": frame.text,
                "handled": frame.handled,
                "note": frame.note,
                "cursor": {"x": frame.cursor_x, "y": frame.cursor_y},
                "transformed": {
                    "editor_x": round(display_x, 3),
                    "editor_y": round(display_y, 3),
                    "canvas_x": round(MARGIN + display_x, 3),
                    "canvas_y": round(layout.header_height + display_y, 3),
                    "scale_x": layout.display_scale,
                    "scale_y": layout.display_scale_y,
                },
                "click_cue": frame.kind in ("mouse_down", "mouse_up"),
                "key_cap": frame.kind in KEY_KINDS,
                "hold_seconds": frame.hold_seconds,
                "observed_wall_ms": frame.observed_wall_ms,
                "movie_start_seconds": round(frame.start_seconds, 6),
                "movie_frames": count,
                "source_file": str(frame.source),
                "source_sha256": frame.source_sha256,
                "width": frame.width,
                "height": frame.height,
                "rendered_file": str((frames_dir / f"step-{frame.index:04d}.png").resolve()),
            }
        )
    return {
        "schema": PROOF_SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "run": str(run),
        "scene": scene.scene,
        "title": scene.title,
        "movie": str(movie_path),
        "movie_sha256": output["sha256"],
        "movie_bytes": output["bytes"],
        "capture_report": str(scene.report_path),
        "capture_report_sha256": scene.report_sha256,
        "logical_viewport": {
            "width": scene.logical_width,
            "height": scene.logical_height,
        },
        "provenance": scene.capture,
        "composition": {
            "editor_display_width": layout.editor_width,
            "editor_display_height": layout.editor_height,
            "canvas_width": layout.canvas_width,
            "canvas_height": layout.canvas_height,
            "image_scale": layout.image_scale,
            "display_scale_x": layout.display_scale,
            "display_scale_y": layout.display_scale_y,
            "pointer": "arrow drawn at each frame's journaled cursor",
            "click_cue": "ring drawn only on mouse_down/mouse_up journal frames",
            "key_hud": "key caps and typed text drawn only from journal fields",
            "chapters": "journal chapter text, never inferred from image diffs",
        },
        "presentation_seconds": scene.presentation_seconds,
        "observed_wall_ms_total": scene.observed_wall_total_ms,
        "output": output,
        "chapter_timeline": _chapter_timeline(scene, counts),
        "final_state": scene.final_state,
        "frames": frames,
        "input_summary": scene.input_summary,
        "warnings": scene.warnings,
    }


def _index_entry(proof: dict[str, Any], proof_path: Path) -> dict[str, Any]:
    output = proof.get("output") or {}
    return {
        "scene": proof.get("scene"),
        "title": proof.get("title"),
        "movie": proof.get("movie"),
        "movie_sha256": proof.get("movie_sha256"),
        "movie_bytes": proof.get("movie_bytes"),
        "capture_report": proof.get("capture_report"),
        "capture_report_sha256": proof.get("capture_report_sha256"),
        "proof": str(proof_path),
        "presentation_seconds": proof.get("presentation_seconds"),
        "output": output,
        "logical_viewport": proof.get("logical_viewport"),
        "input_summary": proof.get("input_summary"),
        "final_state": proof.get("final_state"),
        "chapter_timeline": proof.get("chapter_timeline"),
        "source_frame_files": [frame.get("source_file") for frame in proof.get("frames") or []],
        "rendered_frame_files": [
            frame.get("rendered_file") for frame in proof.get("frames") or []
        ],
    }


def _scene_sort_key(scene: str) -> tuple[int, str]:
    return (SCENE_ORDER.get(scene, len(SCENES)), scene)


def write_index(run: Path, output_dir: Path) -> tuple[Path, list[dict[str, Any]]]:
    """Rebuild the index from the proofs actually on disk."""
    entries: list[dict[str, Any]] = []
    proofs: list[tuple[str, Path]] = []
    for proof_path in output_dir.glob("*.proof.json"):
        proofs.append((proof_path.name[: -len(".proof.json")], proof_path))
    for scene, proof_path in sorted(proofs, key=lambda item: _scene_sort_key(item[0])):
        proof = read_json(proof_path, f"{scene} proof")
        if not isinstance(proof, dict):
            fail(f"{scene} proof: expected a JSON object at {proof_path}")
        movie = proof.get("movie")
        if not isinstance(movie, str) or not Path(movie).is_file():
            print(
                f"warning: skipping index entry for {scene}: movie is missing at {movie!r}",
                file=sys.stderr,
            )
            continue
        entries.append(_index_entry(proof, proof_path))

    payload = {
        "schema": INDEX_SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "run": str(run),
        "scenes": entries,
        "notes": {
            "source": "Only interactive-captures/SCENE/report.json and its named PNGs were read.",
            "cues": "Pointer, click rings, key caps, typed text and chapters come from the capture journal.",
            "timing": "hold_seconds are scripted presentation holds; observed_wall_ms is retained per source frame.",
            "encoding": "H.264 yuv420p, 30 fps CFR, faststart, explicit -t equal to the hold sum.",
        },
    }
    index_path = output_dir / "index.json"
    atomic_write_json(index_path, payload)
    atomic_write_json(output_dir / "video-index.json", entries)
    return index_path, entries


# ---------------------------------------------------------------------------
# Public entry point


def _discovered_scenes(run: Path) -> list[str]:
    captures = run / "interactive-captures"
    if not captures.is_dir():
        fail(
            f"no interactive-captures directory under {run}; record the demo scenes "
            "before rendering them"
        )
    available = {
        entry.name
        for entry in captures.iterdir()
        if entry.is_dir()
        and SCENE_PATTERN.fullmatch(entry.name)
        and (entry / "report.json").is_file()
    }
    if not available:
        fail(f"no recorded scenes with report.json under {captures}")
    ordered: list[str] = []
    for name in _run_metadata_scenes(run):
        if name in available and name not in ordered:
            ordered.append(name)
    for name in sorted(available, key=_scene_sort_key):
        if name not in ordered:
            ordered.append(name)
    return ordered


def _run_metadata_scenes(run: Path) -> list[str]:
    run_json = run / "run.json"
    if not run_json.is_file():
        return []
    metadata = read_json(run_json, "run.json")
    if not isinstance(metadata, dict):
        return []
    value = metadata.get("scenes")
    names: list[str] = []
    if isinstance(value, list):
        for item in value:
            name = item
            if isinstance(item, dict):
                name = item.get("scene") or item.get("name")
            if isinstance(name, str) and SCENE_PATTERN.fullmatch(name.strip()):
                names.append(name.strip())
    elif isinstance(value, dict):
        for key, item in value.items():
            name = item.get("scene") if isinstance(item, dict) else None
            if not isinstance(name, str):
                name = key
            if isinstance(name, str) and SCENE_PATTERN.fullmatch(name.strip()):
                names.append(name.strip())
    return names


def _selected_scenes(run: Path, scenes: Sequence[str] | str | None) -> list[str]:
    if scenes is None:
        return _discovered_scenes(run)
    if isinstance(scenes, str):
        scenes = [scenes]
    names: list[str] = []
    for value in scenes:
        for part in str(value).split(","):
            name = part.strip()
            if not name:
                continue
            if not SCENE_PATTERN.fullmatch(name):
                fail(f"invalid scene name {name!r}")
            if name not in names:
                names.append(name)
    if not names:
        fail("no scenes were requested")
    return names


def render_run(
    run: Path,
    scenes: list[str] | None = None,
    *,
    ffmpeg: str = "ffmpeg",
    ffprobe: str = "ffprobe",
    font: Path | None = None,
    mono_font: Path | None = None,
) -> dict[str, Any]:
    """Render recorded scenes into H.264 movies, proofs and an index.

    Every capture is verified before any output is created.  Only this scene's
    own derived artifacts are replaced; other scenes' movies and proofs on
    disk are preserved and re-indexed.
    """
    run = Path(run)
    if not run.is_dir():
        fail(f"run directory does not exist: {run}")
    run = run.resolve()
    names = _selected_scenes(run, scenes)
    verified = [verify_capture(run, name) for name in names]

    ffmpeg_executable = _resolve_tool(ffmpeg, "ffmpeg")
    ffprobe_executable = _resolve_tool(ffprobe, "ffprobe")

    global _FONTS
    sans = _resolve_font(font, DEFAULT_SANS_FONTS, "sans-serif")
    if mono_font is not None:
        mono = _resolve_font(mono_font, (), "monospace")
    elif font is not None:
        mono = sans
    else:
        mono = _resolve_font(None, DEFAULT_MONO_FONTS + DEFAULT_SANS_FONTS, "monospace")
    _FONTS = (sans, mono)
    _imaging()

    output_dir = run / "interactive-videos"
    frames_root = output_dir / "frames"
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
        frames_root.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        fail(f"could not create the render output directory {output_dir}: {error}")

    rendered_entries: list[dict[str, Any]] = []
    for scene_record in verified:
        scene = _scene_from_verified(scene_record)
        for warning in scene.warnings:
            print(f"warning: {scene.scene}: {warning}", file=sys.stderr)
        canonical_size = (scene.frames[0].width, scene.frames[0].height)
        layout = build_layout(scene, canonical_size)
        movie_path = output_dir / f"{scene.scene}.mp4"
        frames_dir = frames_root / scene.scene
        counts = _frame_counts(
            [
                {"index": frame.index, "hold_seconds": frame.hold_seconds}
                for frame in scene.frames
            ]
        )
        output = encode_scene(
            scene,
            layout,
            counts,
            frames_dir,
            movie_path,
            ffmpeg=ffmpeg_executable,
            ffprobe=ffprobe_executable,
        )
        proof_path = output_dir / f"{scene.scene}.proof.json"
        payload = _proof_payload(run, scene, layout, counts, output, movie_path, frames_dir)
        atomic_write_json(proof_path, payload)
        rendered_entries.append(_index_entry(payload, proof_path))
        print(
            f"rendered {scene.scene}: {output['width']}x{output['height']}, "
            f"{output['duration_seconds']:.3f}s, {output['frames']} frames",
            file=sys.stderr,
        )

    index_path, entries = write_index(run, output_dir)
    return {
        "run": str(run),
        "index": str(index_path),
        "video_index": str(output_dir / "video-index.json"),
        "scenes": entries,
        "rendered": [entry["scene"] for entry in rendered_entries],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Compose genuine headless-Zed capture journals into journal-driven "
            "H.264 movies with visible pointer, click and key cues."
        )
    )
    parser.add_argument("run", type=Path, help="run directory created by zed_demo.py prepare/record")
    parser.add_argument(
        "--scene",
        action="append",
        default=None,
        metavar="SCENE",
        help="render only this scene (repeatable or comma-separated); default: every recorded scene",
    )
    parser.add_argument("--ffmpeg", default="ffmpeg", metavar="EXE", help="ffmpeg executable")
    parser.add_argument("--ffprobe", default="ffprobe", metavar="EXE", help="ffprobe executable")
    parser.add_argument("--font", type=Path, default=None, metavar="PATH", help="sans-serif font file")
    parser.add_argument("--mono-font", type=Path, default=None, metavar="PATH", help="monospace font file")
    arguments = parser.parse_args(argv)
    try:
        result = render_run(
            arguments.run,
            arguments.scene,
            ffmpeg=arguments.ffmpeg,
            ffprobe=arguments.ffprobe,
            font=arguments.font,
            mono_font=arguments.mono_font,
        )
    except RenderError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
