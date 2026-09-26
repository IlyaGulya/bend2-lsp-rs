# Bend 2 language server (Rust)

`bend2-lsp` is a Rust LSP for Bend 2, built with `tower-lsp` and Tokio. It uses the installed `bend` command as its only compiler integration; editor features that do not need compiler checking use a small source-based scanner.

## Requirements

- Rust 1.98.1, pinned by `rust-toolchain.toml`
- Bend 2 CLI (`bend`) on `PATH` for compiler checks and `Base` source

Build and run:

```sh
cargo build --release
./target/release/bend2-lsp
```

The server accepts the `bend` and `bend2` language IDs and uses incremental document sync. Diagnostics are debounced by 250 ms. File, untitled, and virtual documents support the same source-based navigation and editing features; virtual text is staged in a temporary directory, not written into the workspace.

## Editor features

The server provides completion (including imported module members and `Base`), signature help, declaration-derived hover and type navigation, definitions, references, rename, document highlights, document/workspace symbols, semantic tokens, delimiter quick fixes, argument-name inlay hints, reference-count code lenses, folding, selection ranges, import document links, and call hierarchy.

Type hierarchy is dynamically registered when the client supports it. For Bend algebraic data types, the declared type is the parent and its constructors are the children. Call and type hierarchy follow indexed local imports.

## Compiler integration and limits

Compiler checks stage the open source and its reachable relative imports in a temporary tree, preferring open buffers over disk. Changes to imported buffers and watched files trigger dependent-document checks. Repeated identical local source graphs reuse their compiler result.

`bend2-lsp` invokes `bend [compilerArguments] <staged-entry> --check-only`. The CLI exposes human-readable diagnostics, not a structured Rust API or a persistent worker. Its current checker stops at the first compiler error, so one check can publish at most one compiler diagnostic; independent lexical diagnostics can still be reported together. The server maps CLI locations from source excerpts and falls back to the root document start when the excerpt cannot be matched unambiguously.

`Base` declarations are loaded with `bend base` and cached for navigation. Hub package navigation uses the local Bend library cache (`BEND_LIB`, or the Bend home library); Hub fetching remains the CLI's responsibility, so navigation requires the package to be cached.

## Formatting

Formatting preserves the token stream, comments, blank lines, line endings, and final-newline state while normalizing indentation and token spacing. The scanner declines to rewrite input it cannot safely fingerprint. Full-document, range, and newline-triggered formatting honor `tabSize` and `insertSpaces`.

## Editor configuration

Launch `bend2-lsp` over stdio and associate `.bend` files with language ID `bend` (or `bend2`). Workspace settings `bend2-lsp.compilerPath` and `bend2-lsp.compilerArguments` select the compiler executable and arguments.

## Development quality and performance

Install the pinned Rust quality tools and run the authoritative local gate:

```sh
./scripts/install-tools.sh
./scripts/quality
```

The gate checks formatting, rustc, Clippy, tests, feature combinations, and dependency policy. Workspace lints deny warnings, Clippy `all`/`pedantic`/`perf`, production `unwrap()`/`expect()`, inline lint suppressions, and unsafe code.

Release builds use optimization level 3, fat LTO, one codegen unit, and stripped symbols. Linux CI compares the benchmarked source-analysis paths with the pull request base using Callgrind; this is not a measurement of end-to-end editor latency. See [performance policy](docs/performance-policy.md).

The policy-integrity workflow requires a maintainer-applied `policy-approved` label for enforcement-file changes; create that label in GitHub before relying on the gate. For non-bypassable enforcement, configure `main` rulesets to require `quality / quality`, `performance / compare`, and `policy-integrity / protect`, pull requests/review, up-to-date branches, and no force-push or bypass. GitHub host settings cannot be committed as repository files. This checkout has no Git remote, so its ruleset and reviewer identity are not configured here.

## Upstream

The upstream TypeScript server bundles the Bend compiler. This Rust implementation keeps the CLI boundary explicit: compiler diagnostics remain limited to the installed CLI's output, while source-based editor features do not claim compiler-derived type information.
