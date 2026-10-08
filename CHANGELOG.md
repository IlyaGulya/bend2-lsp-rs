# Changelog

## Unreleased

### Better import completion

- Accept local and cached-package import suggestions without manually adding an
  alias. Keep explicit aliases and comments, replace the full path when the
  caret is in its middle, and generate valid collision-free ASCII aliases for
  nested, relative, and Unicode filenames.
- Keep `Base` imports unaliased, including Missing import quick fixes. Reuse the
  existing alias when completing another import of the same path.

### More reliable editing

- Show the actual alias, local, or resolved member name in Rename Symbol previews.
  Reject Rename Symbol on import paths with guidance to rename the file or folder
  in the project tree; never rewrite an alias or path segment instead.
  Alias rename keeps matching import path segments unchanged.

### Better completion

- Refresh import suggestions as you type, backspace, or continue after a path
  separator. Find indexed files by fuzzy file names or paths, with file-name
  exact and prefix matches ahead of fuzzy matches.
- Keep the popup's searchable file name separate from the full import path and
  insertion text. Document Zed's automatic-completion settings and inline
  prediction precedence.

### Better completion

- Accept a symbol suggestion from an already indexed module to insert its
  qualified name and missing import together. Suggestions show their source
  module, reuse existing aliases, and avoid duplicate imports and alias conflicts.
  Unsaved modules and imports are supported; browsing or cancelling leaves the
  document unchanged. In-scope suggestions retain priority over cross-module names.

### Development

- Add default-off `dhat-heap` profiling for the actual LSP server, with
  `BEND2_LSP_DHAT_FILE` enabling profiling and selecting the heap JSON output
  (unset disables recording; empty selects the default path). Capture allocation totals,
  peak/end-live heap, and allocation stacks across server and Tokio runtime
  startup/shutdown; normal builds keep their existing allocator and release
  profile. This complements the historical Valgrind DHAT example measurements.

### Memory

- Halve private dense optional-index slot storage without narrowing public IDs.
  Count identifiers during cold scanning to avoid reserving reference rows for
  punctuation. A source-identical selfhost compiler/Base session measured 26.8%
  less peak live heap and 19.6% less allocation traffic; CPU/cache gates remain
  independent and unchanged.

## [0.4.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.3.0...v0.4.0) (2026-10-08)

### Better completion

- Find names by typing only part of them. Exact and prefix matches appear before
  fuzzy matches.
- Accept a suggestion in the middle of a name without leaving its old suffix
  behind, including around emoji and other Unicode text.
- Complete constructors inside nested case patterns. Known explicit types narrow
  the suggestions; unknown types keep the broader list.
- Complete imports for `Base`, local files, and already indexed cached packages.

### Navigation, rename, and imports

- Open `Base`, local, and cached package imports through clickable links.
- Rename import aliases without changing shadowed local names or member names.
- Rename files or folders and update imports in known workspace files while
  preserving open unsaved buffers.
- Add missing imports for available indexed symbols, reusing aliases and avoiding
  name conflicts or duplicates.
- Organize imports without losing comments, aliases, or existing line endings.

### Compiler integration and editor checks

- Show every error block the compiler actually emits with more accurate source
  ranges, including errors after Unicode text in unsaved imported files.
- Recover when a missing or incompatible compiler is installed or replaced.
  Check its CLI capabilities rather than reject it solely by version number.
- Exercise the actual server in Neovim as part of quality CI.

### Limits

Bend can still stop after its first independent error. Compiler-generated
semantic fixes and inferred types for arbitrary expressions need upstream APIs.
Whole-project discovery remains deferred; this release uses document snapshots
and workspace indexes rather than scanning every project file.

### Performance

The maintainer accepted the measured performance cost of these features in
[#32](https://github.com/IlyaGulya/bend2-lsp-rs/pull/32): constructor-prefix
completion uses 3.28% more instructions, and several instruction-cache metrics
exceed the existing thresholds. The separate latency checks passed. These
function-level benchmarks do not measure editor response time. Performance
thresholds and baseline rules are unchanged.



## [0.3.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.5...v0.3.0) (2026-10-07)


### Better autocomplete

- Function parameters and variables introduced in `case` branches now appear
  in suggestions.
- When writing a `case` pattern, autocomplete suggests constructors from your
  file, imported modules, and explicitly imported `Base`.
- Suggestions use your latest unsaved edits, even at the end of an unfinished
  file.
- Names from another scope no longer interfere with local suggestions.

### More reliable editing

- Improved handling of rapid edits and closing or reopening files, so older
  results cannot replace newer ones.
- Fixed cases where outdated diagnostics could remain visible.
- Improved recovery when reloading `Base` fails or returns no declarations.

### Under the hood

- Completed an internal cleanup to make the server easier to maintain.
- Kept the current workspace indexing model. Whole-project background indexing
  is **not included** in this release.

### Engineering details

For implementation history, regression coverage, measured performance
trade-offs, and preserved research, see
[#14](https://github.com/IlyaGulya/bend2-lsp-rs/pull/14),
[#23](https://github.com/IlyaGulya/bend2-lsp-rs/pull/23),
[#25](https://github.com/IlyaGulya/bend2-lsp-rs/pull/25),
[#26](https://github.com/IlyaGulya/bend2-lsp-rs/pull/26),
[#27](https://github.com/IlyaGulya/bend2-lsp-rs/pull/27),
[#28](https://github.com/IlyaGulya/bend2-lsp-rs/pull/28), and
[#30](https://github.com/IlyaGulya/bend2-lsp-rs/pull/30).
The [architecture decision](docs/adr-global-indexing.md) records why global
indexing and discovery remain deferred.

## [0.2.5](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.4...v0.2.5) (2026-10-03)


### Bug Fixes

* preserve compiler-owned Base document provenance ([#11](https://github.com/IlyaGulya/bend2-lsp-rs/issues/11)) ([ab60a28](https://github.com/IlyaGulya/bend2-lsp-rs/commit/ab60a289e775231738a20bab4fff0c89dafe1489))

- Track compiler-owned `Base` source in the workspace index so navigation does not
  trigger invalid standalone compiler checks. Retain its source and provenance
  across compiler configuration changes and editor close/reopen; preserve lexical
  diagnostics and normal checks for user-owned files named `Base.bend`.

## [0.2.4](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.3...v0.2.4) (2026-10-03)


### Features

* add generated cross-platform release installers ([#8](https://github.com/IlyaGulya/bend2-lsp-rs/issues/8)) ([be1cab9](https://github.com/IlyaGulya/bend2-lsp-rs/commit/be1cab9cfbdaa3393195ee8a09184160577373dd))

- Preserve direct binaries and immutable releases; verify archive SHA256 before extraction and exercise installed binaries on all six native platforms.

## [0.2.3](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.2...v0.2.3) (2026-10-02)


### Bug Fixes

* omit release versions from asset filenames ([#6](https://github.com/IlyaGulya/bend2-lsp-rs/issues/6)) ([20910ec](https://github.com/IlyaGulya/bend2-lsp-rs/commit/20910ecb625ef9f14caaf50d9ce26aae7622a12b))

- Retain version/SHA checks in release metadata; previously published assets remain unchanged.

## [0.2.2](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.1...v0.2.2) (2026-10-02)

### Bug Fixes

- Respect lexical parameter shadowing in declaration navigation and hover, including shadowed module qualifiers and scoped parameter annotations.
- Reject unsupported module-alias rename instead of renaming an imported member under a different cursor.
- Preserve body indentation after colonless declarations during newline formatting; handle comments, quoted delimiters, and incomplete literals safely.
- Retain watched closed-file updates across concurrent document commits and preserve newer open-buffer import edges until close.
- Wait for relevant imported-document revisions in cross-file queries without blocking unrelated documents during cold snapshot construction.
- Update revision readiness atomically so older completion or cancellation cannot overwrite a newer pending revision.
- Kill and reap active compiler-check and `bend base` children before shutdown or process exit completes; preserve configured compiler arguments, buffered responses at stdin EOF, and staging resources until child cleanup finishes.
* preserve lexical bindings, workspace revisions and compiler lifecycle ([#4](https://github.com/IlyaGulya/bend2-lsp-rs/issues/4)) ([e08cace](https://github.com/IlyaGulya/bend2-lsp-rs/commit/e08cace9088ff37ec0789bf9694b1a474093f368))

### CI

- Add report-only Callgrind calibration with independent A/A builds, layout and positive-regression controls, preserved raw evidence, and discovery-only proposals checked against independent validation; active gates remain unchanged.
- Preserve raw baseline/candidate Callgrind profiles and summaries after both successful and failed comparisons, without changing metrics, limits, baseline selection, or gate results.


## [0.2.1](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.2.0...v0.2.1) (2026-10-02)


### Bug Fixes

* resolve imported constructor definitions and track LSP latency ([57d9f76](https://github.com/IlyaGulya/bend2-lsp-rs/commit/57d9f76fe8ae47d84f098e67358e178027a2a69f))


### Performance Improvements

* iterate indexed reference slices directly ([a11ef5d](https://github.com/IlyaGulya/bend2-lsp-rs/commit/a11ef5dc633db0451fb48805b57feb215661fbc0))

## [0.2.0](https://github.com/IlyaGulya/bend2-lsp-rs/compare/v0.1.1...v0.2.0) (2026-10-01)


### Features

* automate tested native nightly and versioned releases ([804ea24](https://github.com/IlyaGulya/bend2-lsp-rs/commit/804ea24b9b653c884942123f8497fb558ce5546c))


### Bug Fixes

* accept upstream v-prefixed pinned tool versions ([38b3637](https://github.com/IlyaGulya/bend2-lsp-rs/commit/38b36372de4937d3de40cddd70a24bf8048963cc))
* harden GitHub Actions and publish direct release binaries ([d9a535a](https://github.com/IlyaGulya/bend2-lsp-rs/commit/d9a535a02154838ad3f11aca658beb01c974cb45))
* preserve native release verification across macOS and Windows ([1f224ba](https://github.com/IlyaGulya/bend2-lsp-rs/commit/1f224ba6828b63fae17136f688ff9488bfb7f251))
* replace policy label gate with PR review and update Node actions ([c2e5fbc](https://github.com/IlyaGulya/bend2-lsp-rs/commit/c2e5fbc582821811f576c37e2c3f999b6f34aebe))
