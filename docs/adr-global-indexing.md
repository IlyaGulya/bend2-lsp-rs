# ADR: keep reachable, snapshot-based workspace analysis

Status: accepted architectural direction; global-index research closed.

## Decision

The merged services/revision refactor [#14](https://github.com/IlyaGulya/bend2-lsp-rs/pull/14)
(`a575cf26`) is the production base. Keep immutable `DocumentSnapshot`, dense
per-document `SyntaxIndex`, and `WorkspaceDb` with import/reverse-import edges.
Index actually loaded/reachable documents, not every file on disk. A global
semantic database and whole-project/background discovery are not requirements.

Do not resume semantic/occurrence/call indexes, hierarchy indexes, memo, CSR,
arenas, or cache-layout tuning merely to future-proof or turn experimental gates
green. Performance thresholds, baselines, benchmark semantics and CI policy stay
unchanged. Experimental PRs are closed without merge; their branches/results
remain historical evidence, not production candidates.

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

PR #13 was superseded by accepted #14; discovery #15 was deferred; full indexing
#16 and minimal R #21 were closed without merge. Diagnostic PRs #17–#20 were
also closed unmerged. H remained an archived branch, not an accepted PR.

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
