#!/usr/bin/env python3
"""Prepare, record and share genuine input-driven Zed demonstrations."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

from zed_demo_build import FIXTURES_DIR, FIXTURE_SCENES as SCENES
from zed_demo_process import owned_process

RUN_SCHEMA = "bend2-zed-demo/run/1"


def sha256_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def executable(value):
    expanded = os.path.expanduser(str(value))
    located = shutil.which(expanded)
    if located is None:
        raise FileNotFoundError(f"executable not found: {value}")
    path = Path(located).resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"not an executable file: {path}")
    return path


def run_file(root, relative):
    """Resolve a regular file without letting a run reach outside its owner."""
    relative = Path(relative)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError(f"run file must be relative and contained: {relative}")
    path = root / relative
    if path.resolve() != path or not path.is_file():
        raise ValueError(f"missing, symlinked or outside run file: {relative}")
    return path


def verify_baselines(run):
    """Refuse recording when any source or executable-rule setting has changed."""
    root = Path(run).expanduser().resolve()
    manifest = json.loads(run_file(root, "baselines/manifest.json").read_text(encoding="utf-8"))
    if manifest.get("schema") != "bend2-zed-demo/baselines/1":
        raise ValueError("unsupported baseline manifest")
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        raise ValueError("baseline manifest must name its captured files")
    seen = set()
    for entry in files:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise ValueError("malformed baseline file")
        relative = entry["path"]
        if relative in seen:
            raise ValueError(f"duplicate baseline file: {relative}")
        seen.add(relative)
        path = run_file(root, relative)
        if path.stat().st_size != entry.get("bytes") or sha256_file(path) != entry.get("sha256"):
            raise ValueError(f"demo input changed since prepare: {relative}")
    return {"files": len(files), "unchanged": True}


def prepare_run(run, *, lsp, lint, lint_args, compiler, lint_cache=None):
    """Create a private workspace without replacing any existing project."""
    requested = Path(run).expanduser()
    if requested.is_symlink():
        raise ValueError("the run directory must not be a symlink")
    if requested.exists() and (not requested.is_dir() or any(requested.iterdir())):
        raise FileExistsError(f"run directory must be new or empty: {requested}")
    root = requested.resolve()
    tools = {name: executable(value) for name, value in (("lsp", lsp), ("lint", lint), ("compiler", compiler))}
    lint_args = list(lint_args)
    cache = Path(lint_cache).expanduser().resolve() if lint_cache else root / "lint-cache"

    # Read the complete small fixture set before creating output, so a missing
    # package asset cannot leave an apparently prepared demonstration behind.
    fixtures = []
    for scene in SCENES:
        for source in sorted((FIXTURES_DIR / scene).iterdir()):
            fixtures.append((Path(scene) / source.name, source.read_bytes()))

    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    baseline = []
    for relative, contents in fixtures:
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(contents)
        baseline.append({"path": relative.as_posix(), "sha256": hashlib.sha256(contents).hexdigest(), "bytes": len(contents)})
    for scene in SCENES:
        settings = {
            "autosave": "off",
            "format_on_save": "off",
            "lsp": {"bend2": {
                "binary": {"path": str(tools["lsp"]), "arguments": [], "env": {"XDG_CACHE_HOME": str(cache)}},
                "settings": {
                    "bend": {"lint": {
                        "enabled": True,
                        "path": str(tools["lint"]),
                        "arguments": lint_args,
                        "dangerousCodeActions": scene == "clip3-dangerous-action",
                        "timeoutMs": 120000,
                    }},
                    "bend2-lsp": {"compilerPath": str(tools["compiler"])},
                },
            }},
        }
        relative = Path(scene) / ".zed" / "settings.json"
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        write_json(target, settings)
        baseline.append({"path": relative.as_posix(), "sha256": sha256_file(target), "bytes": target.stat().st_size})

    (root / "baselines").mkdir()
    write_json(root / "baselines" / "manifest.json", {"schema": "bend2-zed-demo/baselines/1", "files": baseline})
    manifest = {
        "schema": RUN_SCHEMA,
        "scenes": list(SCENES),
        "baseline": "baselines/manifest.json",
        "tools": {name: {"path": str(path), "sha256": sha256_file(path)} for name, path in tools.items()},
        "lint_arguments": lint_args,
        "lint_cache": str(cache),
    }
    write_json(root / "run.json", manifest)
    return {"run": str(root), "scenes": list(SCENES), "manifest": str(root / "run.json")}


def load_run(run):
    root = Path(run).expanduser().resolve()
    manifest = json.loads(run_file(root, "run.json").read_text(encoding="utf-8"))
    if manifest.get("schema") != RUN_SCHEMA or manifest.get("scenes") != list(SCENES):
        raise ValueError("not a supported prepared demo run")
    return root, manifest


def selected_scenes(manifest, scenes):
    selected = list(dict.fromkeys(scenes or manifest["scenes"]))
    if not selected or any(scene not in manifest["scenes"] for scene in selected):
        raise ValueError("unknown demo scene")
    return selected


def verify_native_report(report, scene):
    """These lint walkthroughs must not disguise an unrelated compiler failure."""
    invariants = report.get("invariants", {})
    if (report.get("scene") != scene or invariants.get("disk_files_unchanged") is not True
            or invariants.get("save_commands_invoked") is not False):
        raise ValueError(f"native recording invariants failed: {scene}")
    forbidden = {"save_commands_invoked", "linter_cli_fix_invoked"}
    if any(isinstance(value, bool) and value != (name not in forbidden)
           for name, value in invariants.items()):
        raise ValueError(f"a native behavioral invariant failed: {scene}")

    def check(value):
        if isinstance(value, dict):
            for diagnostic in value.get("diagnostics", []):
                if (diagnostic.get("severity_number") == 1
                        or diagnostic.get("severity") in ("Error", 1)):
                    raise ValueError(
                        f"unexpected error diagnostic in {scene}: "
                        f"{diagnostic.get('source')} {diagnostic.get('code')}; "
                        "inspect report.json and repair the tool installation"
                    )
            for nested in value.values():
                check(nested)
        elif isinstance(value, list):
            for nested in value:
                check(nested)

    check(report)


def record_run(run, *, zed_source, driver=None, build_cache=None, extension, scenes=None, timeout=600):
    if sys.platform != "darwin":
        raise ValueError("record requires macOS and Zed's windowless Metal renderer")
    if timeout <= 0:
        raise ValueError("record timeout must be positive")
    if (driver is None) == (build_cache is None):
        raise ValueError("choose exactly one of --driver or --build-cache")
    root, manifest = load_run(run)
    verify_baselines(root)
    selected = selected_scenes(manifest, scenes)
    for name, tool in manifest["tools"].items():
        path = executable(tool["path"])
        if sha256_file(path) != tool["sha256"]:
            raise ValueError(f"{name} changed since prepare; prepare a new run")

    extension = Path(extension).expanduser().resolve()
    for relative in ("languages/bend2/config.toml", "grammars/bend2.wasm"):
        if not (extension / relative).is_file():
            raise ValueError(f"extension is missing {relative}: {extension}")
    source = Path(zed_source).expanduser().resolve()
    build_revision = None
    if build_cache is not None:
        from zed_demo_build import BuildDriverError, build_driver

        try:
            built = build_driver(source, Path(build_cache))
        except BuildDriverError as error:
            raise ValueError(str(error)) from error
        binary, source, build_revision = built.binary, built.source, built.revision
    else:
        binary = executable(driver)
    revision = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True
    ).strip()
    if not (source / "crates" / "zed" / "Cargo.toml").is_file():
        raise ValueError("--zed-source must be a Zed checkout (needed for runtime assets)")

    from zed_demo_render import verify_capture

    environment = dict(os.environ)
    environment.pop("BEND_LIB", None)
    for variable, directory in (
        ("HOME", "home"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"),
    ):
        path = root / "profile" / directory
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        environment[variable] = str(path)
    environment.update({"ZED_STATELESS": "1", "BEND_NO_TELEMETRY": "1"})
    captures = root / "interactive-captures"
    captures.mkdir(exist_ok=True)
    recorded = []
    for scene in selected:
        output = captures / scene
        if output.exists():
            raise FileExistsError(f"capture already exists: {output}; prepare a fresh run")
        output.mkdir()
        print(f"Recording {scene}; no desktop window is opened", file=sys.stderr)
        with (output / "driver.log").open("wb") as log:
            with owned_process(
                [str(binary), scene, str(root / scene), str(output), str(extension)],
                cwd=source, env=environment, stdout=log, stderr=subprocess.STDOUT,
            ) as process:
                returncode = process.wait(timeout=timeout)
        verify_baselines(root)
        if returncode:
            raise ValueError(f"recorder failed ({returncode}); see {output / 'driver.log'}")
        report_path = output / "report.json"
        report = json.loads(run_file(root, report_path.relative_to(root)).read_text(encoding="utf-8"))
        verify_native_report(report, scene)
        shutil.copyfile(report_path, output / "raw-report.json")
        for frame in report["frames"]:
            frame_path = Path(frame["file"])
            if not frame_path.is_absolute():
                frame_path = output / frame_path
            source_path = run_file(output, frame_path.relative_to(output))
            frame["file"] = source_path.relative_to(output).as_posix()
            frame["sha256"] = sha256_file(source_path)
        write_json(report_path, report)
        verified = verify_capture(root, scene)
        receipt = {
            "schema": "bend2-zed-demo/recording/1", "scene": scene,
            "driver": {"path": str(binary), "sha256": sha256_file(binary)},
            "runtime_asset_source": str(source), "runtime_asset_revision": revision,
            "built_source_revision": build_revision,
            "raw_report_sha256": sha256_file(output / "raw-report.json"),
            "baseline": verify_baselines(root), "capture": verified,
        }
        write_json(output / "recording.json", receipt)
        recorded.append({"scene": scene, "report": str(report_path)})
    return {"run": str(root), "recordings": recorded}


def publish_run(run, *, pr, gh="gh", preview=False):
    """Upload only verified rendered artifacts, and only on explicit invocation."""
    root, manifest = load_run(run)
    verify_baselines(root)
    index = json.loads(run_file(root, "interactive-videos/index.json").read_text(encoding="utf-8"))
    entries = index.get("scenes")
    if not isinstance(entries, list) or not entries:
        raise ValueError("render the selected scenes before publishing")
    from zed_demo_render import verify_capture

    body = ["Input-driven Zed demos: real LSP, unsaved buffers and explicit fixes.",
            "Pointer/key overlays come from dispatched editor events. Clip pacing is not a latency benchmark.", ""]
    movies = []
    for entry in entries:
        scene = entry["scene"]
        selected_scenes(manifest, [scene])
        verify_capture(root, scene)
        movie = root / "interactive-videos" / f"{scene}.mp4"
        run_file(root, movie.relative_to(root))
        if movie.stat().st_size != entry["movie_bytes"] or sha256_file(movie) != entry["movie_sha256"]:
            raise ValueError(f"rendered movie changed: {scene}")
        report = run_file(root, f"interactive-captures/{scene}/report.json")
        verify_native_report(json.loads(report.read_text(encoding="utf-8")), scene)
        if sha256_file(report) != entry["capture_report_sha256"]:
            raise ValueError(f"capture changed after rendering: {scene}")
        movies.append(movie)
        body.extend((f"### {entry['title']}", "", f"![]({movie})", ""))
    body_path = root / "interactive-videos" / "pr-comment.md"
    body_path.write_text("\n".join(body), encoding="utf-8")
    command = [str(executable(gh)), "pr", "comment", pr, "--body-file", str(body_path)]
    for movie in movies:
        command.extend(("--attach", str(movie)))
    if preview:
        return {"preview": True, "body": str(body_path), "command": command}
    result = subprocess.run(command, text=True, capture_output=True, check=True)
    return {"comment": result.stdout.strip(), "movies": len(movies)}


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="create private fixtures and tool settings")
    prepare.add_argument("--run", type=Path, required=True)
    prepare.add_argument("--lsp", required=True)
    prepare.add_argument("--lint", required=True)
    prepare.add_argument("--lint-arg", action="append", default=[])
    prepare.add_argument("--compiler", required=True)
    prepare.add_argument("--lint-cache", type=Path)
    record = commands.add_parser("record", help="capture genuine input in windowless Zed (macOS)")
    record.add_argument("--run", type=Path, required=True)
    record.add_argument("--zed-source", type=Path, required=True)
    binary = record.add_mutually_exclusive_group(required=True)
    binary.add_argument("--driver")
    binary.add_argument("--build-cache", type=Path)
    record.add_argument("--extension", type=Path, required=True)
    record.add_argument("--scene", action="append", choices=SCENES)
    record.add_argument("--timeout", type=float, default=600)
    render = commands.add_parser("render", help="render pointer/key overlays and verify MP4 playback")
    render.add_argument("--run", type=Path, required=True)
    render.add_argument("--scene", action="append", choices=SCENES)
    render.add_argument("--ffmpeg", default="ffmpeg")
    render.add_argument("--ffprobe", default="ffprobe")
    render.add_argument("--font", type=Path)
    render.add_argument("--mono-font", type=Path)
    publish = commands.add_parser("publish", help="explicitly attach verified movies to a PR comment")
    publish.add_argument("--run", type=Path, required=True)
    publish.add_argument("--pr", required=True)
    publish.add_argument("--gh", default="gh")
    publish.add_argument("--preview", action="store_true", help="write the comment without uploading")
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        if args.command == "prepare":
            result = prepare_run(args.run, lsp=args.lsp, lint=args.lint, lint_args=args.lint_arg,
                                 compiler=args.compiler, lint_cache=args.lint_cache)
        elif args.command == "record":
            result = record_run(args.run, zed_source=args.zed_source, driver=args.driver,
                                build_cache=args.build_cache, extension=args.extension,
                                scenes=args.scene, timeout=args.timeout)
        elif args.command == "render":
            root, manifest = load_run(args.run)
            verify_baselines(root)
            for scene in selected_scenes(manifest, args.scene):
                report = run_file(root, f"interactive-captures/{scene}/report.json")
                verify_native_report(json.loads(report.read_text(encoding="utf-8")), scene)
            from zed_demo_render import render_run

            result = render_run(args.run, args.scene, ffmpeg=args.ffmpeg, ffprobe=args.ffprobe,
                                font=args.font, mono_font=args.mono_font)
        else:
            result = publish_run(args.run, pr=args.pr, gh=args.gh, preview=args.preview)
    except KeyboardInterrupt:
        print("recording interrupted; owned child processes stopped", file=sys.stderr)
        return 130
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
