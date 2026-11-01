#!/usr/bin/env python3
"""Ownership boundaries for the optional editor build, without external tools."""

from pathlib import Path
import tempfile
import unittest

from zed_demo_build import BuildDriverError, _cache_paths, _require_empty_cache


class CacheOwnershipTests(unittest.TestCase):
    def test_an_unmarked_nonempty_directory_is_not_adopted_as_a_build_cache(self):
        for kind in ("file", "directory"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                cache = root / "user-data"
                cache.mkdir()
                occupied = cache / "notes"
                if kind == "file":
                    occupied.write_bytes(b"keep this data\n")
                else:
                    occupied.mkdir()
                    (occupied / "keep.txt").write_bytes(b"keep this data\n")
                paths = _cache_paths(cache, root / "zed-source")

                with self.assertRaises(BuildDriverError):
                    _require_empty_cache(paths)
                self.assertEqual(set(cache.iterdir()), {occupied})
                retained = occupied if kind == "file" else occupied / "keep.txt"
                self.assertEqual(retained.read_bytes(), b"keep this data\n")


if __name__ == "__main__":
    unittest.main()
