# Semantic extraction

`bend2/semanticDocument` is an experimental, versioned request for clients that
need written declaration boundaries and resolved source dependencies in addition
to standard LSP navigation and call hierarchy. Support is advertised through
`capabilities.experimental.bend2SemanticDocument`.

## Request and entry selection

```json
{
  "textDocument": {"uri": "file:///project/dep.bend"},
  "rootUri": "file:///project/main.bend"
}
```

`rootUri` is optional and defaults to `textDocument.uri`. It must identify a live
indexed entry: either an open document or a cached source retained by workspace
discovery/import loading. The requested document must belong to that entry's
loaded import graph. Readiness may cold-prepare a missing entry view before the
request reads it; a ready query uses only immutable indexes. This request does
not discover arbitrary files or invent an entry outside the loaded workspace.
Unsaved imported overlays participate in the selected view.

Entry selection matters. Earlier imports can make Base available to a dependency
that does not import it itself. Two entries can implement an imported law with
different bodies. A dependency requested as its own entry can therefore have
unresolved names that resolve through an importing entry.

## Response, schema version 1

- `schemaVersion`: `1`.
- `rootUri`, `viewId`: the selected entry and current server-local view identity.
  View identities are not stable across edits, configuration changes, or restarts.
- `document`: `uri`, `version`, and `contentHash`. Version `-1` denotes a cached
  disk snapshot, not a unique revision. `contentHash` is `fnv1a64:` followed by
  sixteen lowercase hexadecimal digits over the original UTF-8 bytes. This is a
  non-cryptographic provenance aid, not a security checksum or semantic identity.
- `declarations`: written sites in the requested document. Each has `id`, `name`,
  `kind`, `range`, `nameRange`, `signatureRange`, `bodyRange`,
  `documentationRange`, `target`, `signatureOwner`, `bodyOwner`, and `uncertain`.
- `occurrences`: `range`, `fullRange`, `role`, `inType`, `target`, `enclosing`,
  `synthetic`, `uncertain`, and `status` for each indexed written occurrence.
- `complete`: false when the entry has loading/registration issues, the requested
  document has uncertain declarations, or its occurrences remain unresolved or
  ambiguous. This is not a claim that the compiler's type/proof checks passed.

All protocol ranges use zero-based UTF-16 positions and exclusive ends, like LSP.
Byte ranges in the Rust library and differential oracle are converted using the
same immutable document snapshot. Declaration IDs are local to that document's
snapshot and must be interpreted together with the returned view identity.

### Written ranges and documentation

`range` spans the declaration's first syntax token through its last syntax token,
not the following declaration's documentation or trailing whitespace.
`nameRange` covers the complete written declaration name. `signatureRange`
includes the written multiline header, excluding a def/type suite colon.
A law's signature includes its written law declaration and has no body range.
`bodyRange` spans the first through last written body token and is null when no
body is present. A foreign def's written import body is not mistaken for an
ordinary Bend expression.

Documentation is the immediately preceding contiguous block of same-indentation
`#` comments. A blank line or intervening code breaks attachment. Its range
retains original comment markers and Unicode; clients can slice the source at
that range. The final newline is excluded. This attachment rule is an editor
convention, not a compiler-defined semantic property.

`target` is the canonical declaration/binder location, or null when unknown.
`signatureOwner` and `bodyOwner` identify declaration **name sites**, not the
signature/body ranges themselves. They let a client request the corresponding
written site's metadata in the same entry context. A law and its implementation
retain separate written ranges even when they denote one semantic entity.

### Occurrences and dependency roles

Roles are `binding`, `value`, `type`, `constructor`, `call`, and `offload`.
`inType` distinguishes calls occurring in type expressions. `enclosing` is the
canonical enclosing declaration location when known. Together, explicit call
roles, targets, and enclosing declarations describe direct callers/callees.
Value and type dependencies are not automatically runtime calls. Unknown
higher-order callees remain local/unknown; the server does not infer a complete
runtime call graph.

`fullRange` covers an entire dotted name. `range` is the editable terminal name
for qualified global occurrences, preserving import aliases during rename.
Local dotted binders are exact names rather than object prefixes. Synthetic
facts can share a written range; consumers must retain their distinct roles and
must not assume one physical spelling always permits an independent rename.

`status` is `resolved`, `unresolved`, or `ambiguous`; `uncertain` separately
records recovery uncertainty. A null target is not an empty successful binding.

## Standard LSP behavior

Definition, references, rename, hover, signature help, completion, inlay hints,
code lenses, and hierarchies consume the same captured semantic views. Module
path/alias navigation remains distinct from resolving a complete dotted symbol.
Workspace reference results aggregate independently resolved loaded entry views;
they do not construct a hybrid compiler Book.

Call hierarchy items use the effective implementation's document and range when
a body is available. Incoming/outgoing `fromRanges` therefore refer to the
caller's actual file, including implementations of a law declared elsewhere.
Definition and extraction targets still identify the canonical law declaration;
the hierarchy's opaque handle preserves that same entity and entry context.

Incoming calls shared by several entry views appear once only when the physical
caller, call ranges and complete indexed outgoing continuation agree. Different
law implementations remain distinct even when the difference is reached through
another callee. Equivalent results retain the requesting entry's handle when it
is available; this does not assert equivalence of whole compiler Books.

Hierarchy items contain context-qualified `data`. Clients must round-trip it
unchanged. Continuing with a stale item returns LSP `ContentModified` rather than
looking up a new declaration by name. Rename rejects capture or conflicting
interpretations across affected loaded contexts instead of returning partial
edits. Clients advertising workspace `documentChanges` receive versioned edits
for open documents; other clients receive the standard `changes` representation.

Completion preserves the compiler's separate value and constructor namespaces.
A function and constructor can therefore share a label, as with `Base`'s `Pair`
function and a user-defined `Pair{...}` constructor. Local values shadow value
candidates, not constructors. Case patterns still offer only constructors,
filtered by the expected type when it is known.

See [the architecture contract](semantic-architecture.md) for ownership and
publication invariants. Compiler-backed conformance is test-only; the language
server does not execute a second compiler to answer semantic feature queries.

### Editor recovery and compiler diagnostics

The editor keeps best-effort navigation and completion while a document is
unfinished or rejected by the selected compiler. There is no separate strict
mode. This is an intentional editor behavior, not a claim that the compiler
accepted the recovered program.

Selected-compiler errors remain visible through `textDocument/publishDiagnostics`.
Recovery does not suppress or clear them. Diagnostic publications are
asynchronous and versioned: an empty or not-yet-received diagnostic array is not
proof that the current document compiled successfully.

An existing file reached through an import path rejected by the compiler can
still have an editor-only `DocumentLink` or import-path definition. Its link
tooltip and `data.uncertain` distinguish it from a compiler-resolved import.
Opening that file does not add a semantic import edge or establish the meaning
of a member such as `Module.value`.

Recovered declarations and occurrences retain their `uncertain` metadata in
`bend2/semanticDocument`. Ordinary hover and definition results are best-effort;
the standard definition response has no compiler-verification flag. Consumers
may reject results when current-version diagnostics contain errors, but must
not infer compiler acceptance merely from the absence of an error. Even an
extraction response with `complete: true` does not certify compiler type or proof
checks.

Recovery does not relax rename safety: the replacement must be a valid complete
Bend atom for its category, and capture, ambiguity and conflicting entry
interpretations still reject the edit.

