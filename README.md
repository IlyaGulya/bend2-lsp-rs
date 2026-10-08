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
| Completion | Fuzzy ranked names, UTF-16 identifier replacement edits, scoped bindings, explicit-type constructor filtering in nested case patterns, imported members, `Base`, and indexed import paths |
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
| Code actions | Auto-import of available indexed symbols, comment-preserving organize imports, and missing closing delimiters |
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

For qualified names such as `Ast.TermVar`, placing the cursor on `Ast` opens
the imported module; placing it on `TermVar` jumps to the constructor declaration.
The same navigation works in type annotations, match patterns, and expressions.

Declaration queries respect lexical shadowing: a local parameter never resolves
to a same-named global declaration or imported module. Hover and type navigation
use that parameter's own indexed annotation when available. Renaming a module
alias changes its declaration and unshadowed qualified uses; conflicts are rejected.

Completion includes parameters and case-pattern bindings only in their indexed
scope. Local bindings shadow same-named declarations, prelude names and import
aliases; a shadowed alias does not offer the imported module's members.

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
be moved or overwritten. This does not discover unrelated project files.

Global semantic/reverse-index research is closed. Production keeps immutable
snapshots, dense per-document syntax indexes and the loaded/reachable import
graph, without a mandatory global semantic database. See the
[architecture decision and reopening trigger](docs/adr-global-indexing.md).
The [integration salvage audit](docs/integration-salvage-audit.md) classifies
every substantive change in the closed index/discovery PRs against main.

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
- **General refactorings:** extract function and compiler-driven semantic quick fixes.
- **File creation/deletion hooks:** rename hooks are supported, but creation and
  deletion do not provide automatic import rewrites.
- **Whole-project discovery:** indexing every unrelated file on disk or fetching
  missing Hub packages automatically.
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

1. **Background whole-project indexing — deferred:** reconsider only after a
   real-workspace editor/LSP latency problem and an agreed performance budget
   satisfy the [architecture decision](docs/adr-global-indexing.md).
2. **Compiler integration — blocked on upstream APIs:** structured diagnostics,
   semantic fixes, expression types and inferred-type hints require a real Bend
   compiler contract. Do not introduce a divergent independent type checker.

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

Compiler diagnostics are parsed from human-readable output, not a structured
compiler API. Every genuine error block emitted by a check is retained and
deduplicated across stdout and stderr. Bend 2.0.34 can stop after the first
independent error; the server cannot invent errors the compiler did not emit.
Locations use validated source excerpts and UTF-16 caret spans. Ambiguous matches
fall back to the start of the root document rather than pointing at a wrong import.

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

### Opt-in server heap profiling

The optional `dhat-heap` feature profiles allocations in the actual LSP server
using the Rust [dhat crate](https://docs.rs/dhat/0.3.3/dhat/). Normal builds and
published binaries do not include this allocator. Build a separate symbolized
profiling binary without changing the default release profile:

```sh
cargo build --locked --release --features dhat-heap --bin bend2-lsp \
  --target-dir target/dhat \
  --config 'profile.release.debug=1' --config 'profile.release.strip="none"'
BEND2_LSP_DHAT_FILE="$PWD/target/server-heap.json" \
  ./target/dhat/release/bend2-lsp
```

Configure your editor to launch this binary and pass `BEND2_LSP_DHAT_FILE` to
the server process. Unset disables profiling even in an all-features build;
an explicitly empty value selects `dhat-heap.json` in the working directory.
An explicit path selects that file. Its directory must already exist; the file
is created or overwritten when profiling finishes, not at startup. A build without the
feature ignores this variable. Release optimization and fat LTO remain unchanged;
debug information and symbol retention above apply only to the profiling build.

Capture a representative session, then let the client send LSP `shutdown`
followed by `exit` and wait for the process to finish. The existing Unix SIGTERM
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
