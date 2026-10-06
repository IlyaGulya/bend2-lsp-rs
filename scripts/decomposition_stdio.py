"""Throwaway real LSP consumer probe; no compiler-diagnostics claim."""

import argparse
import json
from pathlib import Path
import queue
import subprocess
import threading


def run(server, work, output):
    work.mkdir(parents=True, exist_ok=False)
    sources = {
        "dep.bend": "def shared(x: U32) -> U32:\r\n  x\r\ndef unused() -> U32:\r\n  0\r\n",
        "root.bend": "# Unicode fixture 😀\r\nimport ./dep.bend as Dep\r\nimport ./dep.bend as Again\r\ndef main() -> U32:\r\n  Dep.shared(1)\r\n  Again.shared(2)\r\ndef local() -> U32:\r\n  3\r\ndef caller() -> U32:\r\n  local()\r\n",
        "other.bend": "import ./dep.bend as Dep\r\ndef other() -> U32:\r\n  Dep.shared(3)\r\n",
    }
    for name, text in sources.items():
        (work / name).write_bytes(text.encode())
    uris = {name: (work / name).as_uri() for name in sources}
    messages = queue.Queue()
    stderr_path = output.with_suffix(".stderr")
    stderr_path.parent.mkdir(parents=True, exist_ok=True)
    results = {}
    with stderr_path.open("wb") as stderr:
        process = subprocess.Popen([str(server)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr)
        counter = 0

        def reader():
            try:
                while True:
                    headers = {}
                    while True:
                        row = process.stdout.readline()
                        if not row:
                            return
                        if row == b"\r\n":
                            break
                        key, value = row.decode().split(":", 1)
                        headers[key.lower()] = value.strip()
                    messages.put(json.loads(process.stdout.read(int(headers["content-length"]))))
            except Exception as error:
                messages.put(error)

        threading.Thread(target=reader, daemon=True).start()

        def emit(message):
            data = json.dumps(message).encode()
            process.stdin.write(f"Content-Length: {len(data)}\r\n\r\n".encode() + data)
            process.stdin.flush()

        def notify(method, params):
            emit({"jsonrpc": "2.0", "method": method, "params": params})

        def request(method, params=None):
            nonlocal counter
            counter += 1
            identity = counter
            message = {"jsonrpc": "2.0", "id": identity, "method": method}
            if params is not None:
                message["params"] = params
            emit(message)
            while True:
                response = messages.get(timeout=30)
                if isinstance(response, Exception):
                    raise response
                if response.get("id") == identity and "method" not in response:
                    if "error" in response:
                        raise ValueError(f"{method}: {response}")
                    return response["result"]
                if "method" in response and "id" in response:
                    items = response.get("params", {}).get("items", [])
                    emit({"jsonrpc": "2.0", "id": response["id"], "result": [None] * len(items)})

        def position(name, line, token):
            prefix = sources[name].splitlines()[line].split(token, 1)[0]
            return {"textDocument": {"uri": uris[name]},
                    "position": {"line": line, "character": len(prefix.encode("utf-16-le")) // 2 + 1}}

        def canonical(value):
            if isinstance(value, str):
                return value.replace(work.as_uri() + "/", "fixture://")
            if isinstance(value, list):
                return [canonical(item) for item in value]
            if isinstance(value, dict):
                return {canonical(key): canonical(item) for key, item in value.items() if key != "data"}
            return value

        try:
            request("initialize", {"processId": None, "rootUri": work.as_uri(), "capabilities": {}})
            notify("initialized", {})
            for name in ("root.bend", "other.bend", "dep.bend"):
                notify("textDocument/didOpen", {"textDocument": {
                    "uri": uris[name], "languageId": "bend", "version": 1, "text": sources[name]}})
            selected = position("dep.bend", 0, "shared")
            refs = request("textDocument/references", {**selected, "context": {"includeDeclaration": True}})
            expected_refs = [
                {"uri": uris["dep.bend"], "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 10}}},
                {"uri": uris["root.bend"], "range": {"start": {"line": 4, "character": 6}, "end": {"line": 4, "character": 12}}},
                {"uri": uris["root.bend"], "range": {"start": {"line": 5, "character": 8}, "end": {"line": 5, "character": 14}}},
                {"uri": uris["other.bend"], "range": {"start": {"line": 2, "character": 6}, "end": {"line": 2, "character": 12}}},
            ]
            order = lambda value: json.dumps(value, sort_keys=True)
            if sorted(refs, key=order) != sorted(expected_refs, key=order):
                raise ValueError(f"Reference ranges differ from independent fixture: {refs}")
            results["references"] = sorted(canonical(refs), key=order)
            rename = request("textDocument/rename", {**selected, "newName": "renamed"})
            expected_edits = {}
            for ref in expected_refs:
                expected_edits.setdefault(ref["uri"], []).append({"range": ref["range"], "newText": "renamed"})
            if canonical(rename) != canonical({"changes": expected_edits}):
                # URI and document order are not semantic; compare each edit set.
                changes = rename.get("changes", {})
                if set(changes) != set(expected_edits) or any(sorted(changes[key], key=order) != sorted(expected_edits[key], key=order) for key in expected_edits):
                    raise ValueError(f"Rename edits differ from independent fixture: {rename}")
            results["rename"] = {canonical(key): sorted(canonical(value), key=order) for key, value in rename["changes"].items()}
            prepared = request("textDocument/prepareCallHierarchy", selected)
            if not prepared or prepared[0]["name"] != "shared":
                raise ValueError(f"Missing actual target hierarchy item: {prepared}")
            incoming = request("callHierarchy/incomingCalls", {"item": prepared[0]})
            if sorted(item["from"]["name"] for item in incoming) != ["main", "other"]:
                raise ValueError(f"Wrong incoming callers: {incoming}")
            if sorted(len(item["fromRanges"]) for item in incoming) != [1, 2]:
                raise ValueError(f"Wrong incoming callsite groups: {incoming}")
            results["incoming"] = sorted(canonical(incoming), key=order)
            caller = request("textDocument/prepareCallHierarchy", position("root.bend", 3, "main"))
            outgoing = request("callHierarchy/outgoingCalls", {"item": caller[0]})
            if len(outgoing) != 1 or outgoing[0]["to"]["name"] != "shared" or len(outgoing[0]["fromRanges"]) != 2:
                raise ValueError(f"Wrong outgoing target/ranges: {outgoing}")
            results["outgoing"] = canonical(outgoing)
            notify("textDocument/didClose", {"textDocument": {"uri": uris["root.bend"]}})
            notify("textDocument/didOpen", {"textDocument": {
                "uri": uris["root.bend"], "languageId": "bend", "version": 1,
                "text": "def reopened() -> U32:\r\n  ?TODO\r\n"}})
            hover = request("textDocument/hover", {"textDocument": {"uri": uris["root.bend"]}, "position": {"line": 0, "character": 5}})
            if hover["contents"]["value"] != "```bend\ndef reopened() -> U32\n```":
                raise ValueError(f"Reopened epoch is stale: {hover}")
            results["reopened_hover"] = hover
            request("shutdown")
            notify("exit", {})
            process.stdin.close()
            if process.wait(timeout=10) != 0:
                raise ValueError("Server failed clean shutdown")
            output.write_text(json.dumps({"semantic_outputs": results, "exit": 0,
                                          "limit": "No compiler diagnostics proof; real stdio feature handlers only"}, indent=2) + "\n")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.server.resolve(), args.work.resolve(), args.output.resolve())


if __name__ == "__main__":
    main()
