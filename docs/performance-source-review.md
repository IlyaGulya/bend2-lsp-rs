# Performance source identity and gate review

Status: immutable request validation is implemented. Measurement rules are
unchanged. The deeper control experiments below are a proposal, not collected
results or permission to change the gate. This work addresses part of
[issue #79](https://github.com/IlyaGulya/bend2-lsp-rs/issues/79); it does not close
the measurement-quality review or admit a candidate.

## Exact source contract

A pull-request comparison is about one prospective merge, not everything added
to main since the PR was opened:

```text
published refs/pull/N/merge == CANDIDATE_SHA
candidate has exactly two parents
  first parent  == requested BASE_SHA
  second parent == event WORKFLOW_HEAD_SHA
```

The tooling checkout must equal `WORKFLOW_SHA`, belong to the workflow
repository and resolve the requested commits through its published refs. A
changed published PR merge ref invalidates the old request. Commit headers,
not graph traversal, determine parent identity: shallow checkouts hide parents
from ordinary traversal. Commit messages and their encoding have no authority;
a message line saying `parent ...` is not a parent header.

A stale base or head fails before comparison builds and collection. Validation
records the requested and actual identities, retains `validation.json` and the
request manifest, and publishes no measurement authority on failure. It never
silently adopts the merge's first parent or a mutable branch tip as a different
baseline. A rerun retains the original event; obtain a fresh PR event or
explicitly dispatch the intended exact comparison instead.

An explicit manual request remains a named comparison of two exact published
commits. It is not required to be a two-parent PR merge. Existing repository,
workflow-source, mode, scenario and target checks still apply. The current
Callgrind tuple is `callgrind` / `analysis-hot-paths` /
`paired-median-105-v1`; accepting that tuple does not accept arbitrary scenarios
or a different measurement method.

The ordinary performance workflow runs this preflight after durable request
preparation and before baseline/candidate builds. Tool installation and the
small tooling build still occur first. Existing failure finalization and
artifact publication remain responsible for an honest failed result.

## Evidence, not a noise exemption

The [original #79 reproducer](https://github.com/IlyaGulya/bend2-lsp-rs/issues/79)
records an automatic comparison whose event base was `8ba06b1`, while the
prospective merge's first parent was `46cfc39`. That comparison included
already-merged changes rather than isolating the PR. The source check prevents
that request from acquiring authority; it does not reclassify its counters.

A later version-only PR is also useful evidence:

- [Ordinary run 42228122618](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/42228122618)
  and [protected run 42254839577](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/42254839577)
  each collected 105 source pairs over 102 workloads. All `Ir` rows passed;
  18 `I1mr`/`ILmr` rows in 14 workloads were rejected with the same identities.
- The source tree was later released as `d7a95ef`. The owner exception was
  limited to the recorded source and run. The numerical verdict remained a
  regression; it is not permission for a different candidate.
- Collection took approximately 108–136 minutes in those runs; setup took
  approximately 2–3 minutes. These are observations of those hosted runs, not
  an SLA or a prediction for another machine or workload set.

Repeated rejected rows establish reproducibility for that comparison. They do
not establish code-layout causality, harmless noise or editor latency. We did
not invent raw count pairs from normalized or allowance-adjusted reports.

## What each signal protects

| Signal | Purpose | Does not establish |
| --- | --- | --- |
| `Ir` | CPU work performed by the measured production API and its specified teardown | End-to-end editor latency |
| `I1mr`, `ILmr` | Instruction-cache behavior under the pinned simulated cache geometry | Misses on every real CPU, or a semantic cause |
| Native LSP latency | Observed protocol response time in its recorded workload and host | The same workload as a direct-analysis benchmark |
| Native process memory | Kernel-visible process memory in its stated scope | Exact live heap bytes, fragmentation or allocation ownership |

Cold snapshot/index construction and warm indexed queries remain separate.
Current workloads, ownership/teardown, executable inventories, profiler
geometry, metrics and numeric limits remain those in
[the active policy](performance-policy.md) and
[the accepted small-workload ADR](adr-tiny-performance-gates.md).

## Uncertainty and admission

Keep these outcomes distinct:

- `passed`: complete comparable evidence satisfies the active rule.
- `regression`: the active rule rejects a measured row.
- `inconclusive`: retained uncertainty crosses the decision boundary.
- Collection/validation failure: source, raw data or producer evidence is
  missing, invalid or incomplete; it is not an improvement.
- Owner-accepted regression: a separate explicit acceptance of the exact
  recorded trade-off, not a numerical pass.

Ordinary PR runs must not request human approval. Owner review belongs only to
an intentionally requested protected comparison and its exact identity. An
inconclusive row or incomplete collection cannot acquire a regression
exception. A merge decision must not rewrite the numerical report.

The fixed 105-pair method and existing residual/uncertainty rules stay in force.
No retry-until-green, cache-count floor, blanket exclusion, padding, workload
removal or new stopping rule is introduced here.

## Bounded normal and investigation paths

**Normal PR path:** validate exact source first; run the existing fixed-series
comparison once when its workflow is triggered; retain raw profiles, source and
binary manifests, producer outcomes and compact verdicts. Superseded ordinary
work belongs to [the separate CI-lifecycle review](https://github.com/IlyaGulya/bend2-lsp-rs/issues/69).
Do not repeat the same source merely to search for a favorable verdict. This
change does not narrow workflow triggers or reduce coverage.

**Proposed deeper investigation:** an explicit, source-qualified request with a
predeclared maximum of four full collections: a same-binary A/A control, an
independently rebuilt same-source A/A control, a held-out known algorithmic
regression and the intended candidate. Each remains 105 fresh-process pairs
with the existing workloads and hard resource limits. Preserve unsuccessful and
partial controls as well as successes. Do not start this budget automatically
or treat this proposal as maintainer approval to spend it.

Record build, collection, reporting and review time separately, plus target,
toolchain, profiler/cache geometry, binary/source hashes and peak resource use.
A same-binary control tests collection variability; it does not test independent
build or placement sensitivity. Two successful A/A controls do not establish a
small false-positive rate. Report the control count and its uncertainty rather
than claiming an error rate from this small pilot.

A concrete next decision needs independent control outcomes, sensitivity to a
known real algorithmic regression, raw sparse-cache counts alongside
allowance-adjusted residuals, and representative native LSP observations.
Existing archived positive evidence includes the rejected first-match completion
rewrite in [run 41173453522](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/41173453522)
(7.35% more instructions for medium completion and 2.38% for large). That is
historical evidence, not a fresh control collected by this change. Do not
reintroduce that regression into production to create a control.

Any proposal to change sample count, uncertainty handling, metrics, limits,
baseline selection or the baseline workflow must receive explicit maintainer
approval before implementation. Until then, retain the current gate and its
honest outcomes. Compact summaries should survive artifact expiry, with links
to the original evidence and without claiming missing raw data is available.

## Verification boundaries

`tools/perf/tests/workflow_source.rs` exercises the compiled public CLI against
real local Git repositories: matching parents, a stale creation base, wrong
head, non-merge/octopus candidates, a depth-one checkout, encoded commit
messages, current Callgrind mode and explicit manual comparisons. Simulated
workflow metadata in these tests grants validation context only; these are not
hosted performance measurements.

Existing paired-policy/admission contracts cover unchanged-count fixtures,
positive counter changes, isolated outliers, zero counts, missing/duplicate raw
evidence and retained uncertainty. These test the evaluator, not empirical
A/A error rates. Hosted source preflight and the proposed independent control
series require their own recorded runs; a local CLI smoke is not a substitute.
