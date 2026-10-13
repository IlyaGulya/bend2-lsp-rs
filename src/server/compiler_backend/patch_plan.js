// One patch contract for every exact identity in registry.json. Native declaration
// checking stays native; the callback only observes completed declaration effects.
export function recoveryPatches({ version, resources, helpers }) {
  const once = (id, anchor, replacement = anchor) => ({ id, anchor, replacement, expected: 1 });
  const loop = '  for (let i = 0;i < book.order.length; i++) {\n    const k = book.order[i];\n    const tld = tlds[k];';
  const adtEnd = '      continue;\n    }\n    if (i >= done) {';
  const defEnd = '    if (fin) {\n      book.tlds[k] = tld;\n    }\n  }\n}';
  const trivia = 'function parse_skip(p) {\n  const s = p.str;\n  while (p.pos < s.length) {\n    const n = s.charCodeAt(p.pos);\n    if (n === 32 || n === 10 || n === 13 || n === 9) {\n      p.pos += 1;\n      continue;\n    }\n    if (n === 35) {\n      while (p.pos < s.length && s.charCodeAt(p.pos) !== 10) {\n        p.pos += 1;\n      }\n      continue;\n    }\n    return;\n  }\n}';
  const nameFetch = '  const res = await fetch(BEND_HUB + "/name/" + nv' +
    (version === '2.0.37' ? ', { signal: AbortSignal.timeout(30000) }' : '') + ').catch(() => null);';
  return [
    once('runtime.utf8-source', '#!/usr/bin/env bun\n// @bun\n',
      '#!/usr/bin/env bun\n// Generated UTF-8 recovery module.\n'),
    once('identity.version', 'var VERSION = ' + JSON.stringify(version) + ';'),
    once('schema.book', 'return { tlds: Object.create(null), ctrs: Object.create(null), order: [], hols: 0, tmps: Object.create(null) };'),
    once('resources.directory', /^var BEND_DIR = .*;$/gm, 'var BEND_DIR = ' + JSON.stringify(resources) + ';'),
    once('offline.package', 'async function hub_get(book, sub, hash, spn) {',
      'async function hub_get(book, sub, hash, spn) { throw new Error("Native recovery requires packages already present locally");'),
    once('offline.name', nameFetch,
      '  throw new Error("Native recovery requires package names already cached locally");\n' + nameFetch),
    once('imports.offsets', '    body[i] = "";', '    body[i] = " ".repeat(lines[i].length);'),
    once('imports.provenance', 'const sp = { file: { str: text, ns, al }, beg, end: beg };',
      'const sp = { file: { str: text, ns, al, recovery_path: real, original: recovery_sources.get(real), recovery_import_alias: m?.[2] }, beg, end: beg };'),
    once('loader.source', '  const text = fs.readFileSync(real, "utf8");',
      '  const text = recovery_source(real);\n  recovery_files.add(real);'),
    once('loader.parser-call', '`), ns, al);', '`), ns, al, real, recovery_sources.get(real));'),
    once('parser.signature', 'function parse_book(book, dir, src, ns, al) {',
      'function parse_book(book, dir, src, ns, al, recovery_path, original, parse_start = 0, parse_end = src.length) {'),
    once('parser.provenance', 'const p = { book, dir, str: src, pos: 0, stk: [], frs: 0, ns, al };',
      'const p = { book, dir, str: src, pos: parse_start, stk: [], frs: 0, ns, al, recovery_path, original };'),
    // Parser guards must not survive into loader bookkeeping (Base's b flags).
    // Keep the current declaration on exceptions for native error provenance.
    once('parser.completion', '    if (p.pos >= p.str.length) {\n      return;\n    }',
      '    if (p.pos >= parse_end) {\n      recovery_parser_current = null;\n      return;\n    }'),
    once('parser.todos', '        p.book.hols += 1;',
      '        p.book.hols += 1;\n        (p.book.recovery_todos ??= []).push({ file: p.recovery_path, begin: beg, end: p.pos });'),
    once('parser.declaration', '    p.frs = 0;',
      '    p.frs = 0;\n    recovery_parser_current = p;\n    p.recovery_begin = p.pos; p.recovery_name = null; p.recovery_constructor = null; p.recovery_collision = null; p.recovery_law = false; p.recovery_datatype = false;'),
    once('parser.fresh-name', 'function parse_fresh(p, nm, tab = p.book.tlds, what = "a fresh name") {',
      'function parse_fresh(p, nm, tab = p.book.tlds, what = "a fresh name") {\n  if (tab === p.book.tlds) p.recovery_name = parse_qual(p, nm);\n  else if (tab === p.book.ctrs) p.recovery_constructor = parse_qual(p, nm);'),
    once('parser.collision', '  if (k in tab || nm in tab || a in p.al) {',
      '  if (k in tab || nm in tab || a in p.al) {\n    p.recovery_collision = tab === p.book.ctrs ? Object.keys(p.book.tlds).filter(owner => { const t = Object.getOwnPropertyDescriptor(p.book.tlds, owner).value; return t.$ === "ADT" && t.c.some(c => c.k === k || c.k === nm); }) : [...new Set([k, nm].filter(key => key in tab))];'),
    once('parser.resolution', '  const q = parse_reso(p, nm);',
      '  const q = parse_reso(p, nm);\n  p.recovery_name = q;\n  if (recovery_unfilled_laws.has(q) && (recovery_rejected.has(q) || recovery_parser_blocked.has(q))) throw new Recovery_Blocked(q);'),
    once('parser.law', '    if (parse_word(p, "law")) {', '    if (parse_word(p, "law")) {\n      p.recovery_law = true;'),
    once('parser.datatype', '    if (parse_word(p, "type")) {', '    if (parse_word(p, "type")) {\n      p.recovery_datatype = true;'),
    { id: 'parser.publication', anchor: 'book.order.push(k);', expected: 3,
      replacement: 'book.order.push(k);\n    recovery_record(book, p, k);' },
    once('parser.shape-pattern', '      if (ctr.n !== t.x.length) {'),
    once('parser.shape-quantities', '      if (tld === undefined || tld.$ !== "ADT" || tld.g === 0 || tld.g < tld.n && t.$ !== "ADT") {'),
    once('parser.shape-template', '      const x = hd?.$ === "Def" ? hd.x : 0;'),
    once('parser.shape-fill', '  return tld?.$ !== "ADT" || xs.length + tld.g !== tld.n ? xs : Array.from({ length: tld.g }, () => Qua(Lone(), s)).concat(xs);'),
    once('parser.shape-law', '  const law = tld?.$ === "Def" && tld.v === null && tld.b !== true && !tld.i ? tld : undefined;'),
    // Observe native trivia, preserving its beginning across zero-consumption
    // lookahead calls. Consuming a fresh token moves pos away from the saved end.
    once('parser.shape-trivia', trivia,
      trivia.replace('  const s = p.str;',
        '  if (p.recovery_trivia_end !== p.pos) p.recovery_trivia_begin = p.pos;\n  const s = p.str;')
        .replace('    return;\n  }\n}', '    break;\n  }\n  p.recovery_trivia_end = p.pos;\n}')),
    once('validator.signature', 'function book_valid(book, done = 0) {',
      'function book_valid(book, done = 0, recovery_skip = new Set, recovery_accept = null) {\n  if (book.recovery_preflight) throw new Error("Parser-only recovery projection cannot be validated");'),
    once('validator.loop', loop, '  recovery_guard(book, recovery_skip);\n' + loop +
      '\n    if (recovery_skip.has(k)) continue;\n    recovery_current = k; recovery_index = i;'),
    once('validator.accept-adt', adtEnd,
      '      if (recovery_accept) recovery_accept(book, k, i, fin, tld);\n' + adtEnd),
    once('validator.accept-def', defEnd,
      '    if (fin) {\n      book.tlds[k] = tld;\n    }\n    if (recovery_accept) recovery_accept(book, k, i, fin, tld);\n  }\n}'),
    once('runtime.helper', 'var main_default = PLUGIN;', helpers + '\nvar main_default = PLUGIN;'),
    once('runtime.entry', '  ua_fetch();\n  await cli();\n  process.exit();',
      '  recovery_publish(await recovery_main());'),
  ];
}

function matches(source, anchor) {
  if (typeof anchor !== 'string') {
    return [...source.matchAll(anchor)].map(match => ({ begin: match.index, end: match.index + match[0].length }));
  }
  const found = [];
  for (let at = source.indexOf(anchor); at !== -1; at = source.indexOf(anchor, at + anchor.length)) {
    found.push({ begin: at, end: at + anchor.length });
  }
  return found;
}

export function preparePatches(source, patches, identity) {
  const edits = [], ids = new Set;
  for (const patch of patches) {
    if (ids.has(patch.id)) throw new Error('Duplicate patch ID: ' + patch.id);
    ids.add(patch.id);
    const found = matches(source, patch.anchor);
    if (found.length !== patch.expected) {
      throw new Error(`Compiler patch drift [${patch.id}] Bend ${identity.version} fingerprint ${identity.fingerprint}: expected ${patch.expected} matches, found ${found.length}; no patches applied`);
    }
    for (const range of found) edits.push({ ...range, replacement: patch.replacement, id: patch.id });
  }
  edits.sort((a, b) => a.begin - b.begin);
  for (let i = 1; i < edits.length; i++) {
    if (edits[i].begin < edits[i - 1].end) throw new Error(`Overlapping compiler patches [${edits[i - 1].id}, ${edits[i].id}] Bend ${identity.version} fingerprint ${identity.fingerprint}; no patches applied`);
  }
  // Publication is deferred until every count and overlap has been validated.
  const pieces = [];
  let cursor = 0;
  for (const edit of edits) {
    pieces.push(source.slice(cursor, edit.begin), edit.replacement);
    cursor = edit.end;
  }
  pieces.push(source.slice(cursor));
  return pieces.join('');
}
