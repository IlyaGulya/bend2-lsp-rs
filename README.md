# Bend 2 language server (Rust)

`bend2-lsp` is a Rust LSP for Bend 2, built with `tower-lsp` and Tokio. It uses the installed `bend` command as its only compiler integration; each document revision also builds one immutable syntax index shared by source-based editor features.

## Installation

Prebuilt binaries are prepared for Windows, macOS, and GNU/Linux on both x86_64
and ARM64. Download the archive matching your OS/CPU from the repository's GitHub
**Releases** page, verify its SHA-256 checksum, extract it, and put `bend2-lsp`
(Windows: `bend2-lsp.exe`) on `PATH` or configure your editor with its absolute
path. Stable releases use `vMAJOR.MINOR.PATCH`; development builds are immutable
`nightly-YYYY-MM-DD-<sha>` prereleases. Each published archive must pass native
release-profile tests and packaged-binary LSP E2E on its own architecture.

The binaries do **not** include Bend 2. Install a supported Bend 2 CLI (`bend`)
for compiler checks and `Base` source; its availability on your platform is
independent of the server's binary support. Linux archives require GNU/glibc;
macOS archives are separate Intel/Apple Silicon binaries. Signing and
notarization are not provided yet.

See [binary release installation and setup](docs/releases.md) for checksum
commands, supported runners and limits, licensing, the release App secrets, and
administrator settings required to enable publication. Preparing these files
does not activate GitHub settings or publish a release.

To build from source, use Rust 1.98.1, pinned by `rust-toolchain.toml`:

```sh
cargo build --locked --release
./target/release/bend2-lsp
```

The server accepts the `bend` and `bend2` language IDs and uses incremental document sync. Diagnostics are debounced by 250 ms. File, untitled, and virtual documents support the same source-based navigation and editing features; virtual text is staged in a temporary directory, not written into the workspace.

## Editor features

The server provides completion (including imported module members and `Base`), signature help, declaration-derived hover and type navigation, definitions, references, rename, document highlights, document/workspace symbols, semantic tokens, delimiter quick fixes, argument-name inlay hints, reference-count code lenses, folding, selection ranges, import document links, and call hierarchy.

Type hierarchy is dynamically registered when the client supports it. For Bend algebraic data types, the declared type is the parent and its constructors are the children. Call and type hierarchy follow indexed local imports.

## Compiler integration and limits

Compiler checks stage the open source and its reachable relative imports in a temporary tree, preferring open buffers over disk. Changes to imported buffers and watched files trigger dependent-document checks. Results are cached for fully resolved local source graphs up to 1 MiB, with at most 32 roots retained; the key includes compiler path/arguments/stamp, graph paths/import edges, and snapshot contents. Byte-identical text reuses a cached result across document revisions. Hub, absolute, `Base`, unresolved, and larger graphs bypass this cache.

Cache misses stage on a bounded blocking pool (four compiler permits), then `bend2-lsp` invokes `bend [compilerArguments] <staged-entry> --check-only`. The CLI exposes human-readable diagnostics, not a structured Rust API or a persistent worker. Its current checker stops at the first compiler error, so one check can publish at most one compiler diagnostic; independent lexical diagnostics can still be reported together. The server maps CLI locations from source excerpts and falls back to the root document start when the excerpt cannot be matched unambiguously.
Document snapshot construction, watched-file reads, and import graph staging are
gated by a separate four-permit semaphore before entering Tokio's shared
blocking pool.
Document-scoped requests wait for the latest revision and local reachable imports. Workspace-wide requests use the committed database view; pending document revisions are invisible and do not delay those requests.

`Base` declarations are loaded with `bend base` and cached for navigation. Hub package navigation uses the local Bend library cache (`BEND_LIB`, or the Bend home library); Hub fetching remains the CLI's responsibility, so navigation requires the package to be cached.

## Formatting

Formatting preserves the token stream, comments, blank lines, line endings, and final-newline state while normalizing indentation and token spacing. The scanner declines to rewrite input it cannot safely fingerprint. Full-document, range, and newline-triggered formatting honor `tabSize` and `insertSpaces`.

## Editor configuration

Launch `bend2-lsp` over stdio and associate `.bend` files with language ID `bend` (or `bend2`). Workspace settings `bend2-lsp.compilerPath` and `bend2-lsp.compilerArguments` select the compiler executable and arguments.

## Performance tracing

Opt-in Chrome/Perfetto tracing uses `BEND2_LSP_TRACE`; capture setup, privacy guarantees, span meanings, and profiler guidance are in [docs/tracing.md](docs/tracing.md).

## Persistent Linux Docker environment

The ARM64 Compose service keeps the pinned Rust toolchain, quality tools, Valgrind runner, Cargo downloads, and compiled project artifacts available across runs.

Build and start it once:

```sh
docker compose up -d --build rust
```

For later runs, reuse the existing service and named cache volumes:

```sh
docker compose exec rust ./scripts/quality
docker compose exec rust cargo bench --locked --bench analysis -- \
  --callgrind-args='--cache-sim=yes'
```

Use `docker compose stop rust` and `docker compose start rust` to pause and resume the same container. Rebuild only after changing `Dockerfile`; avoid `docker compose down -v` and `docker volume prune` when retaining caches matters.

## Development quality and performance

Install the pinned Rust quality tools and run the authoritative local gate:

```sh
./scripts/install-tools.sh
./scripts/quality
```

The gate checks formatting, rustc, Clippy, tests, feature combinations, and dependency policy. Workspace lints deny warnings, Clippy `all`/`pedantic`/`perf`, production `unwrap()`/`expect()`, inline lint suppressions, and unsafe code.

Release builds use optimization level 3, fat LTO, one codegen unit, and stripped symbols. Linux CI compares the benchmarked source-analysis paths with the pull request base using Callgrind; this is not a measurement of end-to-end editor latency. See [performance policy](docs/performance-policy.md).

The policy-integrity workflow requires a maintainer-applied `policy-approved` label for enforcement-file changes; create that label in GitHub before relying on the gate. For non-bypassable enforcement, configure `main` rulesets to require `quality / quality`, `performance / compare`, and `policy-integrity / protect`, pull requests/review, up-to-date branches, and no force-push or bypass. Performance and policy-integrity are PR-only gates; release automation consumes successful push quality runs and relies on these rules to enforce PR review and performance. GitHub host settings cannot be committed as repository files. See [release setup](docs/releases.md#one-time-repository-setup) for administrator operations; no settings are changed by preparing this checkout.

## Upstream

The upstream TypeScript server bundles the Bend compiler. This Rust implementation keeps the CLI boundary explicit: compiler diagnostics remain limited to the installed CLI's output, while indexed source features do not claim compiler-derived type information.
