#!/usr/bin/env python3
"""Exercise the capture-authority contract behind the demo renderer.

Every test here is stdlib-only and builds a throwaway run directory in a
temporary directory; the fixtures prove rejection and timeline authority, not
genuine editor evidence.  Pillow is never imported, so a stdlib-only quality
run also proves the verification path needs no optional dependency.
"""

import hashlib
import json
from pathlib import Path
import struct
import tempfile
import unittest
import zlib

from zed_demo_render import RenderError, verify_capture


SCENE = "clip1-unsaved-entry"


def png_bytes(width, height, rgb=(30, 38, 50)):
    """A structurally valid PNG with the requested dimensions."""
    row = b"\x00" + bytes(rgb) * width
    raw = row * height

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


class CaptureFixture(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.run = self.root / "run"
        self.scene_dir = self.run / "interactive-captures" / SCENE

    def write_capture(
        self,
        frames=None,
        *,
        logical=(100.0, 50.0),
        png=(200, 100),
        scale=2.0,
    ):
        """Write a minimal report; each frame spec may override journal fields."""
        self.scene_dir.mkdir(parents=True, exist_ok=True)
        specs = frames if frames is not None else [{}]
        records = []
        for index, spec in enumerate(specs, start=1):
            values = dict(spec)
            file_name = values.pop("file", f"{SCENE}-{index:02d}.png")
            if not values.pop("skip_png", False):
                (self.scene_dir / file_name).write_bytes(
                    png_bytes(*values.pop("png", png), rgb=values.pop("rgb", (30, 38, 50)))
                )
            interaction = {
                "kind": values.pop("kind", "pointer_move"),
                "cursor": values.pop("cursor", {"x": 10.0, "y": 10.0}),
                "viewport": values.pop(
                    "viewport", {"width": logical[0], "height": logical[1]}
                ),
                "keys": values.pop("keys", []),
                "text": values.pop("text", None),
                "handled": values.pop("handled", False),
                "chapter": values.pop("chapter", "The one journaled chapter"),
                "note": None,
            }
            records.append(
                {
                    "file": file_name,
                    "caption": values.pop("caption", f"Journal step {index}"),
                    "hold_seconds": values.pop("hold_seconds", 0.2),
                    "observed_wall_ms": values.pop("observed_wall_ms", 12),
                    "state": {"phase": "step", "interaction": interaction},
                    "width": values.pop("width", png[0]),
                    "height": values.pop("height", png[1]),
                    "logical_width": logical[0],
                    "logical_height": logical[1],
                    "device_scale": scale,
                }
            )
        report = {
            "scene": SCENE,
            "title": "A genuine scene",
            "capture": {
                "kind": "GPUI headless Metal render of a real windowless Workspace",
                "input_provenance": "journaled GPUI input events",
            },
            "logical_width": logical[0],
            "logical_height": logical[1],
            "device_scale": scale,
            "frames": records,
            "before": {"buffer_text": "before"},
            "after": {"buffer_text": "after"},
        }
        (self.scene_dir / "report.json").write_text(json.dumps(report), encoding="utf-8")
        return self.run

    def assert_rejected(self):
        with self.assertRaises(RenderError):
            verify_capture(self.run, SCENE)


class VerifyCaptureTests(CaptureFixture):
    def test_rejects_frame_image_reached_through_symlink_outside_scene(self):
        outside = self.root / "somewhere-else.png"
        outside.write_bytes(png_bytes(200, 100))
        self.write_capture([{"file": "linked.png", "skip_png": True}])
        (self.scene_dir / "linked.png").symlink_to(outside)
        self.assert_rejected()

    def test_rejects_unhandled_keystroke_in_journal(self):
        self.write_capture(
            [{"kind": "keystroke", "keys": ["delete"], "handled": False}]
        )
        self.assert_rejected()

    def test_rejects_pointer_frame_that_would_show_a_false_key_badge(self):
        self.write_capture([{"kind": "pointer_move", "keys": ["delete"]}])
        self.assert_rejected()

    def test_rejects_cursor_outside_the_logical_viewport(self):
        self.write_capture(
            [{"kind": "keystroke", "keys": ["enter"], "handled": True,
              "cursor": {"x": 260.0, "y": 10.0}}]
        )
        self.assert_rejected()

    def test_rejects_non_positive_hold_duration(self):
        self.write_capture([{"hold_seconds": 0.0}])
        self.assert_rejected()

    def test_rejects_frame_without_interaction_journal(self):
        run = self.write_capture([{}])
        report_path = self.scene_dir / "report.json"
        report = json.loads(report_path.read_text(encoding="utf-8"))
        report["frames"][0]["state"] = {}
        report_path.write_text(json.dumps(report), encoding="utf-8")
        with self.assertRaises(RenderError):
            verify_capture(run, SCENE)

    def test_rejects_source_image_changed_after_its_capture_was_attested(self):
        self.write_capture()
        report_path = self.scene_dir / "report.json"
        report = json.loads(report_path.read_text(encoding="utf-8"))
        image = self.scene_dir / report["frames"][0]["file"]
        report["frames"][0]["sha256"] = hashlib.sha256(image.read_bytes()).hexdigest()
        report_path.write_text(json.dumps(report), encoding="utf-8")

        image.write_bytes(png_bytes(200, 100, rgb=(200, 0, 0)))
        self.assert_rejected()

    def test_accepts_journaled_capture_and_keeps_source_order_timeline(self):
        run = self.write_capture(
            [
                {"kind": "pointer_move", "chapter": "Pointer enters"},
                {"kind": "pointer_move", "chapter": "Pointer enters"},
                {"kind": "keystroke", "keys": ["enter"], "handled": True,
                 "chapter": "Confirm the action"},
            ]
        )
        verified = verify_capture(run, SCENE)
        frames = verified["frames"]
        self.assertEqual(len(frames), 3)
        starts = [frame["start_seconds"] for frame in frames]
        self.assertEqual(starts[0], 0.0)
        self.assertTrue(all(later > earlier for earlier, later in zip(starts, starts[1:])))
        self.assertEqual(verified["chapters"], ["Pointer enters", "Confirm the action"])
        self.assertEqual(verified["final_chapter"], "Confirm the action")
        self.assertEqual(verified["final_state"]["frame"], 3)


if __name__ == "__main__":
    unittest.main()
