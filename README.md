# Bend 2 language server

`bend2-lsp` adds navigation, completion, diagnostics, and formatting for **Bend 2**
to editors that support the Language Server Protocol (LSP).

The server is a standalone Rust executable. It uses its own source indexes for
editor features and your installed `bend` CLI for compiler checks. **Bend is not
bundled with the server.**

## Quick start

1. Install the Bend 2 CLI and make `bend` available to your editor, not just your
   terminal. You can also configure an absolute compiler path below.
2. Download the executable for your OS and CPU from
   [Releases](https://github.com/IlyaGulya/bend2-lsp-rs/releases), together with its
   adjacent `.sha256` file. Verify the checksum before installing.
3. Rename the executable to `bend2-lsp` (`bend2-lsp.exe` on Windows). On macOS and
   Linux, make it executable with `chmod +x bend2-lsp`.
4. Configure your editor to launch it over **stdio** for `.bend` files, using
   language ID `bend` or `bend2`. Open a Bend file.

| OS | Available binaries | Notes |
| --- | --- | --- |
| Linux | x86_64, ARM64 | GNU/glibc; not a musl/Alpine build |
| macOS | Intel, Apple Silicon | Choose the matching CPU architecture |
| Windows | x64, ARM64 | Native `.exe` binaries |

Release assets are executables, not archives. Stable releases use `vX.Y.Z`;
nightlies are development prereleases named `nightly-<date>-<commit>`.
All six release binaries must pass native tests and LSP E2E before publication.

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

Local imports are indexed for cross-file features. `Base` comes from `bend base`.
Hub package navigation uses packages already present in the local Bend library
cache (`BEND_LIB` or the Bend home library); the server does not fetch packages.
Untitled and virtual documents also support source-based features. Their text is
staged in temporary files for compiler checks, rather than saved into your project.

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

- [Performance policy, Callgrind gates, and LSP latency reports](docs/performance-policy.md) —
  tracks analysis costs and paired real-process p50/p95; not editor rendering latency.
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
