# Semantic analysis contract

## Ownership

The lexical scanner and `LineIndex` feed a grammar-aware, recovery-capable cold
frontend. It constructs immutable compact file facts: declaration sites, scopes,
bindings, structural ranges, and occurrence roles. Temporary parser structures
are released after construction. The formatter keeps its independent scanner.

File facts do not commit nonlocal references to a universal target. A semantic
view resolves them in an entry context, using ordered imports and declaration
registration, input snapshot identities, and the compiler/Base provenance.
Shared source text and local facts are reused between entries.

File-local `SymbolId` links in `SyntaxIndex` identify written sites for standalone
snapshot queries; they are not workspace entity identities. Library dependency
extraction must use `SemanticView` (or `bend2/semanticDocument`), not combine those
links across files or use them as an alternative import resolver.

Standalone reference queries project compact `Reference` headers from complete
`IndexedReference` records. Linking and metadata extraction retain full dotted
names, source segments, roles, lexical binding identities and recovery flags;
ordinary reference-result copies do not carry those additional fields.
`references` and `references_named` expose those headers; metadata consumers use
`indexed_references_named` or the complete indexed-record iterators.
The projected iterator retains exact sizing, reverse iteration, cloning and
fused exhaustion, but is no longer a slice iterator with `as_slice`.
This is a storage/projection boundary, not a second resolver or deferred
semantic scan.

The workspace owns views; feature handlers consume captured immutable views.
Warm queries must not rescan source, reparse imports or headers, or rebuild a
whole-workspace representation. Loading and preparing a view is cold work,
measured separately from ready-view queries.

Definite declaration-name occurrences reuse their snapshot-local `SymbolId`,
with an exact written-range check, instead of searching all declaration sites.
Entry construction retains signature template arity by semantic entity, so
filling a law does not rescan its declaring file. File generations and overlay
conflicts are prepared once per immutable file context. Registration borrows
the shared snapshot rather than cloning document URI/language metadata for
each declaration. These construction-local facts do not add warm-query work.
Final occurrence, reference and call buckets shrink their payloads in place and
retain their original hash tables instead of allocating replacement tables and
rehashing all keys. Cold grouped indexes reuse the final span cursors rather
than allocating separate per-group counts.

Snapshot-local name interning stores source ranges and `NameId` values, not
copied strings. One per-table randomized hash feeds the raw name table directly;
lookups still compare the complete source spelling, including when full hashes
collide. Cached hashes support table growth without rescanning names. The hash
seed and lookup layout are acceleration details, excluded from logical snapshot
equality. Cold construction and warm lookup remain independently measured.

The server keeps its service/request split: `server/lsp.rs` delegates protocol
requests, `server/requests/` projects indexed results, and `WorkspaceService`
owns document revisions, discovery, and workspace publication. Semantic view
preparation belongs to the existing staging lifecycle, not a second document
database or a replacement monolithic backend. `CompilerService` remains
responsible for native checks and compiler-owned Base resources.

Local document reads prepare only their requested entry; document update
acknowledgements prepare missing open entries. Workspace discovery/revision
barriers do not eagerly construct every discovered Book. Reference, rename,
code-lens, and incoming-call queries prepare missing relevant entry contexts
through indexed reverse imports, then reselect against a coherent generation.
Published immutable views are reused; unrelated discovered sources do not force
copies of their semantic environments into a local query.

Completion preserves fuzzy ranking and grammar-indexed lexical scopes, import
paths, and explicit constructor-pattern types. EOF caret bindings have a separate
compact projection, without extending half-open semantic scope ranges. Argument
hints are indexed cold; contextual `as`/`unsafe` coloring follows parser tokens.
Constructor sites remain available in metadata and child outlines without
becoming duplicate top-level workspace symbols.

## Decision and alternatives

| Approach | Decision and trade-off |
| --- | --- |
| Extend line/name heuristics | Rejected. They cannot represent nested binders, declaration order, inherited imports, or multiple implementations of one law. |
| Invoke the compiler for every feature | Rejected for production queries. It adds an external runtime/process dependency and cannot substitute for indexed editing behavior on unfinished input. The compiler remains an independent test oracle. |
| Retain a full typed AST or replace server orchestration | Not required for written dependency extraction. Preserve existing snapshots, transport, loading and publication; lower only reusable semantic facts. Reconsider if a future consumer needs genuinely typed information. |
| Compact cold frontend plus entry views | Chosen. Parsing and binding happen once per snapshot; context-dependent linking happens during view preparation. Warm consumers use indexes and versioned identities. Extra cold work and retained view state must pass the existing performance policy, not be hidden as query speed. |

The cutover boundary is the query layer: standard feature handlers and extraction
share view selection and target identity. Standalone snapshot links remain
file-local facts, not a second workspace resolver. View preparation does not
authorize additional directory discovery, eager preparation of every discovered
file as an entry, or a universal reverse-reference graph across incompatible
entry Books. Existing workspace discovery remains separate.


## Identity and context

A declaration site is a source location, not a semantic entity. A `law` and its
valid implementation have separate sites but can denote one entity. Its
signature and body can belong to different files. Different entry contexts can
select different implementations of the same imported law.

A reference handle includes the view and its generation. A bare name, local
symbol index, or unversioned disk revision is not a stable semantic identity.
Stale hierarchy handles must fail explicitly rather than bind to a replacement
with the same name.

| File identity | Meaning | Use |
| --- | --- | --- |
| Logical URI/import spelling | The editor's document address and each import's resolved spelling | Overlay ownership, navigation, and literal import edits |
| Canonical content path | Full real path, including the final symlink target | Native Book deduplication, shared source facts, and physical invalidation |
| Directory-entry rename path | Canonical parent directory plus the unchanged final filename | Rename through directory symlinks without treating a leaf symlink as its target |

Rename paths and editor-target presence are prepared outside workspace locks.
Native-invalid extensionless/Unicode paths can remain editor links without
acquiring semantic import edges. A live snapshot or a cold verified file proves
availability; an interned missing-file identity does not. Raw target paths remain
watched so creation, deletion, and retargeting invalidate these prepared facts.

A full dotted Bend name is one semantic atom. A local binding named `D` does not
shadow `D.target`. Alias expansion and definition registration follow the pinned
compiler, not object-member heuristics or path-sorted graph iteration.

A position request without an explicit root uses the document as entry. Explicit
root requests and hierarchy continuations retain their selected view. Workspace
aggregation combines independently resolved contexts, never a synthetic union
Book. Conflicting interpretations must not produce a partial successful rename.

## Binding and dependency coverage

This contract targets the checksum-pinned official Bend 2.0.37 source and Base
described in the [conformance guide](semantic-conformance.md), not an unspecified
installed compiler. Its test runtime is Bun 1.4.0 with the same full revision as
the production compiler pin. Source conformance is still not binary equivalence
to distributed executables. The
matrix records exercised rules, not a proof over every program. Permanent case
IDs refer to `tests/fixtures/semantic/corpus.json`; generated family names refer
to `scripts/semantic_generate.py`.

| Construct or transition | Indexed rule | Compiler-backed witness |
| --- | --- | --- |
| Lambda and higher-order callee | Binder visibility ends at its expression boundary; a local function value is not a direct global call. | `original-lambda`, generated `lambda` |
| Sequential local and tuple binding | RHS sees outer names; new binders become visible in the continuation. | `original-let_valid`, `original-pattern`, generated `let`, `pattern` |
| Parallel local binding | All RHS expressions use the previous environment; continuation sees all new binders. | generated `parallel` |
| Match arms and constructor patterns | Arm binders do not leak to siblings. Unknown uppercase names can bind; known constructors occupy their constructor namespace. | `original-pattern`, `uppercase-pattern-binder`, `upstream-dependent-pair` |
| Dependent telescope and quantifier | Earlier parameters are visible in later annotations; a binder does not capture its own domain and is visible in its codomain. | generated `type-binders`, `template`; `upstream-dependent-pair` |
| Erased/empty type telescope | Written parameter sites and inherited constructor annotations retain exact binder origins. | `erased-and-empty-type-telescopes`, `upstream-dependent-pair` |
| Law clauses | `for`, `exs`, and `where` follow native visibility order; a subsequent braced equality is not swallowed as a constructor application. | generated `law-binders`, `law-existential-brace-boundary`, `upstream-exists-where` |
| Typed `do` binding | Annotation and RHS precede the new binding; explicit type origins survive native lowering into monadic calls. | `typed-do-binding-type-origin`, generated `do` |
| Rewrite motive | Implicit `_` has its real `%` origin, distinct from the explicit proof parameter; motive bindings do not leak into the tail. | `upstream-rewrite-motive` |
| Array write and literal annotation | Preserve both old read and new binding at one written span; later uses target the new binding. Literal element type remains a type dependency after lowering. | `upstream-array-write-rebind` |
| Complete dotted atom | Local lookup matches the whole name, not a prefix. Reserved words inside a dotted name do not split it. | `original-local_prefix`, `dotted-keyword-atom`, generated `prefix`, `dotted` |
| Ordered imports and inherited Base | Registration uses the native entry traversal, not filename sorting; a dependency can inherit Base from an earlier import. | `inherited-base`, `original-dependency-control`, generated `ordered-diamond` |
| Law/implementation identity | Keep separate written sites and effective signature/body owners; the importing entry can select a different body. | `original-law_pair`, `proof-context`, `cross-file-law-hierarchy` |
| Native Base laws | Base can fill its own laws; importing project definitions cannot silently replace native claims. | `base-law-body-owner`; workspace rejection regression |
| Homonymous type/constructor | Resolve expression/type and constructor syntax through distinct declaration identities. | `homonymous-type-constructor` |
| Direct and offloaded calls | Keep exact explicit callee ranges; `!(` is adjacent even when the call follows a newline. No inferred runtime higher-order graph. | `original-gpu`, `newline-offload-adjacency`, `noncontiguous-offload-rejected` |
| Same spelling, different file | Source identity includes the file and selected entry; identical bytes are not one declaration. | `same-text-origins`, `proof-context` |
| Source revisions and overlays | Compare exact source origins after Unicode/CRLF/layout edits and unsaved overlays. | `unicode-crlf-fixed`, `unsaved-overlay`, generated layout variants |
| Rejected/unfinished input | Preserve definite structure without inventing a target or counting rejection as valid agreement. | `missing-name`, `ambiguous-alias`, `editor-recovery`, `original-let-recovery`, `incomplete-native-todo` |

Written signature/body/documentation extents are additionally exercised by
`tests/semantic_metadata.rs` and real stdio extraction checks. Comment attachment
is an explicitly documented editor convention, not a native type-system fact.
Lifecycle and stale-handle transitions are exercised through workspace and real
LSP protocol scenarios rather than inferred from a passing parser-only result.

## Publication and recovery

View construction runs outside workspace write locks from captured immutable
inputs. Publication validates all input generations, liveness, topology, and Base
identity. Readiness acknowledges a coherent snapshot/import/view state. Stale
work must not resurrect removed files, outdated imports, or old implementations.
Failed revision staging and poisoned service state remain explicit failures;
they must not become a successful empty semantic result. Native diagnostic
publication and recovery retain their existing independent lifecycle.

Recovery distinguishes known local structure from unresolved or uncertain
semantics. Invalid input must not acquire a guessed target because another
symbol has a matching name. Compiler-error and editor-recovery cases are
separate from compiler-valid differential cases.

## Conformance and resource gates
See the [conformance operating guide](semantic-conformance.md) for exact pins,
commands, failure replay, limits, and the still-required CI calibration.


The test-only oracle observes a pinned real compiler independently of the Rust
resolver. The corpus compares target sites, contexts, roles, ranges, and both
missing and unexpected edges. Scope-aware generation, semantic-preserving
transformations, replay bundles, and bounded shrinking extend that corpus.
Matching static written references and direct calls does not promise a complete
runtime higher-order call graph or proof of equivalence for every future Bend
version.

Product changes require a pre-change Linux Callgrind baseline, separate cold and
warm workloads, real LSP smoke, and the existing quality and performance gates.
No limits or baseline workflows are relaxed by this architecture.

Conformance-runner runtime and memory have their own versioned workload and
budget; compiler/tool installation and compilation are reported separately.
Planned targets in epic `bend2-lsp-rs-rbt.53` are acceptance requirements, not
measurements or permission to drop cases. Incomplete comparison, missing tools,
resource exhaustion, and stale output fail closed.

## Discovery and aggregation boundary

Current workspace discovery already makes unopened Bend files available to
indexed workspace queries and cross-module completion. This cutover preserves
that service and its lifecycle; it does not authorize additional discovery.

Discovering a file does not make every entry's name environment interchangeable.
Entry views follow ordered imports, and workspace-wide aggregation preserves
their separate identities. New eager whole-project semantic preparation or a
global reverse graph would require representative workspace measurements and a
separate resource contract.
