# ADR: explicit maintainer acceptance of measured performance trade-offs

Status: accepted independent maintainer decision, merged in [PR #25](https://github.com/IlyaGulya/bend2-lsp-rs/pull/25); supersedes the earlier retain-without-exceptions proposal.

## Decision

Keep automated measurements honest and unchanged: the 2% Ir gate, cache-event
allowances, baseline workflow, benchmark semantics and retry policy remain.
A failed compare is still a failed compare. Do not retry to green, hide events,
change fixtures, subtract diagnostic costs from official totals, or describe an
unexplained result as noise.

Use three distinct outcomes:

1. **MERGE:** correctness/quality pass and ordinary performance gates pass, or
   the maintainer explicitly accepts a measured, bounded trade-off below.
2. **OPEN / DRAFT:** useful production code with a real unresolved blocker.
3. **DEFERRED & PRESERVED:** quality architecture not needed at current product
   scale, or too expensive now, preserved at immutable references. **DISCARD**
   is reserved for superseded/unsuccessful implementations and one-off tooling.

A small absolute increase in an unrelated tiny workload need not indefinitely
block useful production work. There is deliberately **no new automatic numeric
floor or universal percentage allowance**. Finite calibration does not justify
one. This decision defines explicit review, not an automatic waiver mechanism.

## Required short acceptance report

For each proposed exception publish, on the PR, the exact candidate SHA and
ordinary CI run, user/correctness benefit and maintainability gain, and:

- authoritative Ir baseline/candidate counts, absolute and relative deltas;
- cache-event baseline/candidate counts, absolute and relative deltas;
- paired actual LSP-process latency results and their finite-sample limits;
- binary size when materially changed, and observed memory costs if relevant;
- algorithmic complexity and any added scans, allocations or workspace-size cost.

State which path changed and whether the failing workload is related. Unknown
causality remains unknown. Calibration or code-identical evidence is context,
not proof that a specific failure is random variation. Link raw ordinary-CI
results; no additional research program is required for a few dozen instructions.

The maintainer must explicitly approve the **specific observed trade-off**.
Record that approval publicly before merging a red compare. Required-check
protection may need a maintainer-authorized bounded merge exception: preserve
the failed check and evidence; do not rewrite CI or silently bypass protection.
Approval applies only to the identified candidate/run, not later unrelated code.

## Non-negotiable blockers

Do not accept correctness/lifecycle defects, races or stale snapshots, worsening
complexity, significant memory growth, sustained real-process latency regression,
noticeable primary hot-query regression, unexplained large cold-build growth,
or a cost that increases with workspace size without a separate explicit decision.

Evaluate cold construction separately from warm queries. Immutable snapshots and
SyntaxIndex/workspace-index invariants remain. Formatter lexical scanning stays
separate. No global indexing/discovery is authorized by this decision.

## Existing evidence and its limits

The [isolated-control calibration](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37024700423)
collected 250 full-suite invocations and 8,250 profiles. Both positive controls
exceeded the existing 2% instruction gate in all 45 validation combinations each;
minimum increases were 83.19% for duplicate work and 8.42% for allocation.
Held-out A/A still had two instruction exceedances out of 495 comparisons, and
no cache-event exceedances. These data predate completion PR #23, show sensitivity
to real extra work, and neither train a tiny-query Ir floor nor establish tail
reliability. See [methodology](performance-policy.md#report-only-ci-calibration).

Docs-only [PR #25 run](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37593679341)
reported medium identifier Ir 782→814 (+32, +4.09207%); docs-only
[PR #28 run](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37599373457)
reported 782→810 (+28, +3.58056%). Both reused an already-built bench binary in
the candidate step. Quality/latency passed; compare passed 35/36. They demonstrate
that effectively unchanged-code comparisons can report instruction regressions,
not their cause, nor a generally safe numerical noise boundary. Keep those red
results. No value here is selected to admit #23 or #27.

The repository already records [bounded correctness acceptance for PR #4](performance-policy.md#functional-fix-performance-acceptance-pr-4)
without converting its failed compare to green. This decision makes that explicit
review procedure reusable, with the blockers and per-candidate approval above.

## Integration order

Merge this independent decision first. Then rebase #23 on current main and run
ordinary CI once; review #27 under ordinary CI as a separate maintainability
change. Publish short reports and obtain explicit acceptance for any failed
compare before merge. Keep #28's useful permanent audit, and preserve quality
research implementations without restoring them to production. Do not perform
new microbenchmark archaeology or fit thresholds to those PRs.
