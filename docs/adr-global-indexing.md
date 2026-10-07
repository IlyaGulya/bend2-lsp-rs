# ADR: keep reachable, snapshot-based workspace analysis

Status: accepted architectural direction; quality global-index and discovery
research implementations **DEFERRED & PRESERVED**, not discarded.

## Decision

The merged services/revision refactor [#14](https://github.com/IlyaGulya/bend2-lsp-rs/pull/14)
(`a575cf26`) is the production base. Keep immutable `DocumentSnapshot`, dense
per-document `SyntaxIndex`, and `WorkspaceDb` with import/reverse-import edges.
Index actually loaded/reachable documents, not every file on disk. A global
semantic database and whole-project/background discovery are not requirements.

Do not resume semantic/occurrence/call indexes, hierarchy indexes, memo, CSR,
arenas, or cache-layout tuning merely to future-proof or turn experimental gates
green. Performance thresholds, baselines, benchmark semantics and CI policy stay
unchanged. Experimental PRs are closed without merge; their implementations and
results are preserved as deferred research, not advertised as production features.

## What the measurements established

The experiments covered full semantic/occurrence/call indexes, minimal reverse
references (R), a separate incoming-call candidate (H), memo/cache/storage
layouts, and directory discovery. None is a production prerequisite.

- A reverse occurrence index **with its indexed consumer** removes the explicit
  O(files) references/rename lookup. In synthetic 10k workspaces, R used roughly
  500× fewer sparse lookup+materialization instructions and 52% fewer matched
  instructions; rename improved similarly. These are Callgrind results, not
  measured editor-latency speedups. Building the index while keeping legacy
  consumers does not obtain the query benefit.
- Minimal R still failed unchanged official acceptance: **22/36**, one attempt.
  Initial100 Ir improved 10.98%, but I1mr rose 19.07% and ILmr 17.05%.
  Small completion and inlay warm Ir rose 2.78% and 2.19%, respectively, and
  also failed. No thresholds or baselines were relaxed to accept these costs.
  Matched materialization had a substantial data-locality cost.
- Call indexing is a separate capability, unnecessary for references/rename.
  Incoming benefits from reverse calls; outgoing is naturally snapshot-local.
  This does not justify introducing either index into production now.
- Sparse bucket lookup was not a workspace-size bottleneck: absent/one/three
  sources cost approximately 462/549/659 Ir across 100–10k files. CSR/arenas are
  not warranted by these results. An extra allocation-count explanation for
  completion/inlay regressions was not confirmed: fresh native traces had equal
  request counts (completion: 65 malloc / 3 realloc; inlay: 1 malloc / 5 realloc).
  Allocator-path variation is not evidence of extra allocations or permission
  to tune fixture heaps.

PR #13 was superseded by accepted #14. Discovery #15, full indexing #16 and
minimal R #21 were closed without merge and remain **DEFERRED & PRESERVED**.
Diagnostic PRs #17–#20 were also closed unmerged. H remained an archived branch,
not an accepted PR.

Evidence: [complete seven-variant decomposition](https://github.com/IlyaGulya/bend2-lsp-rs/pull/20#issuecomment-6018737039),
[official R result](https://github.com/IlyaGulya/bend2-lsp-rs/pull/21#issuecomment-6020933985),
[lookup and allocation traces](https://github.com/IlyaGulya/bend2-lsp-rs/blob/d6b4212dffacb1d6346d91e7df1293b7dbd3dc8d/docs/framework-boundary.md).
Existing CI evidence: [seven-variant decomposition](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37465798456),
[official R comparison](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37494981444),
[sparse lookup](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37339668833),
and [allocation traces](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37424805934).
GitHub artifact retention is finite. Raw profiles, tarballs and a private local
forensic archive are optional supporting evidence; this decision does not depend
on their availability. The findings, rejected hypotheses, acceptance outcome
and reopening conditions are recorded here.

## Preserved deferred implementations

These are quality research implementations with substantive behavior and
coverage, not discarded work or production capabilities. **DEFERRED & PRESERVED**
means retain the implementation for reconsideration under the trigger below;
**DISCARD** is not the disposition of these three research heads. Closing PRs
and retiring experiment branches did not erase the implementations.

Public annotated research tags preserve the exact closed heads. Treat these
references as immutable: never move or overwrite them; any later research
revision must receive a new tag. Both tag and full commit links permit inspection
without a private archive or expiring CI artifacts.

| Deferred implementation | Public immutable tag | Exact preserved head |
|---|---|---|
| [#16](https://github.com/IlyaGulya/bend2-lsp-rs/pull/16): semantic/occurrence/call indexing | [`research/semantic-index-v1`](https://github.com/IlyaGulya/bend2-lsp-rs/tree/research/semantic-index-v1) | [`d6b4212dffacb1d6346d91e7df1293b7dbd3dc8d`](https://github.com/IlyaGulya/bend2-lsp-rs/commit/d6b4212dffacb1d6346d91e7df1293b7dbd3dc8d) |
| [#21](https://github.com/IlyaGulya/bend2-lsp-rs/pull/21): minimal reverse occurrence index, with legacy call hierarchy restored | [`research/occurrence-index-v1`](https://github.com/IlyaGulya/bend2-lsp-rs/tree/research/occurrence-index-v1) | [`dbf7d09ba0f2e70c561e7278421f9d03a8835bd8`](https://github.com/IlyaGulya/bend2-lsp-rs/commit/dbf7d09ba0f2e70c561e7278421f9d03a8835bd8) |
| [#15](https://github.com/IlyaGulya/bend2-lsp-rs/pull/15): whole-workspace discovery, including inherited #16 | [`research/workspace-discovery-v1`](https://github.com/IlyaGulya/bend2-lsp-rs/tree/research/workspace-discovery-v1) | [`ec83bc3a060e49cdea0c3d98f7363928b71abbce`](https://github.com/IlyaGulya/bend2-lsp-rs/commit/ec83bc3a060e49cdea0c3d98f7363928b71abbce) |

They were not merged because measured large synthetic-workspace query benefits
did not satisfy unchanged acceptance, including the small-workload and
instruction-cache costs above. Global identity, contribution lifecycle, indexed
consumers and retained storage also add architectural cost; discovery adds
traversal, background-worker ownership, readiness and root/deletion semantics
beyond the loaded/reachable product scope. The product has not established the
real hundreds/thousands-of-files latency need required below. Deferral reflects
those measured tradeoffs and current product scale, not a claim of poor quality.
Preservation does not justify restoring global indexes or discovery, changing
policy gates, or presenting synthetic instruction reductions as editor latency.

## Trigger for reconsideration

Reopen only when **all** are documented:

1. Real Bend workspaces with hundreds/thousands of files expose a reproducible
   references/rename problem, not just a synthetic future workload.
2. Actual editor/LSP-process latency is measured, including representative open
   buffers and dependency changes, and identifies this path as the bottleneck.
3. A concrete real-process performance budget (latency percentiles, workload,
   hardware, cold/update cost and memory constraints) is agreed before proposing
   an index and is demonstrably exceeded by the simpler architecture.

Until then, prioritise useful completion, safe quick fixes and compiler
integration over another global-index experiment. The trigger does not authorise
relaxing the existing performance policy.
