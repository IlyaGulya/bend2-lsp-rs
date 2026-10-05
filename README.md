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

## Supported LSP features

“Supported” means the feature is implemented; its UI still depends on your
editor. Source-based features are **not** a full compiler type checker.

| Feature | Support and scope |
| --- | --- |
| Diagnostics | Local lexical checks plus errors from the installed Bend CLI; see limitations below |
| Completion | Local names, keywords, imported module members, and indexed `Base` declarations |
| Signature help | Function parameters and the active argument |
| Hover | Declaration-derived information; not inferred types for arbitrary expressions |
| Go to definition | Indexed declarations, ADT constructors, and resolved imports |
| Go to type definition | Declaration-derived type navigation |
| Find references | Indexed symbol occurrences in the loaded workspace graph |
| Rename | Symbol rename across indexed, loaded documents; not file/module rename |
| Document highlights | Matching symbol occurrences in the current document |
| Document symbols | Outline of declarations in a document |
| Workspace symbols | Search over indexed workspace documents; not a scan of every file on disk |
| Semantic highlighting | Full-document semantic tokens |
| Code actions | Limited quick fixes for missing closing delimiters |
| Inlay hints | Argument-name hints; not inferred-type hints |
| Code lenses | Reference counts |
| Call hierarchy | Incoming/outgoing calls through indexed declarations and local imports |
| Type hierarchy | Algebraic data types and their constructors; requires client dynamic registration |
| Formatting | Whole document, selected range, and on-type formatting triggered by newline |
| Folding | Foldable source regions |
| Selection ranges | Expand selection through enclosing source ranges |
| Document links | Import navigation |
| Incremental editing | Unsaved changes are used by source features and staged compiler checks |
| Workspace folders | Multiple roots and workspace-folder changes |
| Watched files | Rechecks affected documents when the client sends file-change notifications; dynamic watcher registration when supported |

For qualified names such as `Ast.TermVar`, placing the cursor on `Ast` opens
the imported module; placing it on `TermVar` jumps to the constructor declaration.
The same navigation works in type annotations, match patterns, and expressions.

Declaration queries respect lexical shadowing: a local parameter never resolves
to a same-named global declaration or imported module. Hover and type navigation
use that parameter's own indexed annotation when available. Module aliases remain
navigation targets, but alias rename is rejected rather than applied to a member.

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
disk updates do not overwrite active unsaved import edges. Closing a document
restores the latest disk snapshot and its imports.

Snapshots and import metadata are prepared outside workspace locks. A validated
workspace commit publishes them together with revision state and generation.
Closing starts a new revision epoch: queued closes, cancelled tickets, and old
diagnostics cannot overwrite a reopened buffer, even when version numbering
restarts. A prepared close also validates the disk snapshot before restoring its
imports.

Workspace, compiler, diagnostics, and registration services own server state.
State poisoning and unexpected worker failure are fatal invariant errors, not
missing feature results. Shutdown drains owned diagnostics work and compiler
children. This server refactor retains the existing analysis representation,
cross-file query implementation, and open/import-reachable workspace scope;
it does not add whole-project discovery or change the LSP framework.

Formatting normalizes indentation and token spacing while preserving tokens,
comments, line endings, and whether the file ends with a newline. It honors
`tabSize` and `insertSpaces` and declines unsafe rewrites.

## Not supported

- **Full compiler-powered semantic analysis:** expression type inference,
  compiler-derived hover/completion, and inferred-type inlay hints.
- **Go to implementation** and the separate LSP **go to declaration** request.
  Go to definition is supported.
- **General refactorings:** extract function, organize imports, automatic import
  insertion, and compiler-driven quick fixes.
- **File-operation hooks:** automatic import updates when files are created,
  renamed, or deleted through LSP file-operation requests.
- **Whole-project discovery:** indexing every unrelated file on disk or fetching
  missing Hub packages automatically.
- **Pull diagnostics** (`textDocument/diagnostic`, `workspace/diagnostic`).
  Diagnostics are pushed through `textDocument/publishDiagnostics`.
- **Semantic token range/delta requests**, completion-item resolution, and
  on-type formatting triggers other than newline.
- **Build/run/debug integration:** the server does not replace Bend's CLI or a
  debug adapter.
- **Bend version negotiation:** no supported-version range, startup version
  check, or automatic compiler installation/update.

## Roadmap

These are proposed priorities, not implemented capabilities or release
commitments. There are no scheduled delivery dates; the support table and
limitations above describe this checkout; see the changelog for unreleased work.

1. **Context-aware completion:** offer types in annotations, constructors in
   patterns, and in-scope local bindings instead of unrelated suggestions.
2. **Auto-import and useful quick fixes:** insert imports for selected symbols,
   reuse existing aliases, and avoid name conflicts.
3. **Background whole-project indexing:** discover and index project files,
   including unrelated files, and incrementally update them after changes.
4. **Alias and file/module rename:** update affected imports and references with
   conflict checks and coordinated workspace edits.
5. **Compiler integration research:** investigate structured compiler output or
   APIs for more precise types and diagnostics before committing to
   compiler-powered hover, completion, or inferred-type hints. Do not introduce
   an independent type checker that can diverge from Bend.

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

Compiler diagnostics are parsed from human-readable output, not a structured
compiler API. The current integration can report at most one compiler diagnostic
per check; independent lexical diagnostics can appear alongside it. Locations
are matched using source excerpts. If a match is ambiguous, the diagnostic falls
back to the start of the root document rather than pointing at the wrong import.

Checks use the latest unsaved buffers and reachable relative imports in a
temporary tree. Changes are debounced by 250 ms; superseded checks are cancelled.
If the compiler is unavailable, compiler checks cannot work, but source-based
editor features do not require the compiler. `Base` navigation does require it.

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

For the persistent ARM64 Linux environment:

```sh
docker compose up -d --build rust
docker compose exec rust ./scripts/quality
```

Keep the named Cargo and target volumes. Use `docker compose stop rust` /
`docker compose start rust` for routine pause/resume; rebuild only when the
container definition changes.

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
