import { createHash } from 'node:crypto';
import { existsSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { preparePatches, recoveryPatches } from './patch_plan.js';

// Executed only by the configured compiler's embedded, fingerprint-pinned Bun.
// The parent owns this directory until the child has been killed and reaped.
const resultPath = process.env.BEND_RECOVERY_RESULT;
const stagingPath = process.env.BEND_RECOVERY_STAGING;
const requestPath = process.env.BEND_RECOVERY_REQUEST;
let request: Record<string, unknown> | null = null;
let identity: { version: string; fingerprint: string; runtime: { bun: string; revision: string } } | null = null;
let staging: string | null = null;
let canWriteResult = false;
function exactFields(value: unknown, fields: string[]): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) &&
    Object.keys(value).length === fields.length && fields.every(field => Object.hasOwn(value, field));
}
function inside(directory: string, filename: string): boolean {
  const child = relative(directory, filename);
  return child !== '' && child !== '..' && !child.startsWith('..' + sep) && !isAbsolute(child);
}
function absolutePath(value: unknown): value is string {
  return typeof value === 'string' && !value.includes('\0') && isAbsolute(value) && resolve(value) === value;
}
function fail(error: unknown): never {
  const result = {
    format_version: 1,
    kind: request?.mode === 'compatibility' ? 'compatibility' : 'check',
    request_id: typeof request?.request_id === 'string' ? request.request_id : '',
    status: 'backend-failure', compiler: identity,
    ...(request?.mode === 'compatibility' ? {} : { sources: [], diagnostics: [],
      coverage: { declarations: [], checked: [], failed: [], blocked: [], loading_complete: false } }),
    error: String(error instanceof Error ? error.stack ?? error.message : error),
  };
  if (canWriteResult && resultPath) {
    try {
      const encoded = JSON.stringify(result);
      if (Buffer.byteLength(encoded, 'utf8') > 8 * 1024 * 1024) throw new Error('Recovery failure result exceeds 8MiB');
      writeFileSync(resultPath, encoded, { flag: 'wx' });
    } catch (writeError) {
      console.error(result.error, String(writeError));
    }
  } else console.error(result.error);
  process.exit(78);
}
try {
  if (!absolutePath(stagingPath) || !absolutePath(resultPath) || !absolutePath(requestPath) ||
      !/^(?:\/\$bunfs\/|B:[\\/]~BUN[\\/])/.test(process.argv[1] ?? '')) {
    throw new Error('Expected parent-owned absolute staging paths and a compiled Bun entry');
  }
  staging = realpathSync(stagingPath);
  if (!statSync(staging).isDirectory() || !inside(staging, realpathSync(requestPath)) ||
      !inside(staging, join(realpathSync(dirname(resultPath)), resultPath.slice(dirname(resultPath).length + 1))) ||
      existsSync(resultPath)) {
    throw new Error('Request/result must belong to owned staging; result must not already exist');
  }
  canWriteResult = true;
  const parsed: unknown = JSON.parse(readFileSync(requestPath, 'utf8'));
  if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
    request = parsed as Record<string, unknown>; // Only an object guard; fields are validated below.
  }
  if (!exactFields(request, ['format_version', 'request_id', 'mode', 'root', 'sources']) ||
      request.format_version !== 1 || typeof request.request_id !== 'string' || request.request_id.length === 0 ||
      (request.mode !== 'compatibility' && request.mode !== 'recover') || !Array.isArray(request.sources)) {
    throw new Error('Invalid contract-v1 recovery request');
  }
  const requestedPaths = new Set<string>();
  for (const source of request.sources) {
    if (!exactFields(source, ['path', 'sha256']) || !absolutePath(source.path) ||
        typeof source.sha256 !== 'string' ||
        !/^[0-9a-f]{64}$/.test(source.sha256) || requestedPaths.has(source.path)) {
      throw new Error('Invalid or duplicate immutable source record');
    }
    requestedPaths.add(source.path);
  }
  if (request.mode === 'compatibility' ? request.root !== null || request.sources.length !== 0 :
      !absolutePath(request.root) || !requestedPaths.has(request.root)) {
    throw new Error('Recovery root must be a captured source; compatibility must contain no sources');
  }
  const source = readFileSync(process.argv[1], 'utf8');
  const fingerprint = createHash('sha256').update(source).digest('hex');
  const registry: unknown = JSON.parse(readFileSync(join(staging, 'registry.json'), 'utf8'));
  if (!exactFields(registry, ['schema', 'families', 'compilers']) ||
      registry.schema !== 2 ||
      registry.families === null || typeof registry.families !== 'object' || Array.isArray(registry.families) ||
      registry.compilers === null || typeof registry.compilers !== 'object' || Array.isArray(registry.compilers)) {
    throw new Error('Invalid compiler registry schema');
  }
  const compilers = registry.compilers as Record<string, unknown>; // Object shape proven above.
  const families = registry.families as Record<string, unknown>; // Object shape proven above.
  const compiler = compilers[fingerprint];
  if (!exactFields(compiler, ['version', 'family', 'targets']) &&
      !exactFields(compiler, ['version', 'family', 'targets', 'runtime'])) {
    throw new Error(`Unsupported embedded compiler fingerprint ${fingerprint}`);
  }
  if (typeof compiler.version !== 'string' || typeof compiler.family !== 'string' ||
      !Object.hasOwn(families, compiler.family) || !Array.isArray(compiler.targets) ||
      compiler.targets.length === 0 || compiler.targets.some(target => typeof target !== 'string')) {
    throw new Error('Invalid compiler family/schema in registry');
  }
  const runtime = compiler.runtime ?? families[compiler.family];
  if (!exactFields(runtime, ['bun', 'revision']) || typeof runtime.bun !== 'string' || typeof runtime.revision !== 'string') {
    throw new Error('Invalid native runtime registry schema');
  }
  if (!compiler.targets.includes(process.platform + '-' + process.arch) ||
      Bun.version !== runtime.bun || Bun.revision !== runtime.revision) {
    throw new Error(`Unsupported native runtime: expected ${compiler.targets.join(', ')} Bun ${runtime.bun} ${runtime.revision}; found ${process.platform}/${process.arch} Bun ${Bun.version} ${Bun.revision}`);
  }
  identity = { version: compiler.version, fingerprint, runtime: { bun: Bun.version, revision: Bun.revision } };
  const resources = join(dirname(realpathSync(process.execPath)), '..', 'bend2');
  try {
    if (!statSync(resources).isDirectory()) throw new Error('Not a directory');
  } catch (error) {
    throw new Error(`Compiler patch drift [resources.directory]: unavailable native resource directory ${resources}: ${String(error)}; no patches applied`);
  }
  // Serialize immutable boundary facts, not inherited environment or mutable ASTs.
  const helpers = 'var recovery_request = ' + JSON.stringify(request) + ';\n' +
    'var recovery_identity = ' + JSON.stringify(identity) + ';\n' +
    readFileSync(join(staging, 'recovery_runtime.js'), 'utf8');
  const patched = preparePatches(source, recoveryPatches({ version: compiler.version, resources, helpers }), identity);
  if (request.mode === 'compatibility') {
    const encoded = JSON.stringify({ format_version: 1, kind: 'compatibility',
      request_id: request.request_id, status: 'checked', compiler: identity, error: null });
    if (Buffer.byteLength(encoded, 'utf8') > 8 * 1024 * 1024) throw new Error('Compatibility result exceeds 8MiB');
    writeFileSync(resultPath, encoded, { flag: 'wx' });
    process.exit(0);
  }
  const modulePath = join(staging, 'recovery-entry.mjs');
  // The frozen patch removes Bun's bundled-ASCII directive so generated
  // request IDs and resource paths retain their original UTF-8 spelling.
  writeFileSync(modulePath, patched, { flag: 'wx' });
  // The module specifier and patched entry are generated in owned staging;
  // static imports cannot load this not-yet-published runtime module.
  await import(modulePath);
  throw new Error('Recovery entry returned without exiting');
} catch (error) {
  fail(error);
}
