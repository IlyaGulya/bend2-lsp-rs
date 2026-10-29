#!/usr/bin/env python3
"""Offline, content-addressed test-tool installation; never installs product dependencies."""
import argparse
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import platform
import sys
import tempfile
import zipfile

from bootstrap_bend import compiler_environment, download_asset

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "tests/fixtures/semantic/compiler"
CACHE = ROOT / "target/semantic-tools"
OFFICIAL_PINS = ROOT / "scripts/bend_compiler_pins.json"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def native_worker_command(runtime, installation):
    return [runtime, str(installation / "worker.ts")]


def replace_once(source, before, after):
    if source.count(before) != 1:
        raise ValueError(f"instrumentation anchor is not unique: {before[:100]!r}")
    return source.replace(before, after)


def instrument(source):
    # Observation only: original parser/resolver decisions and Book mutation stay intact.
    # The callback is installed only during instrumented book_load, not book_valid.
    source += '\nexport let semantic_trace: ((e: any) => void) | null = null;\nexport function semantic_observer(f: ((e: any) => void) | null) { semantic_trace = f; }\n'
    source = replace_once(source, 'parse_book(book, dir, body.join("\\n"), ns, al);', 'parse_book(book, dir, body.join("\\n"), ns, al, real, text);')
    source = replace_once(source, 'export function parse_book(book: Book, dir: string, src: string, ns: string, al: Record<Name, Name>): void {', 'export function parse_book(book: Book, dir: string, src: string, ns: string, al: Record<Name, Name>, real?: string, original?: string): void {')
    source = replace_once(source, 'export type Parse = File & {', 'export type Parse = File & { semantic_project?: boolean;')
    source = replace_once(source, '  const p: Parse = { book, dir, str: src, pos: 0, stk: [], frs: 0, ns, al };', '  const p: Parse = { book, dir, str: src, pos: 0, stk: [], frs: 0, ns, al, semantic_project: real !== BASE_BEND };\n  semantic_trace?.({event:"file", p, real, original});')
    source = replace_once(source, '    p.frs = 0;\n    parse_close(p, 0);', '    p.frs = 0;\n    semantic_trace?.({event:"owner", p, pos:p.pos});\n    parse_close(p, 0);')
    source = replace_once(source, '  return parse_term_ops(p, base, base.s.beg, lvl);', '  if (p.semantic_project !== false) semantic_trace?.({event:"term", p, tm:base});\n  const semantic_out = parse_term_ops(p, base, base.s.beg, lvl);\n  if (p.semantic_project !== false) semantic_trace?.({event:"term", p, tm:semantic_out});\n  return semantic_out;')
    source = replace_once(source, '  return k;\n}\n\nexport function parse_def', '  semantic_trace?.({event:"declaration",p,k,nm,beg:p.pos-nm.length,end:p.pos,kind:tab===p.book.ctrs?"constructor":p.str.slice(0,p.pos-nm.length).trimEnd().endsWith("law")?"law":"type"});\n  return k;\n}\n\nexport function parse_def')
    source = replace_once(source, '  const q   = parse_reso(p, nm);\n  const tld', '  const semantic_beg = p.pos-nm.length;\n  const q   = parse_reso(p, nm);\n  const tld')
    source = replace_once(source, '  const un  = parse_take(p, "?") || u;', '  semantic_trace?.({event:"declaration",p,k,nm,beg:semantic_beg,end:semantic_beg+nm.length,kind:"def"});\n  const un  = parse_take(p, "?") || u;')
    source = replace_once(source, '  return { $: "PVar", k: t.k, i: parse_open(p, t.k), q: t.i < 0 ? Many() : Lone(), s: t.s };', '  const semantic_i = parse_open(p,t.k);\n  if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k:t.k,i:semantic_i,s:t.s});\n  return { $:"PVar", k:t.k, i:semantic_i, q:t.i<0?Many():Lone(), s:t.s };')
    source = replace_once(source, '    tele.push([bare ? None() : q, k, parse_open(p, k), T, s]);', '    const semantic_i = parse_open(p,k);\n    if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k,i:semantic_i,s});\n    tele.push([bare ? None() : q, k, semantic_i, T, s]);')
    source = replace_once(source, '  const k = parse_name(p);\n  parse_eat(p, ":");\n  const A = parse_term(p, 1);', '  const k = parse_name(p);\n  const semantic_s = p.semantic_project === false ? undefined : parse_span(p,p.pos-k.length);\n  parse_eat(p, ":");\n  const A = parse_term(p, 1);')
    source = replace_once(source, '  const i  = parse_open(p, k);\n  const B', '  const i  = parse_open(p, k);\n  if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k,i,s:semantic_s});\n  const B')
    source = replace_once(source, '        cls.push([all, q, c, parse_open(p, c), A, s]);', '        const semantic_i = parse_open(p,c);\n        if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k:c,i:semantic_i,s});\n        cls.push([all, q, c, semantic_i, A, s]);')
    source = replace_once(source, '          const i  = parse_open(p, c);\n          const w', '          const i  = parse_open(p, c);\n          if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k:c,i,s});\n          const w')
    source = replace_once(source, '      const ei = parse_open(p, k);\n      const P', '      const ei = parse_open(p, k);\n      if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k,i:ei,s:e0.s});\n      const P')
    source = replace_once(source, '    vs.push(ts[0]);\n    ts = [w];', '    if (p.semantic_project !== false) semantic_trace?.({event:"retain-use",p,s:w.s,k:w.k});\n    vs.push(ts[0]);\n    ts = [w];')
    source = replace_once(source, '      p.stk.push(["_", xi]);', '      p.stk.push(["_", xi]);\n      if (p.semantic_project !== false) semantic_trace?.({event:"binding",p,k:"_",i:xi,s:{file:p,beg,end:beg+1},synthetic:true});')
    source = replace_once(source, '  book.order.push(k);\n}\n\nexport function parse_book', '  book.order.push(k);\n  semantic_trace?.({event:"definition-end",p,k,has_body:def.v!==null||Boolean(def.i)});\n}\n\nexport function parse_book')
    source = replace_once(source, '    if (parse_at(p, "(") || parse_at(p, "!(")) {', '    if (parse_at(p, "(") || parse_at(p, "!(")) {\n      if (p.semantic_project !== false) semantic_trace?.({event:"call",p,tm:out,offload:parse_at(p,"!(")});')
    source = replace_once(source, '  const A     = typed ? parse_term(p, 1) : step ? parse_var(p, "Unit", t.s) : t;', '  const A     = typed ? parse_term(p, 1) : step ? parse_var(p, "Unit", t.s) : t;\n  if (typed && p.semantic_project !== false) semantic_trace?.({event:"type-origin",p,tm:A});')
    source = replace_once(source, '        const T = parse_term(p, 12);\n        parse_skip(p);', '        const T = parse_term(p, 12);\n        if (p.semantic_project !== false) semantic_trace?.({event:"type-origin",p,tm:T});\n        parse_skip(p);')
    return source


@contextmanager
def authenticated_runtime_archive(archive,artifact):
    # Keep the authenticated compressed snapshot private, as the former bytes value was.
    with Path(archive).open("rb") as source,tempfile.TemporaryFile() as snapshot:
        checksum=hashlib.sha256()
        while block:=source.read(1024*1024):
            checksum.update(block);snapshot.write(block)
        if checksum.hexdigest()!=artifact['sha256']:raise ValueError('runtime artifact checksum mismatch')
        snapshot.seek(0)
        with zipfile.ZipFile(snapshot) as package:
            yield package


def authenticate_runtime(candidate,archive,artifact):
    """Authenticate the executable against a pinned archive without retaining its bytes."""
    with authenticated_runtime_archive(archive,artifact) as package,package.open(artifact['name'][:-4]+'/bun') as stream:
        expected=hashlib.file_digest(stream,"sha256").hexdigest()
    binary=Path(shutil.which(str(candidate)) or candidate).resolve()
    if not binary.is_file() or file_digest(binary)!=expected:raise ValueError('runtime binary does not match authenticated release archive')
    return str(binary)


def runtime_path(download=False,override=None,runtime_pin=None):
    if runtime_pin is None:runtime_pin=json.loads((SOURCE/'runtime-pin.json').read_text())
    machine=platform.machine().lower()
    machine={'arm64':'arm64' if platform.system()=='Darwin' else 'aarch64','aarch64':'arm64' if platform.system()=='Darwin' else 'aarch64','amd64':'x86_64'}.get(machine,machine)
    key=platform.system().lower()+'-'+machine
    artifact=runtime_pin['artifacts'].get(key)
    if artifact is None:raise ValueError('unsupported pinned runtime platform: '+key)
    directory=CACHE/'runtime'/artifact['sha256']
    binary=directory/'bun'
    archive=directory/'runtime.zip'
    if download:
        directory.mkdir(parents=True,exist_ok=True)
        if not archive.exists():
            archive.write_bytes(download_asset("oven-sh/bun",artifact))
        with authenticated_runtime_archive(archive,artifact) as package:
            member=artifact['name'][:-4]+'/bun'
            with package.open(member) as stream:expected=hashlib.file_digest(stream,"sha256").hexdigest()
            if not binary.exists() or file_digest(binary)!=expected:
                with package.open(member) as source,binary.open("wb") as output:
                    shutil.copyfileobj(source,output,length=1024*1024)
        binary.chmod(0o755)
    if not archive.is_file():raise ValueError('pinned runtime is not installed; run semantic_install.py --download-runtime')
    return authenticate_runtime(override or binary,archive,artifact)


def validate_compiler_pin(pin):
    """Bind offline oracle bytes to the product's official release source pins."""
    official_bytes = OFFICIAL_PINS.read_bytes()
    official = json.loads(official_bytes)
    identity = {
        "id": f"bend-{official['version']}-official-content-v1",
        "compiler_version": official["version"],
        "repository": "https://github.com/bendlang/bend",
        "release_api": official["release_api"],
        "source_commit": official["source_commit"],
        "source_archive": {key: official["source"][key] for key in ("size", "sha256")},
    }
    for field, expected in identity.items():
        if pin.get(field) != expected:
            raise ValueError(f"official compiler pin mismatch: {field}")
    if set(pin["files"]) != {"bend.ts", "base.bend", "LICENSE"}:
        raise ValueError("official compiler pin mismatch: files")
    for name, expected in {
        "bend.ts": official["source"]["files"]["bend2/bend.ts"],
        "base.bend": official["resources"]["bend2/base.bend"],
    }.items():
        content = (SOURCE / name).read_bytes()
        if pin["files"][name] != expected[1] or [len(content), digest(content)] != expected:
            raise ValueError(f"compiler provenance mismatch: {name}")
    runtime_pin = json.loads((SOURCE / "runtime-pin.json").read_text())
    expected_runtime = {key: official["bun"][key] for key in ("version", "revision")}
    if pin["runtime"] != {"name": "bun", **expected_runtime} or any(
        runtime_pin.get(key) != value for key, value in {**expected_runtime, "release_api": official["bun"]["release_api"]}.items()
    ):
        raise ValueError("runtime provenance mismatch")
    return official_bytes


def install(runtime=None,check=False):
    pin = json.loads((SOURCE / "pin.json").read_text())
    official_bytes = validate_compiler_pin(pin)
    for name, expected in pin["files"].items():
        if digest((SOURCE / name).read_bytes()) != expected:
            raise ValueError(f"compiler provenance mismatch: {name}")
    runtime=runtime_path(override=runtime)
    result = subprocess.run([runtime, "--version"], capture_output=True, text=True, timeout=10, check=True,
        env=compiler_environment())
    if result.stdout.strip() != pin["runtime"]["version"]:
        raise ValueError(f"runtime pin mismatch: require Bun {pin['runtime']['version']}, got {result.stdout.strip()}")
    revision = subprocess.run([runtime, "-e", "console.log(Bun.revision)"],
        capture_output=True, text=True, timeout=10, check=True, env=compiler_environment())
    if revision.stdout.strip() != pin["runtime"]["revision"]:
        raise ValueError(f"runtime revision mismatch: require {pin['runtime']['revision']}, got {revision.stdout.strip()}")
    adapter = Path(__file__).read_bytes()
    worker = (ROOT / "scripts/semantic_worker.ts").read_bytes()
    pristine_worker = (ROOT / "scripts/semantic_pristine_worker.ts").read_bytes()
    book_stream = (ROOT / "scripts/semantic_book_stream.ts").read_bytes()
    key = digest((SOURCE / "pin.json").read_bytes() + official_bytes + adapter + worker + pristine_worker + book_stream)
    dest = CACHE / key
    if not check:dest.mkdir(parents=True, exist_ok=True)
    contents = {"bend.ts": (SOURCE / "bend.ts").read_bytes(), "base.bend": (SOURCE / "base.bend").read_bytes(), "instrumented.ts": instrument((SOURCE / "bend.ts").read_text()).encode(), "worker.ts": worker, "semantic_pristine_worker.ts": pristine_worker, "semantic_book_stream.ts": book_stream}
    for name, content in contents.items():
        path = dest / name
        if check:
            if not path.is_file() or path.read_bytes()!=content:raise ValueError('oracle installation is stale; run semantic_install.py')
        elif not path.exists() or path.read_bytes() != content:path.write_bytes(content)
    provenance = {"schema":1,"pin":pin,"pin_sha256":digest((SOURCE/'pin.json').read_bytes()),"adapter_sha256":digest(adapter),"worker_sha256":digest(worker),"pristine_worker_sha256":digest(pristine_worker),"book_stream_sha256":digest(book_stream),"runtime_pin_sha256":digest((SOURCE/'runtime-pin.json').read_bytes()),"runtime_sha256":file_digest(Path(shutil.which(runtime) or runtime).resolve()),"installation":key}
    provenance["official_pins_sha256"] = digest(official_bytes)
    recorded=dest/'provenance.json'
    if check:
        if not recorded.is_file() or json.loads(recorded.read_text())!=provenance:raise ValueError('installation provenance mismatch')
    else:recorded.write_text(json.dumps(provenance, sort_keys=True)+"\n")
    return dest, provenance


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime")
    parser.add_argument("--download-runtime",action="store_true")
    parser.add_argument("--check",action="store_true")
    parser.add_argument("--native-contracts",action="store_true",help="Exercise bounded native Book streaming and worker failure contracts")
    parser.add_argument("--report",type=Path)
    args = parser.parse_args()
    if args.download_runtime:runtime_path(download=True)
    destination, provenance = install(args.runtime,check=args.check)
    if args.native_contracts:
        subprocess.run([runtime_path(override=args.runtime),str(ROOT/'scripts/test_semantic_book_stream.ts'),str(destination),sys.executable],cwd=ROOT,check=True,
            env=compiler_environment())
    if args.report:
        args.report.parent.mkdir(parents=True,exist_ok=True)
        args.report.write_text(json.dumps({"schema":1,"status":"pass","provenance":provenance},indent=2)+"\n")
    print(destination)
