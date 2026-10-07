# ADR: retain current instruction gates for tiny workloads

Status: maintainer decision selected; integration subject to ordinary CI.

## Decision

Keep the existing 2% instruction regression gate, cache-event allowances,
baseline workflow, benchmark semantics and retry policy unchanged. Do not add
an instruction absolute floor or a PR-specific exception. This resolves the
question raised in [#24](https://github.com/IlyaGulya/bend2-lsp-rs/issues/24)
without changing enforcement. The maintainer selected this conservative option
before integration; the delta in completion PR #23 does not select a threshold.

## Independent evidence and its limits

The existing [isolated-control calibration](https://github.com/IlyaGulya/bend2-lsp-rs/actions/runs/37024700423)
collected 250 full-suite invocations and 8,250 workload profiles. Duplicate-query
and allocation controls exceeded the unchanged instruction gate in all 45
validation workload/series combinations each, with minimum increases of 83.19%
and 8.42%. The held-out A/A set still had two instruction-limit exceedances out
of 495 comparisons. These observations predate #23.

As recorded in [the calibration methodology](performance-policy.md#report-only-ci-calibration),
positive controls and non-inlined measurement helpers target inlay workloads.
The finite sample neither establishes tail reliability nor validates a generic
absolute instruction floor for tiny identifier queries. Report-only process
latency is not an agreed editor-latency budget. There is therefore no independent
basis here for choosing a numeric floor or declaring a failed instruction gate
noise.

## Future policy changes

Any new floor or tiny-workload model needs a separately approved representative
scope and product budget, independent discovery/validation, and positive controls
that demonstrate retained sensitivity on the affected tiny queries. Assess cold,
warm and cache-event costs separately. Numerical values must not be fitted to a
blocked product PR. Enforcement changes require their own reviewed policy PR and
repository policy approval; no calibration or new harness is introduced here.

## Integration consequence

After this decision is merged, rebase #23 on current main and allow one ordinary
CI comparison under the unchanged policy. A previous failure remains a failure;
a new current-head result is not a retroactive waiver or evidence that the old
result was noise. Merge only when current required gates and review permit it.
No further performance archaeology or global-index research is authorized.
