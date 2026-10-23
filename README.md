# Bend 2 language server

`bend2-lsp` adds navigation, completion, diagnostics, and formatting for **Bend 2**
to editors that support the Language Server Protocol (LSP).

The server is a standalone Rust executable. It uses its own source indexes for
editor features and your installed `bend` CLI for compiler checks. **Bend is not
bundled with the server.**

## Quick start

1. Install the Bend 2 CLI and make `bend` available to your editor, not just your
   terminal. You can also configure an absolute compiler path below.
2. Install the server with the [one-command installer](#one-command-installation)
   below, then reopen your terminal/editor so it sees the updated `PATH`.
3. Configure your editor to launch `bend2-lsp` over **stdio** for `.bend` files, using
   language ID `bend` or `bend2`. Open a Bend file.

| OS | Available binaries | Notes |
| --- | --- | --- |
| Linux | x86_64, ARM64 | GNU/glibc ≥ 2.39; not a musl/Alpine build |
| macOS | Intel, Apple Silicon | Choose the matching CPU architecture |
| Windows | x64, ARM64 | Native `.exe` binaries |

Starting with **v0.2.4**, releases include direct executables, `.tar.gz` / `.zip`
archives, and generated Shell/PowerShell installers. Stable releases use `vX.Y.Z`;
nightlies are development prereleases named `nightly-<date>-<commit>`.
All six binaries and both installers must pass native E2E before publication.

### One-command installation

Install the latest stable release (installer support starts at **v0.2.4**):

```sh
# macOS / Linux
curl -fsSL https://github.com/IlyaGulya/bend2-lsp-rs/releases/latest/download/bend2-lsp-installer.sh | sh
```

```powershell
# Windows PowerShell
irm https://github.com/IlyaGulya/bend2-lsp-rs/releases/latest/download/bend2-lsp-installer.ps1 | iex
```

The installer selects your OS/CPU, verifies the archive SHA256, installs to
`~/.local/bin` (Windows: `$HOME\.local\bin`), and sets up your user `PATH`;
no administrator access or Rust toolchain is needed. Reopen your terminal/editor.
Set `INSTALLER_NO_MODIFY_PATH=1` to opt out of PATH changes. To pin a stable or
nightly release, replace `releases/latest/download` with `releases/download/<tag>`.
These commands execute downloaded code; download and inspect the installer first
if your security policy requires it. **v0.2.3 and earlier have no installer
assets**; use manual installation for those releases.

### Manual installation

As an alternative to executing the installer, download the executable for your
OS/CPU from [Releases](https://github.com/IlyaGulya/bend2-lsp-rs/releases), together
with its adjacent `.sha256` file. Verify the checksum before installing.

Rename it to `bend2-lsp` (`bend2-lsp.exe` on Windows) and place it in a directory
on your editor's `PATH`. On macOS/Linux, make it executable with `chmod +x bend2-lsp`.

Signing and macOS notarization are not provided. Older OS versions are not
certified merely because a binary exists for that OS. See
[installation details](docs/releases.md#installing-an-executable) for checksum
commands, platform requirements, and security prompts.

## Editor setup

Use these values in your editor's LSP configuration:

| Setting | Value |
| --- | --- |
| Server command | `bend2-lsp`, or its absolute path |
| Transport | stdio |
| File extension | `.bend` |
| Language ID | `bend` or `bend2` |

The server is an LSP process, not an interactive CLI. Running it directly can
appear to do nothing: it is waiting for protocol messages on stdin. It does not
provide a `--version` command; its version is returned during LSP initialization.

### Compiler settings

The defaults are `compilerPath: "bend"` and `compilerArguments: []`. To override
them, have your client send this settings object through
`workspace/didChangeConfiguration`:

```json
{
  "bend2-lsp": {
    "compilerPath": "/absolute/path/to/bend",
    "compilerArguments": []
  }
}
```

This is an LSP settings payload, not a configuration file the server reads from
disk. Your editor may have a different way to express it. `initializationOptions`
is not used for these settings.

Arguments are passed as individual strings, not interpreted by a shell. The
server appends the staged source path and `--check-only` itself. The same
configured executable and argument prefix are also used for `bend base`.
Changing these settings clears the cached `Base` module and rechecks open files.

### Optional bend-lint (disabled by default)

`bend2-lsp` can run the standalone
[`bend-lint`](https://github.com/MattCozendey/bend-lint) after you save a file.
Compiler and lexical diagnostics remain independent and are merged with lint
findings instead of replacing one another.

**Trust boundary:** enabling this provider is explicit consent to execute the
configured external program and the project's lint rules. Rules are executable
project code, not passive configuration. Upstream bend-lint can download its
pinned Bend and BendHub dependencies, compile rules, create caches, and start
native or JavaScript child processes. The LSP does not install bend-lint, run
package-manager discovery, or download dependencies itself. Prepare the external
tool and its caches outside the LSP if you require offline operation.

Install the tool yourself in each trusted project. The inspected standalone
release is `bend-lint@0.1.0`, requiring Bun 1.4.3. Clang is optional: upstream
otherwise builds rules as JavaScript. Its native backend accepts clang 14 or
newer, but **Windows ARM64 requires a compatible version**, not just that minimum.

For native rule builds on Windows ARM64, use the verified
[Clang 18.1.8 ARM64 installer](https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.8/LLVM-18.1.8-woa64.exe).
Before installing or running bend-lint, set `CC` to the installed `clang.exe`'s
absolute path. Your editor must inherit that environment when starting the LSP.
The native CI details below explain the LLVM 19–22 incompatibilities.

```sh
# These are explicit installation commands for you, never commands run by the LSP.
bun add -D --trust bend-lint@0.1.0
```

The upstream installer can create `bend-lint.json` and a formatter rule. Review
those files before enabling the provider. The server itself never creates lint
configuration or replaces its existing formatter. The older tool bundled with
Bend 2.0.37 / PR #1389 uses a different CLI/JSON contract and is not supported.
The machine-readable contract was checked against standalone upstream
[`02ef84b38b8c06cbd607e270c4930e633e9dab0f`](https://github.com/MattCozendey/bend-lint/tree/02ef84b38b8c06cbd607e270c4930e633e9dab0f).
Use the installed npm package, not a raw development checkout: upstream's
packaging rewrites its rule API imports to match published BendHub packages.

Send the following through `workspace/didChangeConfiguration`, together with any
compiler overrides you already use. Use an absolute installed Bun path and an
absolute installed script path; this also avoids Windows `.cmd` launchers:

```json
{
  "bend": {
    "lint": {
      "enabled": true,
      "path": "/absolute/path/to/bun",
      "arguments": ["/absolute/project/node_modules/bend-lint/src/linter/seam.ts"],
      "timeoutMs": 120000,
      "dangerousCodeActions": false
    }
  }
}
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `bend.lint.enabled` | `false` | Explicit consent to external tool and project-rule execution |
| `bend.lint.path` | empty | Required absolute executable path; no PATH/package-manager discovery |
| `bend.lint.arguments` | `[]` | Literal argument prefix, for example an installed script; never interpreted by a shell |
| `bend.lint.timeoutMs` | `10000` | Process deadline, from 1 to 120000 ms; first rule builds can require the larger value |
| `bend.lint.dangerousCodeActions` | `false` | Separate consent to explicitly requested, potentially behavior-changing edits |

The invocation appends `<saved-file> --json --jobs 1`, with the working directory
set to the nearest ancestor containing `bend-lint.json`. No `--fix` or
`--fix-dangerously` invocation is permitted. Missing tools, invalid configuration,
malformed output, process failures, and timeouts are reported through the editor's
LSP log, not fake source diagnostics. Process output is bounded to 8 MiB per stream;
logged stderr is limited to 16 KiB. Cancellation, timeout, shutdown, and exit kill
and reap the contained process group (Unix) or job object (Windows).

Only `didSave` starts analysis: opening, typing, and enabling the setting do not.
Disk findings are withheld if any open file buffer differs from disk, including
dependencies and configured program entries. One lint job and result snapshot are
active server-wide; a new save supersedes the previous one. Edits, close/reopen,
workspace/configuration changes, and watched source/rule/configuration changes
cancel and clear lint results conservatively. The saved snapshot is bounded to
4096 files, 4096 directories, 32768 directory entries, and 32 MiB. Source features
and compiler overlay checks continue to support unsaved text.

Findings keep the upstream rule code, severity, message, path, and verified
zero-based UTF-16 range under source `bend-lint`; suppressed findings are ignored.
The standalone analyzer does **not** emit Bend's own compiler errors. It can skip
type-dependent rules when its own check fails. Its pinned compiler is independent
of `compilerPath`: the inspected Unix tool pins Bend 2.0.37, while Windows pins an
upstream fork. The LSP log includes invocation identity, available installed-package
version/compiler metadata (not executable attestation), and upstream stderr notes.
An arbitrary executable or the JSON response alone does not attest tool versions.

With `dangerousCodeActions: true`, invoke Code Actions explicitly to see
`quickfix.bend-lint.dangerous` suggestions labelled **Dangerous**. They are never
preferred, offered on automatic requests, applied by the server, or exposed as a
safe fix-all. Every touched file must be open, saved, unchanged since analysis,
and have valid non-overlapping UTF-16 edits. All edits carry document versions;
your client must honor those versions when applying them. Closed, missing, dirty,
stale, or unverifiable multi-file targets are refused as a whole.

The native quality matrix requires both the portable lifecycle suite and a
genuine analyzer E2E on all six OS/CPU targets. The latter checks real
`language/unneeded_plus` diagnostics and a `layout/format` replacement through
stdio LSP, refuses automatic actions, and applies the versioned edit only as an
explicit simulated user action. Its bootstrap pins the npm archive and native
Bun 1.4.3 archive checksums and records the actual runtime architecture.
Windows ARM CI additionally pins native Clang 18.1.8 through upstream's `CC`
override and verifies that the genuine analyzer builds an ARM64 rule executable.
Bend's existing compiler-capability guard uses ordinary C calling conventions
with this version; the native backend remains enabled and no linter or compiler
source is patched. LLVM 19–21 have a Windows ARM64
[`preserve_most` register-preservation defect](https://github.com/llvm/llvm-project/pull/166436);
LLVM 22.1.8 instead fails the rule program's SEH prologue.

Deterministic regression tests also hold lint publication busy while opening,
editing, closing, or immediately saving a document. Subsequent requests must not
overtake document updates or observe an older snapshot, even with lint disabled.
Process fixtures wait for a stdio read acknowledgement before releasing held
analyzer output: writing prior mutations into stdin alone does not prove that
the server has observed them.

In-flight results and requested edits are also rejected when a captured source
or configuration symlink changes target, including symlinked parent directories;
unchanged target bytes alone are not sufficient. Linux regression cases require
findings on non-UTF-8 native filenames to retain their lossless file URIs without
terminating the server.

Failed genuine CLI runs can retain their exact input and invocation through
`BEND2_LINT_TEST_FAILURE_DIR`. Windows ARM CI additionally captures native PE
headers and an exception minidump with a restricted debuggee environment; CI,
repository, and cache credentials are not passed to the debugger or analyzer.
Diagnostic collection never turns the original failing E2E into a pass.

The genuine test is opt-in locally because it executes trusted rules and may
populate upstream's compiler and BendHub caches. After provisioning your tools:

```sh
BEND2_LINT_TEST_PATH=/absolute/path/to/bun \
BEND2_LINT_TEST_ARGUMENTS='["/absolute/project/node_modules/bend-lint/src/linter/seam.ts"]' \
BEND2_COMPILER_TEST_PATH=/absolute/path/to/bend \
cargo test --locked --test bend_lint_e2e -- --ignored --nocapture
```


## Supported LSP features

“Supported” means the feature is implemented; its UI still depends on your
editor. Source-based features are **not** a full compiler type checker.

| Feature | Support and scope |
| --- | --- |
| Diagnostics | Local lexical checks plus errors from the installed Bend CLI; optional saved-file bend-lint findings |
| Completion | Fuzzy ranked names, UTF-16 identifier replacement edits, scoped bindings, explicit-type constructor filtering in nested case patterns, imported members, `Base`, indexed import paths, and indexed symbols with acceptance-time auto-imports |
| Signature help | Function parameters and the active argument |
| Hover | Declaration-derived information; not inferred types for arbitrary expressions |
| Go to definition | Indexed declarations, ADT constructors, and resolved imports |
| Go to type definition | Declaration-derived type navigation |
| Find references | Indexed symbol occurrences in the loaded workspace graph |
| Rename | Indexed symbols and module aliases; client-driven file/folder rename updates imports and indexed identities |
| Document highlights | Matching symbol occurrences in the current document |
| Document symbols | Outline of declarations in a document |
| Workspace symbols | Search over indexed workspace documents; not a scan of every file on disk |
| Semantic highlighting | Full-document semantic tokens |
| Code actions | Auto-import, comment-preserving organize imports, missing closing delimiters, and separately opted-in explicit dangerous lint fixes |
| Inlay hints | Argument-name hints; not inferred-type hints |
| Code lenses | Reference counts |
| Call hierarchy | Incoming/outgoing calls through indexed declarations and local imports |
| Type hierarchy | Algebraic data types and their constructors; requires client dynamic registration |
| Formatting | Whole document, selected range, and on-type formatting triggered by newline |
| Folding | Foldable source regions |
| Selection ranges | Expand selection through enclosing source ranges |
| Document links | Resolved `Base`, local/extensionless paths, and already cached package imports |
| Incremental editing | Unsaved changes are used by source features and staged compiler checks |
| Workspace folders | Multiple roots and workspace-folder changes |
| Watched files | Rechecks affected documents when the client sends file-change notifications; dynamic watcher registration when supported |

For a qualified use such as `Ast.TermVar`, when `Ast` is an import alias,
placing the cursor on `Ast` opens the imported module; placing it on `TermVar`
jumps to the constructor declaration. This works in annotations, patterns, and
expressions. A written qualified declaration is selected by its complete name,
including its prefix. A dotted prefix without an import alias is part of the
symbol name, not a module navigation target.

Local bindings shadow the same **complete Bend name**: a parameter named `target`
shadows a global `target`, but a parameter named `D` does not shadow `D.target`.
Hover and type navigation use a binding's own indexed annotation when available.
Renaming an import alias changes that alias and its actual qualified uses;
conflicts and capture are rejected.

Completion includes parameters and case-pattern bindings only in their indexed
scope. Local bindings shadow declarations with the same complete name; an
unqualified binding does not turn a distinct dotted name into object access.

Import completion searches indexed file names and import paths, including fuzzy
input such as `apd` for `./nested/append.bend`. File-name exact and prefix matches
rank ahead of fuzzy matches; directory prefixes still constrain the path.
The suggestions refresh while typing, after `/` or `.`, and after backspace.
Candidates come from open documents and already indexed imports, not a hidden
scan of every project file. Open a target file to make an otherwise unknown file
available.

In Zed, enable automatic LSP suggestions for the extension's language name:

```json
{
  "languages": {
    "Bend 2": {
      "show_completions_on_input": true,
      "completions": { "lsp": true }
    }
  }
}
```

Type the file name normally after `import `; a manual completion command is not
required. Zed can give an active inline edit prediction precedence over ordinary
word-triggered completions. If that prevents LSP suggestions, add
`"show_edit_predictions": false` to the same `"Bend 2"` object to prefer LSP
completion without changing predictions for other languages. The extension does
not disable automatic completion or change your personal settings.

Regular symbol completion also offers matching declarations from already indexed
modules, including their unsaved buffers. The source module is shown beside each
candidate, so same-named symbols remain distinguishable. Accepting a suggestion
inserts its qualified name and the needed import together; an existing usable alias
is reused, and new aliases avoid indexed names. In-scope names rank before these
cross-module candidates. Browsing or cancelling the popup does not change the buffer.
Repeat acceptance uses the latest unsaved imports and does not add duplicates.
The Missing import quick fix remains available and follows the same alias rules.
Unopened `.bend` files discovered under workspace roots are also candidates;
the server does not download packages.

Known explicit ADT annotations narrow case-pattern constructor candidates.
Unknown types keep the available candidates rather than pretending to infer a type.

Accepting an import completion inserts `import Base` without an alias, or a
complete local/cached-package import such as `import tools.bend as Tools`.
An alias you already typed is preserved; accepting in the middle of a path
replaces its old suffix too. New aliases use the filename stem with non-ASCII
and punctuation characters removed, an uppercase first ASCII letter, and
`Module` prefixed for empty or digit-leading stems. Name conflicts add `2`, `3`,
and so on (`Tools2`); repeated imports reuse their existing alias. Unicode
filenames remain unchanged in the path.

Call hierarchy distinguishes direct calls from higher-order local values and
includes `!` calls and the implementation bodies linked to laws. References and
rename use independently resolved loaded entry contexts; conflicting edits are
rejected instead of returning a partial rename.

For semantic extraction, the experimental [`bend2/semanticDocument` request](docs/semantic-api.md)
returns documentation, written signature/body ranges, and resolved occurrences
with their enclosing declarations. The selected entry matters when imports
supply names or implementations to another file. This API does not promise type
inference or a complete runtime call graph.

Local imports are indexed for cross-file features. `Base` comes from `bend base`.
Hub package navigation uses packages already present in the local Bend library
cache (`BEND_LIB` or the Bend home library); the server does not fetch packages.
Untitled and virtual documents also support source-based features. Their text is
staged in temporary files for compiler checks, rather than saved into your project.

The generated `Base.bend` is compiler-owned navigation source, not a standalone
program. Opening it preserves indexed features and local lexical diagnostics
without running a compiler check against it as a root. Its backing file and
provenance remain in the workspace index until server shutdown, including after
compiler settings change or the editor closes/reopens it. User-owned files named
`Base.bend` still receive normal compiler checks.

Cross-file document queries wait for pending revisions in their indexed import
graph; unrelated documents remain queryable while snapshots are built. Watched
disk updates do not overwrite active unsaved import edges. Closing a user-owned
document reloads its current disk contents only if another open document still
needs it; otherwise its workspace snapshot is released.

Snapshots and import metadata are prepared outside workspace locks. A validated
workspace commit publishes them together with revision state and generation.
Closing starts a new revision epoch: queued closes, cancelled tickets, and old
diagnostics cannot overwrite a reopened buffer, even when version numbering
restarts. Disk loads validate per-file generations before publication, so a late
load cannot restore a graph retired by close or import removal.

Background discovery indexes regular `.bend` files under workspace roots,
respecting ignore rules and excluding hidden/service directories and symlinks.
Workspace symbols, references/rename, and auto-import candidates include these
unopened files. Explicit imports still load their dependencies, including paths
outside workspace roots and discovery exclusions.

Local completion does not wait for the initial background scan. While discovery
is pending, completion lists are marked incomplete so the editor can request
updated auto-import candidates. Preceding watched-file updates still finish
before completion reads their results.

User-owned snapshots follow the import graph of open documents and discovered
workspace roots. Watched create/change/delete events update the index; unsaved
buffers take precedence over disk snapshots. Removing a workspace folder retires
its discovered roots, while open buffers and explicit imports retain their
dependencies. Removing the last owning root releases orphan snapshots and
outgoing import edges, including cycles. Events outside this scope are ignored
before source reads. Stable identities survive eviction, and in-flight readers
may retain their immutable snapshots.
Compiler-result cache keys retain exact source bytes, not complete semantic
indexes. This is demand-driven ownership, not an RSS cap; compiler-owned generated
navigation sources retain the lifetime described above.

Rename Symbol selects the name under the cursor: an explicit import alias, the
qualifier in `Test.foo`, a supported resolved member such as `foo`, or a local
binding. The rename preview highlights only that name. An import path such as
`test.bend` is a file link, not an alias rename target; use the project tree's
file/folder rename instead. Alias rename leaves import paths, shadowed locals,
unrelated members, and comments unchanged.

File-operation clients must request `workspace/willRenameFiles`, apply the
returned versioned workspace edit, perform the filesystem move, and send
`workspace/didRenameFiles`. Open buffers retain their unsaved snapshots; known
closed sources are refreshed during cold staging. Compiler-owned `Base` cannot
be moved or overwritten.

Global semantic/reverse-index research remains closed. Background file discovery
uses the existing immutable snapshots, dense per-document syntax indexes, and
import graph; it does not introduce a global semantic database. See the
[architecture decision and scope amendment](docs/adr-global-indexing.md).
The [integration salvage audit](docs/integration-salvage-audit.md) classifies
every substantive change in the closed index/discovery PRs against main.

Workspace, compiler, diagnostics, and registration services own server state.
State poisoning and unexpected worker failure are fatal invariant errors, not
missing feature results. Shutdown cancels and drains discovery and diagnostics
workers and compiler children. Discovery retains the existing analysis
representation, cross-file query implementation, and LSP framework.

Formatting normalizes indentation and token spacing while preserving tokens,
comments, line endings, and whether the file ends with a newline. It honors
`tabSize` and `insertSpaces` and declines unsafe rewrites.

### Organize imports in Zed

With a `.bend` editor focused, open the command palette and run
**editor: organize imports** (`editor::OrganizeImports`), or use the default
**Alt+Shift+O** shortcut (**Option+Shift+O** on macOS). This is a whole-document
source action, not a diagnostic quick fix: the cursor can be in the function
body, and no error or selection is required. It uses the current unsaved buffer
and leaves saving to the editor. The server must support `source.organizeImports`
(available since v0.4.0); an older configured or extension-installed binary will
not offer it.

For example, before:

```bend
# File header
import ./z.bend as Z # keep this comment
# Dependency documentation
import ./dep.bend as D

def main: U32
  D.value
```

After:

```bend
# File header
# Dependency documentation
import ./dep.bend as D
import ./z.bend as Z # keep this comment

def main: U32
  D.value
```

Rules:

- Organize each leading import group independently. Blank lines and incomplete
  import suffixes separate groups; imports are not moved into another group.
- Sort by the written import path, then alias, without rewriting either.
  Preserve order when different targets share an alias, including multiple
  unaliased targets whose exposed names may conflict.
- Deduplicate only the same indexed target (or identical written path when
  unresolved) with the same alias. Distinct aliases remain. Keep duplicate
  comments attached to the surviving import.
- Inline comments and contiguous comment-only lines before a following import
  move with that import. The file header before the first import stays in place.
  Preserve LF/CRLF, final-newline presence, and text outside the affected groups.
- **Do not remove unused imports.** This action does not perform compiler-backed
  usage analysis.

When there is nothing safe to change, the server returns no organize action.
Zed's command then completes without changing the buffer or showing a success
message; a second invocation is normally this same no-op. This also applies to
files without imports and groups whose existing order must be preserved.

## Not supported

- **Full compiler-powered semantic analysis:** expression type inference,
  compiler-derived hover/completion, and inferred-type inlay hints.
- **Go to implementation** and the separate LSP **go to declaration** request.
  Go to definition is supported.
- **General refactorings:** extract function and compiler-driven semantic quick fixes.
- **File creation/deletion hooks:** rename hooks are supported, but creation and
  deletion do not provide automatic import rewrites.
- **Package discovery:** fetching missing Hub packages automatically or indexing
  files outside workspace roots without an explicit import.
- **Pull diagnostics** (`textDocument/diagnostic`, `workspace/diagnostic`).
  Diagnostics are pushed through `textDocument/publishDiagnostics`.
- **Semantic token range/delta requests**, completion-item resolution, and
  on-type formatting triggers other than newline.
- **Build/run/debug integration:** the server does not replace Bend's CLI or a
  debug adapter.
- **Automatic compiler installation/update:** compatibility probes do not install
  Bend or enforce a supported numeric version range.

## Roadmap

These are proposed priorities, not implemented capabilities or release
commitments. There are no scheduled delivery dates; the support table and
limitations above describe this checkout; see the changelog for unreleased work.

1. **Compiler-backed language features:** semantic fixes, expression types and
   inferred-type hints still need a real Bend compiler contract; do not introduce
   a divergent independent type checker.

New semantic data must be prepared during cold snapshot/index construction.
Warm features must reuse immutable snapshots and workspace indexes rather than
rescan entire documents or rediscover files on every request.

## Bend compatibility and diagnostics

There is **no hard pin to a particular Bend version**. The server runs the Bend
executable you configure and expects these CLI operations to remain compatible:

```text
bend [compilerArguments] <staged-file.bend> --check-only
bend [compilerArguments] base
```

A newer compiler is not rejected simply for being newer. That is not a promise
of forward compatibility: CLI changes, new language syntax, or a different
error-output format can break compiler integration or source-based features.
The server has its own syntax index, so acceptance by the compiler does not
necessarily mean a new language construct is understood by every editor feature.

Startup/configuration probes check `version` and `--help`; known missing CLI
operations produce a clear compatibility warning. Unknown version output is not
a numeric-version rejection. Installing or replacing an executable allows a new probe.

Enhanced checks report multiple independent compiler errors in one check,
including imported-source errors, with the exact registered Bend 2.0.37 builds
for macOS, Linux, and Windows on ARM64 and x86_64. Earlier verified Bend
2.0.32–2.0.36 builds retain Apple Silicon macOS support. The adapter runs the
installed compiler's own parser and checker; it does not introduce a separate
type checker or require a separately installed JavaScript runtime. Executable
bundle, target, and embedded runtime identities must match the verified registry
before recovery is enabled.

Windows has no official upstream Bend binary. Enhanced Windows checks require
the checksum-pinned native upstream source build reproduced by
`scripts/bootstrap_bend.py`; see [compiler provisioning](docs/releases.md#native-platforms-and-gates).
Bend is not bundled with the LSP.

Structured diagnostics retain their original source and UTF-16 range. Errors in
dependencies also appear on the affected import path, with links to the original
errors, including through transitive imports. Independent source errors remain
visible. Blocked definitions are not separate errors and do not produce a
first-line incomplete-check counter. Backend failures, including an incomplete
check with no located cause, remain explicit failures rather than clean results.

Unverified platforms and compiler versions retain the textual CLI integration. Every
genuine error block emitted by a check is retained and deduplicated across stdout
and stderr, but that compiler may stop after the first independent error.
Locations use validated source excerpts and UTF-16 caret spans. Ambiguous matches
fall back to the root rather than pointing at a wrong import.

Checks use the latest unsaved buffers and reachable relative imports in a
temporary tree. Changes are debounced by 250 ms; superseded checks are cancelled.
If the compiler is unavailable, compiler checks cannot work, but source-based
editor features do not require the compiler. `Base` navigation does require it.

Compiler-owned `Base` source opened through navigation is not checked as an
ordinary user module. This exemption follows the source's ownership, not its
filename: a project-local `Base.bend` or `base.bend` is still checked. Importing a
local copy alongside `import Base` can produce genuine duplicate declarations.

### Troubleshooting

- **No features at all:** check the server command, `.bend` association, language
  ID, and your editor's LSP log.
- **Compiler cannot be started:** a GUI editor may not inherit your terminal's
  `PATH`. Set an absolute `compilerPath`.
- **No `Base` completions/navigation:** check that the configured compiler can
  run `bend base` successfully.
- **No Hub package navigation:** ensure the package is present in the local Bend
  library cache and the editor sees the correct environment.
- **Error points at the start of a file:** the compiler's output could not be
  mapped unambiguously; read the diagnostic message for the compiler error.
- **A feature is missing from the UI:** check both the support table above and
  your editor's support. Type hierarchy needs dynamic registration in particular.

## Development

The Rust toolchain is pinned in `rust-toolchain.toml` (currently 1.98.1).

```sh
cargo build --locked --release
```

The server executable is `target/release/bend2-lsp` (`.exe` on Windows).
Before submitting a change, install the pinned helper tools and run the full gate:

```sh
./scripts/install-tools.sh
./scripts/quality
```

Installing the helpers requires Rust/Cargo and Go. The gate covers formatting,
rustc, Clippy, tests, feature combinations, dependency policy, and GitHub Actions
security checks (`actionlint`, `ghalint`, `zizmor`).

The full gate automatically provisions genuine checksum-pinned Bend 2.0.37 when
`BEND2_COMPILER_TEST_PATH` is unset. To provision it explicitly on macOS/Linux:

```sh
python3 scripts/bootstrap_bend.py --destination /tmp/bend-native-e2e-2.0.37
export BEND_NO_TELEMETRY=1
export BEND2_COMPILER_TEST_PATH=/tmp/bend-native-e2e-2.0.37/bin/bend
./scripts/quality
```

The destination must not already exist. All six native quality, release, and
installed-executable matrices require the genuine compiler tests. Unix hosts use
pinned official distributions; Windows builds exact upstream source with a
pinned native Bun runtime and a resource-locator adjustment, not an official
Windows compiler release. The server still does not bundle Bend.

Focused stdio E2Es use the actual server and compiler, not a compiler fixture:

```sh
cargo test --locked --test native_compiler_diagnostics --test native_law_ranges --test compiler_multi_error
```

They check direct and transitive import locations, links to original errors,
independent source errors, unsaved corrections, exact declaration spans, and
compiler-owned versus user-owned `Base`, and spaces/non-ASCII workspace and
temporary paths. A separate macOS ARM64 job retains Bend 2.0.36 coverage.

Real Neovim E2E coverage runs the native server through Neovim's built-in LSP
client with default and explicit configurations:

```sh
cargo build --locked --bin bend2-lsp
python3 scripts/neovim_e2e.py --nvim nvim --binary target/debug/bend2-lsp
```

Quality CI uses checksum-pinned Neovim 0.12.5 on Linux. The harness isolates
editor/compiler configuration and exercises unsaved buffers, navigation,
completion edits and diagnostic recovery. Completion edits are applied through
Neovim's LSP edit utility; popup handling of a middle-token suffix is not asserted.

For the persistent ARM64 Linux environment:

```sh
docker compose up -d --build rust
docker compose exec rust ./scripts/quality
```

Keep the named Cargo and target volumes. Use `docker compose stop rust` /
`docker compose start rust` for routine pause/resume; rebuild only when the
container definition changes.

### Native Zed acceptance on macOS

The separately patched Zed checkout contains the opt-in runner at
`crates/zed/src/visual_test_runner/bend2_acceptance.rs`. This is a local
extension of Zed's existing offscreen visual-test infrastructure, not a test
included in unmodified upstream Zed or this repository's default quality gate.
Run from that patched checkout with absolute executable paths:

```sh
BEND_NO_TELEMETRY=1 \
BEND2_LSP_TEST_BINARY=/absolute/path/to/bend2-lsp \
BEND2_COMPILER_TEST_PATH=/absolute/path/to/bend \
VISUAL_TEST_OUTPUT_DIR=/tmp/bend2-zed-acceptance \
cargo run --locked -p zed --bin zed_visual_test_runner --features visual-tests
```

The runner launches the actual LSP child and compiler, opens real editor buffers,
and applies unsaved edits through GPUI input rather than macOS keyboard
automation. The previously recorded native acceptance covered independent errors,
damaged imports, and unsaved dependency changes using the earlier incomplete-check
banner contract. The mandatory stdio E2Es above cover the current import-local
diagnostic contract; this separately maintained visual runner is not a CI gate.
Assertions check compiler ownership, semantic error fields, exact ranges,
versioned publications, and unchanged disk bytes. Metal-rendered screenshots
record the editor surface. Existing completion, rename, and organize-import
scenarios remain enabled.

Startup waits for project-local binary settings to become effective. Teardown
keeps registered buffers alive while editor views close, then drains and reaps
the owned LSP child before releasing versioned documents. The runner supplies
language metadata and an explicit binary; it does not test extension WASM loading.

### Hosted performance workflow

Use the same Rust entrypoint on Linux, macOS, and Windows:

```sh
cargo perf doctor
cargo perf compare --base main --candidate HEAD
cargo perf profile discovery-10000 --cpu
cargo perf profile discovery-10000 --heap
cargo perf profile discovery-10000 --native
cargo perf open <run-id-or-downloaded-directory>
```

`compare` and `profile` resolve pushed source commits, launch hosted CI, and
download the exact requested run. They do not measure the local machine.
`--target` selects a native triple; the default runs all six targets.
The offline report separates failed or incomplete collection, changed workload
scope, report-only measurements, and the original Callgrind gate.

Process-memory observations use Linux resident pages, macOS resident size and
separate physical footprint, or Windows working set and separate private commit.
CPU sampling uses pinned samply; native diagnostics use Linux perf, macOS
Instruments, and Windows WPR/WPA. Clean latency, CPU sampling, and heap profiling
run in separate LSP processes against shared, semantically checked scenarios.

For an interactive CPU view, run
`cargo perf open <downloaded-directory> --cpu --scenario discovery-10000 --target <triple>`.
This validates the selected trace and supplies its downloaded symbols to samply;
it does not collect another measurement. The viewer requires samply installed.
Native traces retain their original formats for Instruments, WPA, or perf.
See [the performance policy](docs/performance-policy.md) for prerequisites,
artifact verification, and the diagnostic and weekly workflows.

### Opt-in server heap profiling

The optional `dhat-heap` feature profiles allocations in the actual LSP server
using the Rust [dhat crate](https://docs.rs/dhat/0.3.3/dhat/). Normal builds and
published binaries do not include this allocator. Hosted CI builds separate
optimized, symbolized profiling executables without changing the default release
profile, then captures the real LSP lifecycle and the line-index/folding examples
on all six native platforms. Timing runs use the uninstrumented binaries.

All new performance measurements run on CI, including Callgrind, calibration,
LSP latency/discovery, and allocation profiles. Local correctness checks are not
performance evidence. See [the performance policy](docs/performance-policy.md)
for workloads, per-platform comparisons, and raw artifact provenance.
The unpublished [`bend2-perf`](tools/perf) Rust crate owns the portable LSP
transport, collectors, reports, comparator, and calibration. It builds separately
and is not linked into production server binaries or measured analysis functions.

CI explicitly sets `BEND2_LSP_DHAT_FILE` for each profiling child. Unset disables
profiling even in an all-features build;
an explicitly empty value selects `dhat-heap.json` in the working directory.
An explicit path selects that file. Its directory must already exist; the file
is created or overwritten when profiling finishes, not at startup. A build without the
feature ignores this variable. Release optimization and fat LTO remain unchanged;
debug information and symbol retention apply only to the profiling build.

The collector sends LSP `shutdown`
followed by `exit` and waits for the process to finish. The existing Unix SIGTERM
shutdown path also finalizes the profile after server startup. Profiling starts
before Tokio runtime creation and finishes after its teardown. A `shutdown`
response alone does not write the file. Empty stdin before initialization was
verified to finalize it; closing stdin immediately after initialization did not
terminate either the profiling binary or the published v0.4.0 binary in the
observed sessions. Use `shutdown`/`exit`, not EOF alone, for initialized sessions.
Crashes, aborts, SIGKILL, and other termination that bypasses cleanup cannot be
relied upon to save a profile.

Open the JSON in the [DHAT viewer](https://nnethercote.github.io/dh_view/dh_view.html).
It includes cumulative allocation bytes/blocks, peak live heap (`At t-gmax`),
end-live heap (`At t-end`), and symbolized allocation stacks without a frame-count
limit. The summary and any write errors go to stderr, never LSP stdout.
End-live is measured after server/runtime teardown, not while documents remain
open. Tracked heap is not process RSS or OS memory footprint: it excludes
profiler bookkeeping, allocator overhead/retained pages, stacks, mappings, and
compiler child processes. Collect OS process-memory measurements separately.
Profiles can expose local paths and command-line arguments; review before sharing.

This complements the existing
[Valgrind DHAT example measurements](docs/performance-policy.md), including
line-index/cold-snapshot profiles and folding allocation counts; those are not
actual editor-driven LSP sessions. The upstream crate is experimental, and
allocation stack collection can substantially slow the server and increase
memory use. Use this build for attribution, not production latency measurements.

#### Measured actual-server memory

On macOS ARM64, three fresh alternating stdio sessions compared the
checksum-verified public v0.3.0 and v0.4.0 executables. Each session used the
260,429-byte large benchmark fixture, 100 completion requests after one full-text
edit, 20 further unsaved full-text edits, then 100 additional 32,429-byte medium
documents. Document-symbol responses and revision diagnostics confirmed that
each snapshot was available before sampling. Compiler and prelude loading were
disabled through isolated configuration with an unavailable compiler.

Checkpoint RSS medians, in MiB (1,048,576 bytes), measured with macOS `ps`:

| Checkpoint | v0.3.0 | v0.4.0 |
| --- | ---: | ---: |
| Initialized | 3.31 | 3.64 |
| Large document open | 13.75 | 13.91 |
| After 100 completion requests | 19.50 | 19.64 |
| After 20 further unsaved edits | 39.83 | 37.75 |
| Large plus 100 medium documents open | 192.05 | 190.88 |
| All documents closed | 192.11 | 191.09 |

These are checkpoint samples, not peak RSS, physical footprint, an editor
measurement, or a memory regression gate. The v0.4.0 101-document samples ranged
from 124.84 to 191.05 MiB; the lower sample's cause was not established. Do not
interpret the small median decrease as a demonstrated optimization. Closing
documents did not promptly reduce RSS; this alone cannot distinguish indexed
cache retention, allocator-retained pages, or a leak.

A separate symbolized `dhat-heap` build from v0.4.0 source
`8c866cb6b9d66ed1255b17086222fe5dba39a3e4`, with the optional allocator integration,
measured complete server lifetimes, including shutdown:

| Session | Total allocated MiB / blocks | Peak live heap MiB |
| --- | ---: | ---: |
| Initialize and exit | 0.248 / 967 | 0.167 |
| Open and close large document | 19.829 / 18,935 | 8.474 |
| Large document, one edit, 100 completion requests, close | 47.659 / 105,955 | 12.970 |
| Large plus 100 medium documents, then close | 530.163 / 650,448 | 157.802 |

All four ended with 26,232 tracked bytes in 84 blocks after server/runtime
teardown. The warm-session total includes snapshot rebuilding and protocol work,
not just the individual queries. Global heap peak is the sum of program-point
`gb` fields, not the sum of independent `mb` maxima. The 101-document fixture
contains 3,503,329 source bytes and repeated nested function calls; it does not
represent all project shapes, compiler child memory, generated Base, or package
discovery. Its nine largest peak allocation program points account for 82.0% of
tracked peak bytes, primarily `CallSite`, `Reference`, token storage, and dense
token-to-index arrays. These historical runs precede the compact storage change.

The existing scoped CPU/cache acceptances remain independent of these figures.
Keep Callgrind thresholds unchanged; collect allocation/retained-heap and process
memory evidence separately before proposing a memory gate.

#### Compact indexes: source-identical compiler workload

Six private token-to-name/symbol/reference/call/delimiter arrays now store empty
slots in one machine word rather than two-word `Option<usize>` records. Public
IDs retain their `usize` domain. The cold scanner counts identifier tokens while
already scanning; reference construction reserves for that count rather than all
tokens, without another scan. Published references were already boxed slices:
this reservation change reduces construction allocations, not retained vector
capacity. `Token`, `Reference`, and `CallSite` remain contiguous records; no
hot/cold split or public record-layout migration is needed for these savings.

A frozen 908,972-byte, 16-module selfhost compiler graph, real Bend 2.0.34, and
the identical 71,530-byte generated `Base` were exercised over stdio. The session
opened five compiler sources and Base, issued 100 warm feature requests, applied
five unsaved parser revisions, closed all buffers, and completed shutdown/exit.
Every revision's compiler diagnostics was empty.

| Complete-session heap | Before | Compact indexes and reservation |
| --- | ---: | ---: |
| Total allocated bytes | 798,861,979 | 642,049,185 |
| Peak live bytes | 118,943,743 | 87,044,204 |
| End-live bytes | 26,808 | 26,808 |

That is 19.6% less allocation traffic and 26.8% less peak live heap. Separate
ordinary-binary sessions sampled LSP physical footprint with macOS libproc:
96.45 → 75.52 MiB after opening the graph, and 153.78 → 101.36 MiB after the
five revisions. These are one source-identical pair, not medians or a host-wide
unique-memory measurement. Compiler child memory is not part of DHAT.
The initial Linux ARM64 Callgrind comparison improved cold snapshot instructions
but failed several unchanged warm instruction/cache thresholds. The evidence
does not authorize a regression or replace hosted x86_64 CI.

The `workspace_orphan_graph_release` benchmark separately measures releasing a
hundred-file user-owned graph after import removal and root close, including
fixture destruction. Warm workspace queries reuse the authoritative reachable
file index rather than rebuilding the same graph during each query.

Further details:

- [Performance policy, Callgrind gates, calibration, and LSP latency reports](docs/performance-policy.md) —
  tracks analysis costs, report-only CI calibration, and paired real-process p50/p95; not editor rendering latency.
- [Tracing with Chrome/Perfetto](docs/tracing.md) — opt-in through `BEND2_LSP_TRACE`.
- [Release automation and repository setup](docs/releases.md) — ordinary PR
  review plus CI; server-side branch rules must be configured separately.

## Implementation

Built with `tower-lsp` and Tokio. Cold document construction prepares immutable
syntax and occurrence indexes; warm feature requests reuse snapshots and
workspace indexes. The formatter keeps its own lexical scanner.

Unlike the upstream TypeScript server, this implementation does not bundle Bend.
The installed CLI is the only compiler integration; navigation and editing
features otherwise operate on indexed source.
