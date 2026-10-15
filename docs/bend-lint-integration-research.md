# bend-lint integration: verified upstream contract

Source inspection, not runtime certification. The integration targets standalone
[`MattCozendey/bend-lint` at `02ef84b38b8c06cbd607e270c4930e633e9dab0f`](https://github.com/MattCozendey/bend-lint/tree/02ef84b38b8c06cbd607e270c4930e633e9dab0f).
The [npm metadata](https://registry.npmjs.org/bend-lint/0.1.0) identifies the
published standalone version as `0.1.0`. No multi-platform runtime result is
implied by inspecting sources or by adding the ignored native smoke.

## Standalone versus the older bundled tool

[Bend PR #1389](https://github.com/bendlang/bend/pull/1389) merged as
`2dbb907e14906632d793910f2253b970ec0a0cf0`; Bend
[`v2.0.37`](https://github.com/bendlang/bend/releases/tag/v2.0.37) contains the older
`tools/bend-lint` CLI. Its `--rules`/`--config` interface and `{ok,findings}` output
are not the standalone tool's protocol. In particular, do not import its
`bend/check` compiler findings or former safe-fix claims into the current API.

The standalone [README](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/README.md)
and [CLI entry](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/src/linter/seam.ts)
verify nearest-ancestor `bend-lint.json` lookup from **cwd**, per-project rule
installation, literal file/glob arguments, `--json`, `--jobs`, and exits 0 (no
error findings), 1 (error finding), 2 (usage/configuration failure). The integration
supplies one absolute saved file, `--json --jobs 1`, and the discovered project cwd.

## JSON, ranges, and fixes

[`cli.bend`, `Object.finding`, `Object.fix`, `Object.edits`, `end`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/src/linter/cli.bend)
emit `{ok,findings,suppressed}`. Each finding contains `code`, `severity`, `message`,
`path`, `range`, `fixes`; severities are `error`, `warning`, `information`, `hint`.
Each fix has `title`, `applicability: "dangerous"`, and edits containing `path`,
`range`, `text`. `Object.position` subtracts one from internal line/column values.
[`text/main.bend`, `Position`, `Walk.go`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/src/text/main.bend)
explicitly counts UTF-16 units, including two units for scalars at least 65536.
Suppressed records are separate and must never become editor diagnostics.

JSON has **no source digest, document revision, schema version, tool version, or
compiler identity**. The LSP therefore captures saved content before execution,
checks it again afterward and when offering edits, and refuses fixes for any
closed or unverifiable target. Versioned client-applied edits are not an upstream
proof of behavioral safety. No `--fix`/`--fix-dangerously` command is generated.

## Runtime trust and provenance

[`package.json`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/package.json)
names `src/linter/seam.ts` as the executable, Bun `1.4.3`, Unix
`bendlang/bend/v2.0.37`, and Windows
`MattCozendey/bend/4113803537dd07a7c45101476d9e895fcde06703`.
[`shared/bend2.ts`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/src/shared/bend2.ts)
shows that an ordinary run downloads missing pinned Bend sources, discovers
clang, and may execute `bend version` to report a mismatch. Rule imports can fetch
BendHub packages. The LSP does not perform installation/download commands; enabling
the external provider explicitly trusts these upstream side effects. Offline users
must prepare their analyzer and dependency caches independently.

`seam.ts` compiles the configured rules and spawns a native program or Bun running
its generated JavaScript; workers can create further children. Containment must
cover the process tree, not only its CLI parent. The standalone tool does **not**
emit Bend's own compiler errors; it can skip type-dependent rules on failed
checks and report that on stderr. Its compiler is independent of the LSP-selected
Bend. Available installed-package metadata and upstream stderr are logged without
claiming that arbitrary executable output attests a compiler version.

The parent exercised the genuine officially shipped CLI with Bun 1.4.3. Package
generation produced API hash `0x2b0a0dcada30f71a8eb4ec2e148b9279` and rules hash
`0x5f2737ee4ecbd87b3f07a0b779aafea9`. The sample returned `ok: true`, a
`language/unneeded_plus` hint at zero-based line 2, characters 7–8,
`suppressed: []`, and `fixes: []`. Its stderr independently warned that the PATH
Bend was 2.0.36 while its own pinned Bend was 2.0.37. The provider preserves this
stderr rather than silently attributing those results to the LSP's compiler.
The first rules compilation took 81.66 seconds, so a cold run can need the
explicit 120000 ms deadline or independent cache preparation. This observation is
CLI evidence only, **not** a completed LSP e2e or multi-platform result.

## Reproducible genuine smoke (parent-run)

Use Bun 1.4.3 and either the installed npm package or the official package output
generated from the exact revision above. **Do not point packaged BendHub rules at
the raw development checkout's CLI.** Its local `../lint.Rule` module has a
different identity from the published `bend-lint-api` module. The upstream
[`ship.ts`, `getPackage` / `getNpmPackage`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/src/meta/ship.ts)
rewrites API imports to `bend-lint-api@0.1.0.0` and omits the local API, preserving
the official analyzer implementation. The parent observed the unshipped checkout
fail with the expected-local/observed-packaged `Rule` proof mismatch; that failed
invocation must not be presented as a successful analyzer smoke.

The CLI's runtime imports are Bun/Node built-ins and repository-relative modules
(its `bend2` imports are type-only). clang 14+ is optional; upstream can use its
JavaScript fallback. Explicit preparation below generates official packages into
an empty output directory; omit `--publish` (publications are not part of this
integration). The LSP itself never runs these preparation commands:

```sh
git clone https://github.com/MattCozendey/bend-lint /absolute/plain-path/bend-lint
git -C /absolute/plain-path/bend-lint checkout --detach 02ef84b38b8c06cbd607e270c4930e633e9dab0f
/absolute/path/to/bun /absolute/plain-path/bend-lint/src/meta/ship.ts /absolute/plain-path/bend-lint-shipped
BEND2_LINT_TEST_PATH=/absolute/path/to/bun \
BEND2_LINT_TEST_ARGUMENTS='["/absolute/plain-path/bend-lint-shipped/npm/src/linter/seam.ts"]' \
BEND2_COMPILER_TEST_PATH=/absolute/path/to/selected/bend \
cargo test --locked --test bend_lint_e2e -- --ignored --nocapture
```

Alternatively install `bend-lint@0.1.0` in the trusted project as documented in the
README and pass that installed `node_modules/bend-lint/src/linter/seam.ts` path.
The [registry artifact metadata](https://registry.npmjs.org/bend-lint/0.1.0) records
tarball integrity
`sha512-nPjkcg+fLLu4B0lZCUjvnqlDNF2xQeQW9Gjivx79UOclWR2DuVLxJaN1IqNqlUog3el1zH/3BHox9vnQsdlYpg==`.
Package generation/source inspection alone is not runtime verification; execute
the genuine e2e and retain its result before claiming integration success.

The smoke invokes the genuine CLI, then compares its actual findings and dangerous
replacement with the stdio LSP result for the same saved file. It enables published
`language/unneeded_plus` and `layout/format` rules together. The sample preserves
independent binder-code/message/UTF-16 assertions from
[`language/unneeded_plus.test.ts`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/bend-lint-rules/language/unneeded_plus.test.ts)
and introduces the four-space indentation exercised in
[`layout/format.test.ts`](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/bend-lint-rules/layout/format.test.ts).
The latter rule's
[`judged` implementation](https://github.com/MattCozendey/bend-lint/blob/02ef84b38b8c06cbd607e270c4930e633e9dab0f/bend-lint-rules/layout/format.bend#L2367-L2376)
reports a real full-file replacement. The test explicitly opts in, rejects
automatic action requests, compares the versioned explicit action with the actual
CLI edit, checks disk remains untouched, then simulates user application and
independently expects canonical two-space indentation. This does not replace the
server's normal formatter. Both rules use BendHub `bend-lint-rules@0.1.0.0`;
no fixture replays an analyzer result. The deterministic protocol suite separately
exercises process races, failures, containment, saved buffers, UTF-16 and stale or
conflicting edits.
