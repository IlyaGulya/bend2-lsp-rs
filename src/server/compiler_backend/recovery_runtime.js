// Faithfully migrated native Book reconstruction and bounded parser projection.
// Boundary additions bind immutable bytes and positions; they do not check terms.
import { createHash as recovery_create_hash } from "node:crypto";
var recovery_requested = new Map;
var recovery_captured = new Map;
var recovery_positions = new Map;
var recovery_library_roots = [];
function recovery_initialize() {
  for (const source of recovery_request.sources) {
    const real = fs.realpathSync(source.path);
    if (recovery_requested.has(real)) {
      throw new Error("Captured source has ambiguous canonical ownership: " + source.path);
    }
    recovery_requested.set(real, source);
  }
  recovery_library_roots.push(fs.realpathSync(BEND_DIR));
  if (fs.existsSync(BEND_LIB)) recovery_library_roots.push(fs.realpathSync(BEND_LIB));
}
function recovery_position(real, offset) {
  const raw = recovery_sources.get(real), starts = recovery_positions.get(real);
  if (typeof raw !== "string" || !starts || !Number.isInteger(offset) || offset < 0 || offset > raw.length) {
    throw new Error("Invalid original-source diagnostic offset");
  }
  if (offset > 0 && offset < raw.length &&
      raw.charCodeAt(offset - 1) >= 0xd800 && raw.charCodeAt(offset - 1) <= 0xdbff &&
      raw.charCodeAt(offset) >= 0xdc00 && raw.charCodeAt(offset) <= 0xdfff) {
    throw new Error("Native diagnostic splits an original-source UTF16 surrogate pair");
  }
  let lo = 0, hi = starts.length;
  while (lo < hi) {
    const mid = lo + Math.floor((hi - lo) / 2);
    if (starts[mid] <= offset) lo = mid + 1;
    else hi = mid;
  }
  const line = lo - 1;
  let end = (starts[line + 1] ?? raw.length + 1) - 1;
  if (starts[line + 1] !== undefined && end > starts[line] && raw.charCodeAt(end - 1) === 13) end--;
  const character = Math.min(offset, end) - starts[line];
  if (line > 0xffffffff || character > 0xffffffff) throw new Error("Native diagnostic position exceeds contract-v1 limits");
  return { line, character };
}
function recovery_wire_diagnostic(item) {
  const captured = recovery_captured.get(item.file);
  if (!captured) throw new Error("Native diagnostic source is absent from immutable user indexes: " + item.file);
  if (typeof item.message !== "string" || item.message.length === 0 ||
      item.definition !== null && typeof item.definition !== "string") {
    throw new Error("Invalid native diagnostic message/definition");
  }
  let range = null;
  if (item.span_origin !== "unavailable") {
    if (!Number.isInteger(item.begin) || !Number.isInteger(item.end) || item.end < item.begin) {
      throw new Error("Invalid native diagnostic range");
    }
    range = { start: recovery_position(item.file, item.begin), end: recovery_position(item.file, item.end) };
  } else if (item.begin !== null || item.end !== null) throw new Error("Unavailable diagnostic has offsets");
  return { file: captured.path, definition: item.definition, range,
    span_origin: item.span_origin, message: item.message };
}
function recovery_publish(result) {
  let encoded = JSON.stringify(result);
  if (Buffer.byteLength(encoded, "utf8") > 8 * 1024 * 1024) {
    result = { format_version: 1, kind: "check", request_id: recovery_request.request_id,
      status: "backend-failure", compiler: recovery_identity, sources: [], diagnostics: [],
      coverage: { declarations: [], checked: [], failed: [], blocked: [], loading_complete: false },
      error: "Native recovery result exceeded the 8MiB boundary limit" };
    encoded = JSON.stringify(result);
    if (Buffer.byteLength(encoded, "utf8") > 8 * 1024 * 1024) process.exit(78);
  }
  fs4.writeFileSync(process.env.BEND_RECOVERY_RESULT, encoded, { flag: "wx" });
  process.exit(result.status === "checked" ? 0 : result.status === "source-error" ? 1 : 78);
}
var recovery_files = new Set;
var recovery_sources = new Map;
var recovery_masks = new Map;
var recovery_masked_sources = new Map;
var recovery_boundaries = new Map;
var recovery_parse_errors = [];
var recovery_rejected = new Set;
var recovery_constructor_owners = new Map;
var recovery_incomplete_sources = new Set;
var recovery_parser_current = null;
var recovery_parser_blocked = new Map;
var recovery_unfilled_laws = new Set;
function recovery_source(real) {
  // Replays parse the same immutable source text, never shared AST/closures.
  if (!recovery_sources.has(real)) {
    const requested = recovery_requested.get(real);
    if (!requested && !recovery_library_roots.some(root => real.startsWith(root + path.sep))) {
      throw new Error("Native loader reached a source outside immutable staging/library ownership: " + real);
    }
    const bytes = fs.readFileSync(real);
    const digest = requested ? recovery_create_hash("sha256").update(bytes).digest("hex") : null;
    if (requested && digest !== requested.sha256) throw new Error("Immutable source digest mismatch: " + requested.path);
    const raw = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
    const starts = [0];
    for (let i = 0; i < raw.length; i++) if (raw.charCodeAt(i) === 10) starts.push(i + 1);
    recovery_sources.set(real, raw);
    recovery_positions.set(real, starts);
    if (requested) recovery_captured.set(real, { path: requested.path, sha256: digest });
  }
  const ranges = recovery_masks.get(real);
  const cached = recovery_masked_sources.get(real);
  // Masks only append during reconstruction. Reuse immutable text, never ASTs;
  // apply only newly rejected ranges instead of remasking the original source.
  let text = cached?.text ?? recovery_sources.get(real);
  for (let i = cached?.count ?? 0; i < (ranges?.length ?? 0); i++) {
    const [beg, end] = ranges[i];
    text = text.slice(0, beg) + text.slice(beg, end).replace(/[^\r\n]/g, " ") + text.slice(end);
  }
  if (!cached || cached.count !== (ranges?.length ?? 0)) {
    recovery_masked_sources.set(real, { text, count: ranges?.length ?? 0 });
  }
  return text;
}
function recovery_starts(real, raw) {
  // Original sources remain immutable throughout this run; masks do not change
  // declaration boundaries or the conservative handling of quoted declarations.
  const cached = recovery_boundaries.get(real);
  if (cached) return cached;
  const starts = [], topLevelStarts = [];
  let offset = 0, quote = null;
  for (const line of raw.split("\n")) {
    if (quote === null && /^[ \t]*(?:@unsafe[ \t]+)?(?:def|type|law)[ \t]/.test(line)) {
      starts.push(offset);
      if (/^(?:@unsafe[ \t]+)?(?:def|type|law)[ \t]/.test(line)) topLevelStarts.push(offset);
    }
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (quote !== null) {
        if (c === "\\") i++;
        else if (c === quote) quote = null;
      } else if (c === "#") break;
      else if (c === '"' || c === "'") quote = c;
    }
    offset += line.length + 1;
  }
  const boundaries = { starts, topLevelStarts };
  recovery_boundaries.set(real, boundaries);
  return boundaries;
}
// This is a parser-only projection, not a native Book: it has no terms, cells,
// closures, constructor types or template cache. Only completed native parser
// publications contribute immutable primitive shapes.
function recovery_shape(t) {
  if (t.$ === "ADT") return Object.freeze({ $: t.$, n: t.n, g: t.g,
    c: Object.freeze(t.c.map(c => Object.freeze({ k: c.k, n: c.n }))) });
  if (t.$ === "Def") return Object.freeze({ $: t.$, n: t.n, x: t.x,
    v: t.v === null ? null : true, b: t.b === true, i: !!t.i, u: !!t.u, m: t.m });
  throw new Error("Unsupported native parser shape");
}
function recovery_parser_tables(book) {
  const reads = book.recovery_reads ??= { tlds: new Set, ctrs: new Set };
  book.tlds = new Proxy(book.tlds, { get(target, key) {
    if (recovery_parser_current && key !== recovery_parser_current.recovery_name &&
        (recovery_rejected.has(key) || recovery_parser_blocked.has(key))) throw new Recovery_Blocked(key);
    if (recovery_parser_current && key !== recovery_parser_current.recovery_name) {
      reads.tlds.add(key);
    }
    // Native law filling mutates its Def. Give each preflight its own primitive
    // copy; inherited immutable metadata must never be changed by the parser.
    const value = Reflect.get(target, key);
    if (book.recovery_preflight && value?.$ === "Def" && value.v === null &&
        value.b !== true && !value.i && !Object.hasOwn(target, key)) {
      const copy = { ...value };
      Object.defineProperty(target, key, { value: copy, writable: true, enumerable: true, configurable: true });
      return copy;
    }
    return value;
  }});
  book.ctrs = new Proxy(book.ctrs, { get(target, key) {
    const owner = recovery_constructor_owners.get(key);
    if (recovery_parser_current) reads.ctrs.add(key);
    if (recovery_parser_current && (recovery_rejected.has(owner) || recovery_parser_blocked.has(owner))) throw new Recovery_Blocked(owner);
    return Reflect.get(target, key);
  }});
}
function recovery_prefix_invalidated(reads, name) {
  if (reads?.tlds.has(name)) return true;
  for (const [key, owner] of recovery_constructor_owners) {
    if (owner === name && reads?.ctrs.has(key)) return true;
  }
  return false;
}
function recovery_batch_dependencies(book, p, starts, next, raw) {
  // A newly blocked name may invalidate an earlier successful prefix on the
  // next cold restart. Do not resolve later names against that stale prefix.
  const reads = book.recovery_reads;
  if (recovery_prefix_invalidated(reads, p.recovery_name)) return;
  const maskBegin = starts[next - 1], maskEnd = starts[next] ?? raw.length;
  if ((book.recovery_declarations ?? []).some(d => d.file === p.recovery_path &&
      maskBegin <= d.begin && d.begin < maskEnd)) return;
  const shapes = Object.create(null), constructors = Object.create(null);
  for (const k of book.order) {
    if (Object.hasOwn(shapes, k)) continue;
    const native = Object.getOwnPropertyDescriptor(book.tlds, k)?.value;
    // Only completed order entries qualify. A current law fill may have dirtied
    // its prior Def; use its pre-fill primitive publication, never those fields.
    const shape = (k === p.recovery_name ? book.recovery_law_shapes?.get(k) : null) ?? recovery_shape(native);
    shapes[k] = native?.b === true && shape.$ === "Def" ? Object.freeze({ ...shape, b: true }) : shape;
    if (shape.$ === "ADT") for (const c of shape.c) constructors[c.k] = c;
  }
  const aliases = Object.freeze(Object.assign(Object.create(null), p.al));
  for (let i = next; i < starts.length; i++) {
    const begin = starts[i], end = starts[i + 1] ?? raw.length;
    // The recovery boundary scanner is intentionally conservative, not a
    // semantic parser. Indented/decorated split boundaries need a cold replay.
    if (!/^(?:@unsafe[ \t]+)?(?:def|type|law)[ \t]/.test(raw.slice(begin, end))) return;
    if ((recovery_masks.get(p.recovery_path) ?? []).some(([b, e]) => b === begin && e === end)) continue;
    const probe = { recovery_preflight: true, tlds: Object.create(shapes),
      ctrs: Object.create(constructors), order: [], hols: 0, recovery_reads: reads };
    recovery_parser_tables(probe);
    // Native positions/columns use the original masked string, without copying
    // and rescanning its prefix. Native term lookahead is otherwise unchanged.
    try {
      parse_book(probe, p.dir, p.str, p.ns, aliases, p.recovery_path, raw, begin, end);
      // Native lookahead may skip masked declarations as trivia. Accept only
      // that exact native skip; a real token past the boundary still stops.
      if ((probe.recovery_declarations ?? []).some(d => d.end > end)) {
        const trivia = { str: p.str, pos: end };
        parse_skip(trivia);
        if (probe.recovery_declarations.some(d => d.end > trivia.pos)) return;
      }
      for (const [k, shape] of probe.recovery_shapes ?? []) {
        shapes[k] = shape;
        if (shape.$ === "ADT") for (const c of shape.c) constructors[c.k] = c;
      }
    } catch (error) {
      // Native errors are never emitted or masked from projected state. Stop
      // here and let a fresh native reconstruction establish the diagnostic.
      if (!(error instanceof Recovery_Blocked)) {
        if (error?.$ !== "Err") throw error;
        return;
      }
      const current = recovery_parser_current;
      if (!current?.recovery_name || current.recovery_begin < begin ||
          current.recovery_begin >= end || current.recovery_collision?.length) return;
      recovery_parser_blocked.set(current.recovery_name, error.dependency);
      const partial = Object.getOwnPropertyDescriptor(probe.tlds, current.recovery_name)?.value;
      if (current.recovery_law || partial?.$ === "Def" && partial.v === null) recovery_unfilled_laws.add(current.recovery_name);
      if (partial?.$ === "ADT") {
        for (const c of partial.c) recovery_constructor_owners.set(c.k, current.recovery_name);
        if (current.recovery_constructor) recovery_constructor_owners.set(current.recovery_constructor, current.recovery_name);
      }
      recovery_masks.get(p.recovery_path).push([begin, end]);
      if (recovery_prefix_invalidated(reads, current.recovery_name)) return;
      // Discard this failed projection as well. The next declaration gets fresh
      // native parser ASTs and writable tables over primitive metadata only.
    } finally {
      recovery_parser_current = null;
    }
  }
}
async function recovery_parse(root) {
  while (true) {
    const book = book_nil();
    recovery_parser_current = null;
    recovery_parser_tables(book);
    try {
      await book_load(book, root, "", new Map);
      recovery_parser_current = null;
      return book;
    } catch (error) {
      const dependencyError = error instanceof Recovery_Blocked;
      if (!dependencyError && error?.$ !== "Err") throw error;
      const p = dependencyError ? recovery_parser_current : error.spn?.file;
      recovery_parser_current = null;
      if (!p?.recovery_path) throw error;
      const raw = recovery_sources.get(p.recovery_path);
      if (p.recovery_begin === undefined) {
        // Loader errors retain the native import span. Removing a failed import
        // cannot certify any declaration in its file: its name environment is
        // incomplete, including any duplicate alias's earlier binding.
        recovery_parse_errors.push(recovery_diagnostic(error));
        recovery_incomplete_sources.add(p.recovery_path);
        const ranges = recovery_masks.get(p.recovery_path) ?? [];
        let at = 0, added = false;
        for (const line of raw.split("\n")) {
          const end = at + line.length;
          const match = /^import\s+(\S+)(?:\s+as\s+([A-Za-z_]\w*))?\s*(?:#.*)?$/.exec(line.trim());
          if (at <= error.spn.beg && error.spn.beg <= end ||
              p.recovery_import_alias && match?.[2] === p.recovery_import_alias) {
            if (!ranges.some(([b, e]) => b === at && e === end)) {
              ranges.push([at, end]); added = true;
            }
          }
          at = end + 1;
        }
        if (!added) throw error;
        recovery_masks.set(p.recovery_path, ranges);
        continue;
      }
      if (!p.recovery_name) {
        if (dependencyError) throw error;
        const diagnostic = recovery_diagnostic(error);
        const begin = p.recovery_begin;
        const end = recovery_starts(p.recovery_path, raw).topLevelStarts.find(at => at > begin) ?? raw.length;
        if (!Number.isInteger(begin) || begin < 0 || begin > error.spn.beg ||
            error.spn.end > end || end <= begin ||
            !/\S/.test(recovery_source(p.recovery_path).slice(begin, end))) throw error;
        // A native parser error between declarations has no safe declaration
        // owner. Preserve its exact source span, quarantine this file's name
        // environment, and restart a fresh Book at a quote-safe top-level edge.
        recovery_parse_errors.push(diagnostic);
        recovery_incomplete_sources.add(p.recovery_path);
        const ranges = recovery_masks.get(p.recovery_path) ?? [];
        ranges.push([begin, end]);
        recovery_masks.set(p.recovery_path, ranges);
        continue;
      }
      const boundaries = recovery_starts(p.recovery_path, raw);
      const starts = boundaries.starts;
      let lo = 0, hi = starts.length;
      while (lo < hi) {
        const mid = lo + Math.floor((hi - lo) / 2);
        if (starts[mid] <= p.recovery_begin) lo = mid + 1;
        else hi = mid;
      }
      const begin = starts[lo - 1];
      const end = starts[lo] ?? raw.length;
      if (begin === undefined || end <= begin) throw error;
      const ranges = recovery_masks.get(p.recovery_path) ?? [];
      if (ranges.some(([b, e]) => b === begin && e === end)) throw error;
      if (dependencyError) {
        recovery_parser_blocked.set(p.recovery_name, error.dependency);
      } else {
        recovery_parse_errors.push(recovery_diagnostic(error, { definition: p.recovery_name }));
        recovery_rejected.add(p.recovery_name);
      }
      const partial = book.tlds[p.recovery_name];
      if (p.recovery_law || partial?.$ === "Def" && partial.v === null) recovery_unfilled_laws.add(p.recovery_name);
      if (partial?.$ === "ADT") {
        for (const c of partial.c) recovery_constructor_owners.set(c.k, p.recovery_name);
        if (p.recovery_constructor) recovery_constructor_owners.set(p.recovery_constructor, p.recovery_name);
      }
      if (p.recovery_datatype && p.recovery_collision?.length && !partial) {
        // A conflicting datatype never published its constructors. Obtain those
        // names from a fresh isolated native parse, not the competing binding.
        const isolated = book_nil();
        const declaration = raw.slice(0, begin).replace(/[^\r\n]/g, " ") + raw.slice(begin, end);
        try { parse_book(isolated, p.dir, declaration, p.ns, p.al, p.recovery_path, raw); }
        catch (probe) { if (probe?.$ !== "Err") throw probe; }
        recovery_parser_current = null;
        const datatype = isolated.tlds[p.recovery_name];
        if (datatype?.$ === "ADT") for (const c of datatype.c) recovery_constructor_owners.set(c.k, p.recovery_name);
      }
      for (const owner of p.recovery_collision ?? []) {
        recovery_rejected.add(owner);
        const prior = book.tlds[owner];
        if (prior?.$ === "ADT") for (const c of prior.c) recovery_constructor_owners.set(c.k, owner);
      }
      ranges.push([begin, end]);
      recovery_masks.set(p.recovery_path, ranges);
      if (dependencyError) recovery_batch_dependencies(book, p, starts, lo, raw);
      // Discard the entire partial Book, including constructor tables and laws.
    }
  }
}
var recovery_current = null;
var recovery_index = -1;
class Recovery_Incomplete extends Error {}
class Recovery_Blocked extends Error {
  constructor(dependency) { super("Blocked by " + dependency); this.dependency = dependency; }
}
function recovery_record(book, p, k) {
  (book.recovery_declarations ??= []).push({
    definition: k, file: p.recovery_path, begin: p.recovery_begin, end: p.pos
  });
  const tld = book.tlds[k];
  if (book.recovery_preflight) (book.recovery_shapes ??= new Map).set(k, recovery_shape(tld));
  else if (tld.$ === "Def" && tld.v === null && !tld.i) {
    (book.recovery_law_shapes ??= new Map).set(k, recovery_shape(tld));
  }
}
function recovery_guard(book, rejected) {
  if (rejected.size === 0) return;
  const constructors = new Map(recovery_constructor_owners);
  for (const k of rejected) {
    const t = book.tlds[k];
    if (t?.$ === "ADT") for (const c of t.c) constructors.set(c.k, k);
  }
  const guard = (table, owners) => new Proxy(table, { get(target, key) {
    const owner = owners?.get(key) ?? key;
    if (rejected.has(owner)) throw new Recovery_Blocked(owner);
    return Reflect.get(target, key);
  }});
  book.tlds = guard(book.tlds);
  book.ctrs = guard(book.ctrs, constructors);
}
function recovery_dependencies(book) {
  const owners = new Map(recovery_constructor_owners);
  for (const k of book.order) {
    const t = book.tlds[k];
    if (t.$ === "ADT") for (const c of t.c) owners.set(c.k, k);
  }
  const graph = new Map;
  function collect(term, out, seen = new WeakSet) {
    if (!term || typeof term !== "object") return;
    if (seen.has(term)) return;
    seen.add(term);
    if (["Ref", "ADT", "Ctr", "Lit", "Mat"].includes(term.$)) {
      const k = owners.get(term.k) ?? term.k;
      // Parser-blocked targets are masked out of this Book, not nonexistent.
      // Keep their edges so dependents need not rediscover them in validation.
      if (book.tlds[k] !== undefined || recovery_rejected.has(k) || recovery_parser_blocked.has(k)) out.add(k);
    }
    for (const [key, value] of Object.entries(term)) {
      if (key === "s") continue; // Source File points back to its Book.
      if (Array.isArray(value)) for (const child of value) collect(child, out, seen);
      else collect(value, out, seen);
    }
  }
  for (const k of book.order) {
    const t = book.tlds[k];
    const refs = new Set;
    collect(term_lower(t.T), refs);
    if (t.v !== null && t.v !== undefined) collect(term_lower(t.v), refs);
    if (t.$ === "ADT") for (const c of t.c) collect(term_lower(c.T), refs);
    refs.delete(k);
    graph.set(k, refs);
  }
  return graph;
}
function recovery_diagnostic(error, fallback) {
  if (error?.$ !== "Err") throw error;
  const span = error.spn;
  const file = span?.file;
  const filename = file?.recovery_path ?? fallback?.file;
  if (!filename) throw new Error("Native diagnostic has no captured source file");
  if (!recovery_captured.has(filename)) {
    throw new Error("Native diagnostic source is absent from immutable user indexes: " + filename + "\n" + err_show(error));
  }
  if (span && (!Number.isInteger(span.beg) || !Number.isInteger(span.end) ||
      span.beg < 0 || span.end < span.beg || span.end > file.str.length)) {
    throw new Error("Invalid native diagnostic span");
  }
  // Show original imports, while using the compiler's own diagnostic renderer.
  const shown = span ? { ...error, spn: { ...span,
    file: { ...file, str: file.original ?? file.str } } } : error;
  return { file: filename, definition: error.def ?? fallback?.definition ?? null,
    begin: span?.beg ?? null, end: span?.end ?? null,
    span_origin: span ? "compiler-error" : "unavailable", message: err_show(shown) };
}
async function recovery_main() {
  const root = recovery_request.root;
  const diagnostics = [];
  const checked = new Set, failed = new Set, blocked = new Set;
  let names = [], graph = new Map, complete = false;
  const output = (status, error = null) => {
    const wireDiagnostics = [];
    for (const diagnostic of diagnostics) {
      try {
        wireDiagnostics.push(recovery_wire_diagnostic(diagnostic));
      } catch (boundaryError) {
        status = "backend-failure";
        error = (error ? error + "\n" : "") + String(boundaryError?.stack ?? boundaryError);
      }
    }
    if (status === "backend-failure") {
      names = [...new Set([...names, ...failed, ...blocked, ...recovery_rejected,
        ...recovery_parser_blocked.keys(), ...diagnostics.map(d => d.definition).filter(k => k !== null)])];
    }
    const coverage = { declarations: names,
      checked: names.filter(k => checked.has(k) && !failed.has(k) && !blocked.has(k)),
      failed: names.filter(k => failed.has(k)), blocked: names.filter(k => blocked.has(k) && !failed.has(k)),
      loading_complete: status !== "backend-failure" && complete };
    if (status !== "backend-failure" && coverage.checked.length + coverage.failed.length + coverage.blocked.length !== names.length) {
      status = "backend-failure"; error = "Native checking did not cover every declaration";
    }
    if (status === "checked" && (!complete || diagnostics.length || coverage.failed.length || coverage.blocked.length)) {
      status = "backend-failure"; error = "Native checking reported contradictory successful coverage";
    }
    if (status !== "backend-failure" && !recovery_captured.has(fs.realpathSync(root))) {
      status = "backend-failure"; error = "Native checking did not bind the immutable root source";
    }
    if (status === "backend-failure") coverage.loading_complete = false;
    return { format_version: 1, kind: "check", request_id: recovery_request.request_id,
      status, compiler: recovery_identity, sources: [...recovery_captured.values()].sort((a, b) => a.path.localeCompare(b.path)),
      diagnostics: wireDiagnostics, coverage, error };
  };
  const dependents = new Map, propagated = new Set;
  const propagate = () => {
    const queue = [...failed, ...blocked];
    for (let i = 0; i < queue.length; i++) {
      const dependency = queue[i];
      if (propagated.has(dependency)) continue;
      propagated.add(dependency);
      for (const k of dependents.get(dependency) ?? []) {
        if (failed.has(k) || blocked.has(k)) continue;
        blocked.add(k); checked.delete(k); queue.push(k);
      }
    }
  };
  const parse = () => recovery_parse(root);
  let original;
  try {
    recovery_initialize();
    original = await parse();
    diagnostics.push(...recovery_parse_errors);
    for (const k of recovery_rejected) failed.add(k);
    for (const k of recovery_parser_blocked.keys()) if (!failed.has(k)) blocked.add(k);
    complete = recovery_parse_errors.length === 0;
    names = [...new Set([...original.order, ...recovery_rejected, ...recovery_parser_blocked.keys()])];
    const laws = path3.join(path3.dirname(root), "LAWS.bend");
    if (path3.basename(root) === "PROOF.bend" && fs4.existsSync(laws) &&
        !recovery_files.has(fs4.realpathSync(laws))) {
      complete = false;
      for (const k of names) if (!failed.has(k)) blocked.add(k);
      diagnostics.push({ file: fs4.realpathSync(root), definition: null,
        begin: null, end: null, span_origin: "unavailable",
        message: "Error: PROOF.bend must import ./LAWS.bend" });
      return output("source-error");
    }
    graph = recovery_dependencies(original);
    for (const [k, dependency] of recovery_parser_blocked) graph.set(k, new Set([dependency]));
    for (const [k, dependencies] of graph) for (const dependency of dependencies) {
      if (!dependents.has(dependency)) dependents.set(dependency, new Set);
      dependents.get(dependency).add(k);
    }
    const order = original.order.slice();
    const final = new Map(order.map((k, i) => [k, i]));
    const locations = new Map((original.recovery_declarations ?? []).map(d => [d.definition, d]));
    for (const declaration of original.recovery_declarations ?? []) {
      if (recovery_incomplete_sources.has(declaration.file) && !failed.has(declaration.definition)) blocked.add(declaration.definition);
    }
    const todos = new Map;
    const unfinished = [...final.keys()].filter(k => {
      const t = original.tlds[k];
      return t.$ === "Def" && t.v === null && t.b !== true && !t.i;
    }).length;
    const holeCount = original.hols + unfinished;
    const todoMessage = "Error: " + holeCount + " TODO" + (holeCount === 1 ? "" : "s") +
      " found.\nThe code is incomplete, and not a valid proof yet.";
    for (const hole of original.recovery_todos ?? []) {
      const owner = (original.recovery_declarations ?? []).find(d =>
        d.file === hole.file && d.begin <= hole.begin && hole.end <= d.end);
      if (!owner) throw new Error("TODO has no captured declaration provenance");
      if (!todos.has(owner.definition)) todos.set(owner.definition, []);
      todos.get(owner.definition).push({ ...hole, definition: owner.definition,
        span_origin: "parser-declaration", message: todoMessage });
    }
    original = null; // Dependency lowering forces cells: check fresh closures.
    propagate();
    let cursor = 0;
    while (cursor < order.length) {
      while (cursor < order.length && (failed.has(order[cursor]) || blocked.has(order[cursor]))) cursor++;
      if (cursor === order.length) break;
      const book = await parse();
      if (book.order.length !== order.length || book.order.some((k, i) => k !== order[i])) {
        throw new Error("Source changed during recovery replay");
      }
      recovery_current = null; recovery_index = -1;
      try {
        // Native initialization hides all future bodies, preserving genuine
        // forward-reference errors. Accepted declarations share one native pass.
        // On replay the native checker rebuilds every surviving prefix effect.
        book_valid(book, 0, new Set([...failed, ...blocked]), (book, k, i, fin, t) => {
          if (i < cursor) return;
          if (fin && todos.has(k)) {
            diagnostics.push(...todos.get(k));
            failed.add(k);
            throw new Recovery_Incomplete;
          }
          if (fin && t.$ === "Def" && t.v === null && t.b !== true && !t.i) {
            diagnostics.push({ ...locations.get(k), span_origin: "parser-declaration",
              message: "Error: unfilled law; native compiler rejects incomplete proofs" });
            failed.add(k);
            throw new Recovery_Incomplete;
          }
          if (fin) checked.add(k);
          cursor = i + 1;
        });
        cursor = order.length;
      } catch (error) {
        const k = recovery_current, i = recovery_index;
        if (i < cursor || order[i] !== k) {
          throw new Error("Previously accepted declaration failed replay: " + k);
        }
        // No partial rollback: cells, checked terms, template caches, provisional
        // instances and law publication all belong to this now-discarded Book.
        // Only native source errors and our two sentinels permit reconstruction.
        if (error instanceof Recovery_Incomplete) {
          // The success callback already recorded this incomplete declaration.
        } else if (error instanceof Recovery_Blocked) {
          graph.get(k).add(error.dependency); blocked.add(k);
          checked.delete(k);
        } else if (error?.$ === "Err") {
          diagnostics.push(recovery_diagnostic(error, locations.get(k))); failed.add(k);
          checked.delete(k);
        } else throw error;
        cursor = i + 1;
        propagate();
      }
    }
    return output(diagnostics.length || blocked.size ? "source-error" : "checked");
  } catch (error) {
    complete = false;
    for (const diagnostic of recovery_parse_errors) if (!diagnostics.includes(diagnostic)) diagnostics.push(diagnostic);
    return output("backend-failure", String(error?.stack ?? error));
  }
}
