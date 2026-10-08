#!/usr/bin/env python3
"""Run real headless Neovim against an already-built native bend2-lsp (POSIX)."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile


# This deterministic executable models only the external Bend compiler boundary.
# The editor, LSP transport, server, indexing, staging, and diagnostics are real.
COMPILER = r'''
import json
import os
from pathlib import Path
import sys

arguments = sys.argv[1:]
log = Path(os.environ["E2E_LOG_DIR"])
record = {"arguments": arguments}
if arguments == ["version"]:
    print("bend 2.0.99")
elif arguments == ["--help"]:
    print("usage: bend <file.bend> --check-only; bend base")
elif arguments == ["base"]:
    print("def builtin(value: U32) -> U32:\n  value")
elif len(arguments) == 2 and arguments[1] == "--check-only":
    entry = Path(arguments[0])
    source = entry.read_text()
    record.update(path=str(entry), source=source)
    workspace = Path(os.environ["E2E_WORKSPACE"])
    if entry.parent.resolve() == workspace.resolve():
        raise RuntimeError("compiler received user workspace instead of isolated staging")
    if "Dep.clamp(1)" in source:
        dependency = (entry.parent / "dep.bend").read_text()
        record["dependency"] = dependency
        if "def clamp(value: U32) -> U32:" not in dependency:
            raise RuntimeError("compiler did not receive unsaved dependency buffer")
    with (log / "compiler.jsonl").open("a") as output:
        output.write(json.dumps(record) + "\n")
    for number, line in enumerate(source.splitlines(), 1):
        if line == "  missing_first":
            print(f"Error:\n- expected : a defined name\n- observed : missing_first\nContext:\n- value : U32\nLocation: main\n{number}>| {line}\n  |   ^^^^^^^^^^^^^", file=sys.stderr)
            sys.exit(1)
else:
    raise RuntimeError(f"unexpected compiler invocation: {arguments!r}")
if not (len(arguments) == 2 and arguments[1] == "--check-only"):
    with (log / "compiler.jsonl").open("a") as output:
        output.write(json.dumps(record) + "\n")
'''


def executable(value: str) -> Path:
    found = shutil.which(value)
    path = Path(found if found else value).resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"not an executable file: {path}")
    return path


def kill_owned_group(process: subprocess.Popen) -> None:
    # The editor and all its children share this owned session; Lua sets
    # detached=false for the native server. Kill leftovers even after success.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def run(arguments: argparse.Namespace) -> None:
    if os.name != "posix":
        raise ValueError("Neovim E2E currently requires POSIX process groups (Linux/macOS)")
    nvim = executable(arguments.nvim)
    binary = executable(arguments.binary)
    logs = arguments.log_dir.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    script = Path(__file__).with_suffix(".lua").resolve()
    with tempfile.TemporaryDirectory(prefix="bend2-neovim-e2e-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace with spaces"
        workspace.mkdir()
        tools = root / "bin"
        tools.mkdir()
        compiler = tools / "bend"
        compiler.write_text(f"#!{sys.executable}\n" + COMPILER)
        compiler.chmod(0o755)
        for name in ("home", "config", "data", "state", "cache", "tmp"):
            (root / name).mkdir()
        (workspace / "dep.bend").write_text("def stale(value: U32) -> U32:\n  value\n")
        (workspace / "main.bend").write_text(
            "import Base\nimport ./dep.bend as Dep\n"
            "type Shape is Data:\n  Circle{}\n  Square{}\n"
            "def local_helper(value: U32) -> U32:\n  value\n"
            "def main() -> U32:\n  Dep.clamp(1)\n"
        )
        # Deliberately no inherited PATH, NVIM_*, VIM*, BEND_*, XDG_*, LUA_*,
        # PYTHON_*, loader injection, or user profile/configuration variables.
        environment = {
            "HOME": str(root / "home"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_STATE_HOME": str(root / "state"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "XDG_CONFIG_DIRS": str(root / "config"),
            "XDG_DATA_DIRS": str(root / "data"),
            "TMPDIR": str(root / "tmp"),
            "PATH": str(tools),
            "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TERM": "dumb",
            "E2E_BINARY": str(binary), "E2E_WORKSPACE": str(workspace),
            "E2E_LOG_DIR": str(logs),
        }
        version = subprocess.run([str(nvim), "--version"], env=environment,
                                 capture_output=True, text=True, timeout=10, check=True)
        (logs / "editor-version.txt").write_text(version.stdout)
        (logs / "compiler.jsonl").write_text("")
        print(version.stdout.splitlines()[0], flush=True)
        command = [str(nvim), "--headless", "-u", "NONE", "-i", "NONE", "-n", "-l", str(script)]
        (logs / "run.json").write_text(json.dumps({
            "command": command, "binary": str(binary), "platform": sys.platform,
            "compiler": "controlled external-process fixture; not real Bend compiler",
        }, indent=2) + "\n")
        with (logs / "editor.stdout").open("w") as stdout, (logs / "editor.stderr").open("w") as stderr:
            process = subprocess.Popen(command, cwd=workspace, env=environment,
                                       stdout=stdout, stderr=stderr, start_new_session=True)
            try:
                code = process.wait(timeout=arguments.timeout)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(f"Neovim E2E timed out after {arguments.timeout}s") from error
            finally:
                kill_owned_group(process)
                native_log_path = logs / "native-log-path.txt"
                if native_log_path.exists():
                    native_log = Path(native_log_path.read_text())
                    if native_log.is_file() and native_log.is_relative_to(root):
                        shutil.copyfile(native_log, logs / "lsp.log")
        stdout_text = (logs / "editor.stdout").read_text()
        stderr_text = (logs / "editor.stderr").read_text()
        lsp_log = logs / "lsp.log"
        transcript = stdout_text + stderr_text + (lsp_log.read_text() if lsp_log.exists() else "")
        if code != 0:
            raise RuntimeError(f"Neovim E2E exited {code}:\n{stderr_text}")
        if re.search(r"panicked at|thread .* panicked|Error executing|stack traceback|\bE\d{2,}:", transcript):
            raise RuntimeError(f"editor/server error in E2E transcript:\n{transcript}")
        report = json.loads((logs / "result.json").read_text())
        if report.get("status") != "passed" or report.get("scenarios") != ["default", "explicit-dynamic"]:
            raise RuntimeError(f"missing successful editor scenarios: {report}")
        records = [json.loads(line) for line in (logs / "compiler.jsonl").read_text().splitlines()]
        staged = [record for record in records if "dependency" in record]
        if not staged or not all("def clamp(value: U32) -> U32:" in record["dependency"] for record in staged):
            raise RuntimeError("compiler fixture did not observe the unsaved dependency in staged input")
        if (workspace / "dep.bend").read_text() != "def stale(value: U32) -> U32:\n  value\n":
            raise RuntimeError("E2E unexpectedly saved an unsaved dependency")
    print(f"Real Neovim E2E passed (default and explicit dynamic capabilities); logs: {logs}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nvim", default="nvim", help="explicit editor executable, or nvim on PATH")
    parser.add_argument("--binary", default="target/debug/bend2-lsp", help="already-built native LSP executable")
    parser.add_argument("--log-dir", type=Path, default=Path("target/neovim-e2e"))
    parser.add_argument("--timeout", type=int, default=90, help="whole-editor deadline in seconds")
    arguments = parser.parse_args()
    if arguments.timeout <= 0:
        parser.error("--timeout must be positive")
    try:
        run(arguments)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"Neovim E2E failed: {error}\nLogs: {arguments.log_dir.absolute()}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
