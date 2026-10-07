# Integration-only salvage audit

## Basis and boundaries

This audit compares actual closed heads with main
`cd1cd95eddc33e3f8ad5eba6e725069e8eb584c8`, not merely GitHub's PR-base diffs:

- [#15](https://github.com/IlyaGulya/bend2-lsp-rs/pull/15):
  `ec83bc3a060e49cdea0c3d98f7363928b71abbce`, **32 differing paths**.
  It inherits the complete #16 head, then adds discovery.
- [#16](https://github.com/IlyaGulya/bend2-lsp-rs/pull/16):
  `d6b4212dffacb1d6346d91e7df1293b7dbd3dc8d`, **27 differing paths**.
- [#21](https://github.com/IlyaGulya/bend2-lsp-rs/pull/21):
  `dbf7d09ba0f2e70c561e7278421f9d03a8835bd8`, **27 differing paths**.
  It retains the occurrence-index experiment but restores legacy call hierarchy.

Main includes [#14](https://github.com/IlyaGulya/bend2-lsp-rs/pull/14)
(`a575cf26efc4ae3296687ffebe3653b79ad50c01`) and
[#22](https://github.com/IlyaGulya/bend2-lsp-rs/pull/22). The latter changed
README and CHANGELOG **as well as** adding `docs/adr-global-indexing.md`.
The ADR's absence from all three closed heads is not a proposed deletion.

No independently missing production correctness fix was established.
EXTRACT below means a focused public-invariant test or a behavior-preserving
cold-code move, not approval to transplant an experimental commit.
The governing decision remains [the global-indexing ADR](adr-global-indexing.md).
Raw research archives are optional provenance, not prerequisites to understand
this decision. No index, discovery service, diagnostic benchmark, layout change,
performance threshold or compatibility shim is extracted.

## Shared substantive changes: #16, inherited #15, and retained #21

The initial commit is
[`49f5899`](https://github.com/IlyaGulya/bend2-lsp-rs/commit/49f5899357d20a9c411b60bd238678ba5c6ff02a).
It mixes module decomposition, semantic indexing, storage changes, handlers,
tests and documentation; **there is no standalone pure-move commit**.

| ID / change and origin | Problem addressed | Equivalent in audited main | Global/index/discovery dependency | Independent correctness value | Independent maintainability value | Regression coverage | Verdict |
|---|---|---|---|---|---|---|---|
| S1 GlobalSymbolId, per-file epochs, prepared semantic snapshots, external occurrence groups; 49f5899 | Stable cross-file identity and reusable reverse lookup | Local SymbolId/NameId, immutable snapshots and reachable workspace resolution | Global identity + semantic/reverse index | No missing main invariant demonstrated | Only with the deferred representation | Experiment epoch/group tests depend on new API | **DEFER BY ADR** |
| S2 Global incoming/outgoing call groups and handler rewiring; 49f5899 | Avoid repeated cross-file call resolution | Existing hierarchy resolver and indexed snapshot calls | Global call index | Existing hierarchy behavior already covered | Coupled to deferred index | Existing public call/type hierarchy tests; experiment group tests | **DEFER BY ADR**; #21 removes this part, see R1 |
| S3 Prepared snapshot plumbing through compiler registration, close and disk updates; 49f5899 | Commit new semantic contributions consistently | WorkspaceService commit validation and snapshot updates | PreparedSemanticSnapshot | Existing close/revision guard is not a new fix | New plumbing has no purpose without index | Existing close/reopen and overlay lifecycle tests | **ALREADY MERGED** for invariant; **DEFER BY ADR** for plumbing |
| S4 clear_compiler_base on configuration changes; 49f5899 | Invalidate newly cached resolved Base targets | didChangeConfiguration already clears base_module and base_module_attempted; loader installs only successful nonempty Base | Semantic cache only | No missing production invalidation established | No extra cache to clear on main | Existing Base provenance/profile tests; S10 adds failure coverage | **ALREADY MERGED** for main behavior; **DEFER BY ADR** for new-cache hook |
| S5 Cold scanner and declaration decomposition; 49f5899 | Separate cold construction from query/storage code | All bodies exist in monolithic syntax.rs | None when re-authored from main | Behavior preserved, not a fix | Two private cold boundaries reduce coupling without moving warm NameTable or changing data | Existing syntax, constructor, binding, hierarchy and semantic-token tests; actual baseline/refactor stdio equality | **EXTRACT**, [#27](https://github.com/IlyaGulya/bend2-lsp-rs/pull/27); not the mixed commit |
| S6 Remaining seven-module split: builder/calls/references/names/delimiters; 49f5899 | Further file decomposition | Existing main bodies and root data types | Shipped builder/calls/references/names contain representation/counter changes | None independently demonstrated | A pure move is possible, but unnecessary extra boundaries for this integration | Existing feature coverage | **DISCARD** shipped variants; retain delimiter inside extracted scanner |
| S7 NameCandidates, direct ordinal symbol_by_name, FileSet membership and import_by_range; 49f5899 | Reduce allocation and lookup overhead | HashMap/HashSet and existing import edges; equivalent full-edge dirty comparison | Storage/layout experiments; some are mechanically independent | No correctness defect demonstrated | No required coupling reduction | Existing name/import/overlay tests | **DISCARD**, out-of-scope microperformance changes |
| S8 folding trim to trim_start; 49f5899 | Avoid trailing trim work for blank-line check | Existing behavior-equivalent folding implementation | None | No behavior fix | No coupling reduction | Existing folding boundary/EOF tests | **DISCARD**, performance-only |
| S9 Semantic workspace tests: local/self imports, external alias/caller groups, snapshot/language lifecycle, retired epochs; 49f5899 + e39c9b0 | Validate new index representation and lifecycle | Existing public snapshot, shadowing, import and lifecycle tests | Global IDs, group APIs and/or index stats | Behavioral portions overlap existing tests | Representation-specific assertions cannot guard main | local_and_self_imported_calls_merge_and_follow_snapshot_reordering; external_alias_groups_preserve_noncall_ranges_and_caller_boundaries; local_queries_follow_snapshot_and_language_lifecycle; external_group_lookup_rejects_same_name_from_retired_epoch | **DISCARD** index-specific fixtures; existing invariants **ALREADY MERGED** |
| S10 Base reload failure/empty-output and recovery fixture; 49f5899 | Detach a failed prelude while retaining old generated source navigation | Success-only provenance/reopen and configured profile coverage existed; failed/empty transition was not explicitly guarded | Public definition/navigation subset does not require index; original fixture also exercises indexed relations | New test coverage for existing behavior, not a production fix | Focused fixture reuses existing stdio helpers | New failed_and_empty_base_reload_detach_prelude_and_allow_recovery: failure and empty modes, definition/completion detach, recovery, retained old bytes/navigation | **EXTRACT** focused public invariant, [#26](https://github.com/IlyaGulya/bend2-lsp-rs/pull/26); do not transplant entire mixed fixture |
| S11 Compact external occurrence/caller storage; dcbec8c | Reduce index cost | No corresponding global storage on main | Semantic/call storage | None for main | No independent benefit | Experimental index tests/benchmarks | **DISCARD** implementation; cost conclusions retained by ADR |
| S12 Discard cold targets and use direct reference ordinals; 793f396 | Reduce cold retained data and reference work | Existing snapshot reference representation | Reverse-index layout | None independently demonstrated | No independent boundary reduction | Experimental lookup profiles | **DISCARD** |
| S13 Cache local semantic totals and retain external contributions only; e39c9b0 | Reduce aggregate query/count work | Existing warm queries; no new cached totals | Snapshot counters, token capacity and semantic contribution layout | None | Performance-only | Cold/warm experimental benchmarks | **DISCARD** |
| S14 Exact-length ordinal slice materialization; e2ffed8 | Avoid over-allocation/copying in indexed references | Existing legacy reference materialization | Reverse index | None | Performance-only | Experimental lookup/output profiles | **DISCARD** |
| S15 Allocation-free lookup diagnostic benchmark; 64d0914 | Attribute external lookup overhead | Ordinary production benchmark remains | Experimental lookup API + benchmark | None | Diagnostic harness, not product | Diagnostic profile only | **DISCARD** |
| S16 Staged cold reference candidate layout; 48b7a018, reverted by 652bfd91 | Try a compact cold candidate representation | No candidate-column experiment | Semantic/layout experiment | None | Rejected; no surviving head change | Recorded rejected cold result | **DISCARD**, already reverted in closed history |
| S17 Lookup isolation/rejected-layout evidence; 44fafe0 | Record measurement conclusions | Self-contained merged ADR records decision and evidence | Research documentation | No code fix | Decision preserved without giant experiment log | ADR contains evidence and reopen conditions | **ALREADY MERGED** conclusions; **DISCARD** experimental document |
| S18 Production reference conversion adapter and diagnostic stages; b376762 | Separate lookup/output/protocol attribution | Existing handlers return LSP locations | WorkspaceOccurrence/global index + stage harness | None | Adapter is dead without index; not an independent clean boundary | Diagnostic reference_stages profiles | **DISCARD** adapter/harness; deferred consumer rewrite belongs to S1 |
| S19 Exact benchmark entry-point fences; 74adbf2 | Make diagnostic stage attribution stable | Existing ordinary benchmark gate | Diagnostic wrappers/inline fences | None | No product value | Diagnostic profiles | **DISCARD** |
| S20 Materialize only documents returned by indexed references; 17e64d9 | Avoid unrelated document materialization | Existing legacy reference path | Indexed reference output | No missing correctness invariant | Performance-only | Experimental output profiles | **DISCARD** |
| S21 Output-stage evidence/materialization savings; 84dd42a | Record experiment outcome | Merged ADR's decision/evidence summary | Research only | None | Conclusions, not machinery | ADR evidence | **ALREADY MERGED** conclusions; **DISCARD** raw-stage narrative |
| S22 Consecutive cold import-module memoization; 8ce7f7c (#16/#15 only) | Avoid repeated cold resolution | Existing uncached cold import resolution | Memo/layout performance work | None | No independent coupling reduction | Cold construction profiles | **DISCARD** |
| S23 Experimental feature claims and final framework-boundary narrative; d6b4212 and shared README/CHANGELOG edits | Document proposed indexed capabilities | README/CHANGELOG and ADR reflect merged scope after #22 | Claims require unmerged index | Not a fix | Accepted scope already documented | Documentation review against main | **DISCARD** proposed capability claims; accepted decision **ALREADY MERGED** |
| R1 Reverse-only isolation; dbf7d09 (#21 only) | Remove call-index regression while retaining occurrence index | Main already uses legacy hierarchy and has no global occurrence index | Global reverse index remains | Restored behavior already exists on main | Useful only inside rejected experiment | Experimental references/rename and adjusted local lifecycle test | **DEFER BY ADR** remaining occurrence index; **DISCARD** isolation patch, not an independent main refactor |

## Discovery-only substantive changes in #15

All rows originate in
[`ec83bc3`](https://github.com/IlyaGulya/bend2-lsp-rs/commit/ec83bc3a060e49cdea0c3d98f7363928b71abbce).
The entire S table above is also inherited by #15; its 13-file incremental PR
diff is not its complete difference from main.

| ID / change | Problem addressed | Equivalent in audited main | Global/index/discovery dependency | Independent correctness value | Independent maintainability value | Regression coverage | Verdict |
|---|---|---|---|---|---|---|---|
| D1 DiscoveryService worker, scan/readiness/rediscovery and roots | Find unopened workspace importers | Main loads open documents and reachable imports, intentionally not whole project | Whole-project discovery + inherited index | New scope, not a missing main invariant | No independent service without feature | D6 tests | **DEFER BY ADR** |
| D2 ignore dependency and Cargo.lock additions | Respect ignore rules during discovery | No whole-project traversal | Discovery-only | None | Unneeded dependency | Discovery fixtures | **DISCARD** |
| D3 initialize/initialized/workspace-folder/watch scheduling, query wait barrier and update/delete tracking | Keep discovered roots current and queries ready | Existing registration, watched-file updates and reachable import refresh | DiscoveryService and global contribution lifecycle | Existing watched/reachable behavior already covered | No independent wiring | D6 tests | **DEFER BY ADR** |
| D4 Discovery cancellation in diagnostics/transport shutdown | Drain new background worker | Existing main cancels/drains its diagnostics handles and compiler children | Discovery task only | Does not repair existing task ownership | No task exists to drain on main | Existing shutdown tests; discovery runtime | **ALREADY MERGED** main invariant; **DISCARD** extra hook |
| D5 discovered_roots activation/deactivation/tombstones; inactive helper and cosmetic WorkspaceService edits | Preserve discovery membership over deletion/restoration | Main close/sync_disk_snapshot preserves open overlays and restores/deletes reachable disk sources | Discovery roots + semantic activation | No independently missing disk/overlay fix established | New membership has no main purpose; dead helper is not salvage | D6 tests and existing main overlay tests | **DEFER BY ADR** root mechanism; **DISCARD** unused helper/cosmetics |
| D6a workspace_references_include_unopened_importers_and_remove_deleted_files | References from unopened importers | Deliberately outside loaded/reachable scope | Discovery + global references | New scope only | None without feature | Named test | **DEFER BY ADR** behavior; **DISCARD** fixture for main |
| D6b rediscovery_tombstones_deleted_dependency_without_watched_file_event | Infer deletion during rescan | Main uses watched updates/reachable loading, not rescan | Discovery | No main event-contract defect demonstrated | None independently | Named test | **DEFER BY ADR** behavior; **DISCARD** fixture |
| D6c rediscovery_preserves_open_overlay_until_recreated_disk_is_restored_on_close | Preserve overlay through discovery rescan | Main already preserves open overlays/restores disk on close | Discovery rescan is new; overlay invariant exists | Existing overlay invariant already guarded | No independent new mechanism | Named test plus existing main overlay tests | **ALREADY MERGED** overlay invariant; **DISCARD** discovery fixture |
| D6d rediscovery_keeps_existing_ignored_explicit_import_but_tombstones_proven_deletion | Distinguish ignored files from confirmed deletion | Main resolves explicit/reachable imports without ignore traversal | Discovery ignore/deletion semantics | New contract only | None without discovery | Named test | **DEFER BY ADR** behavior; **DISCARD** fixture |
| D6e rediscovery_does_not_infer_deletion_inside_removed_workspace_root | Limit rescan deletion inference to scanned roots | No root rescan on main | Multi-root discovery | New contract only | None without discovery | Named test | **DEFER BY ADR** behavior; **DISCARD** fixture |
| D7 README/CHANGELOG unopened-project claims and discovery completion | Describe experimental feature | Main retains explicit reachable scope and deferred roadmap | Discovery | None | Would misrepresent merged capabilities | Documentation review | **DISCARD** |

## Complete path coverage ledger

The shared 27-path ledger applies to #16 and #21 and is inherited by #15.
R1 additionally changes CHANGELOG, framework-boundary, orchestration, hierarchy,
workspace_tests, workspace and semantic in #21. No row authorizes wholesale
file transplantation.

| Path | Substantive rows |
|---|---|
| CHANGELOG.md | S23; R1; D7 for #15 |
| README.md | S23; D7 for #15 |
| benches/analysis.rs | S13–S15, S18–S20 |
| benches/support/reference_stages.rs | S18–S19 |
| docs/adr-global-indexing.md | **ALREADY MERGED**; keep main-only decision, absent closed heads |
| docs/framework-boundary.md | S17, S21, S23; R1 |
| src/analysis.rs | S8, S13 |
| src/analysis/syntax.rs | S5–S7, S13 |
| src/analysis/syntax/builder.rs | S6–S7, S13 |
| src/analysis/syntax/calls.rs | S2, S6, S13 |
| src/analysis/syntax/declarations.rs | S5 |
| src/analysis/syntax/names.rs | S6–S7 |
| src/analysis/syntax/references.rs | S1, S6–S7, S13 |
| src/analysis/syntax/scanner.rs | S5 |
| src/analysis/syntax/scanner/delimiters.rs | S5–S6; keep within the extracted scanner |
| src/server/compiler_service.rs | S3–S4 |
| src/server/lsp.rs | S4; D3 for #15 |
| src/server/mod.rs | S18; D1/D3 for #15 |
| src/server/orchestration.rs | S1–S3, S18, S20; R1; D3 for #15 |
| src/server/reference_locations.rs | S18 |
| src/server/requests/hierarchy.rs | S2; R1 |
| src/server/requests/references.rs | S1, S18 |
| src/server/workspace_service.rs | S3; D5 for #15 |
| src/server/workspace_tests.rs | S9; R1 |
| src/workspace.rs | S1–S4, S7, S13; R1; D5 for #15 |
| src/workspace/semantic.rs | S1–S2, S11–S14, S16, S20, S22; R1 |
| tests/lsp_protocol.rs | S10; D6 for #15 |

#15 adds these five paths to the ledger, totaling **32**, not 31:

| Additional path | Substantive rows |
|---|---|
| Cargo.lock | D2 |
| Cargo.toml | D2 |
| src/server/diagnostics.rs | D4 |
| src/server/discovery.rs | D1 |
| src/server/transport.rs | D4 |

## Lifecycle comparison against main, not experimental APIs

| Area | Main equivalent and boundary | Audit outcome |
|---|---|---|
| Revision, queued close, reopen | revision.rs generation/closed_through state; WorkspaceService::commit_close validates epoch/URI and disk Arc identity before committing, returns RetryDisk on replaced disk snapshot | #14 already supplies invariant; moving the same guard into close_document_prepared is not a new fix |
| Compiler/Base configuration | Configuration clears base_module/base_module_attempted; compiler loader installs only successful nonempty output; generated Base provenance/reopen and compiler profile tests already exist | No missing invalidation fix; #26 guards previously unasserted failed/empty transitions using current public APIs |
| Disk/open overlay, deletion/restoration | Workspace close_document/sync_disk_snapshot and watched changes maintain open overlay priority and restore disk snapshot or tombstone reachable deleted file | Already merged for main scope; do not import discovery-specific deletion inference |
| Cancellation, shutdown and task ownership | #14 service ownership, admission rejection, fatal-state handling, diagnostics cancellation/drain and compiler-child reaping | #16/#21 do not change revision/state/transport/diagnostics; #15 adds ownership only for its new discovery worker |
| Imported diagnostic cleanup | Existing diagnostics ownership/supersession cleanup, including blocked_superseded_import_diagnostics_do_not_restore_stale_errors and generated Base diagnostic assertions | Already merged; no diagnostics correctness patch identified in closed heads |

## Independent extraction evidence

- **#26**, test-only, based on audited main: targeted new test passed;
  `./scripts/quality` passed, **120 passed / 1 skipped**. Actual stdio server
  exercised initial binding, failed detachment, recovery, empty detachment,
  second recovery, retained old source navigation/bytes, completion removal and
  restoration, and shutdown exit 0 with a configured fixture compiler.
- **#27**, two private cold modules built from main bodies: `./scripts/quality`
  passed, **119 passed / 1 skipped**. Actual baseline and refactored stdio
  binaries returned identical documentSymbol, definition, hover, foldingRange,
  formatting and semanticTokens results. Checked constructor ownership, target
  range and UTF-16 parameter/declaration/Unicode-string/comment/number tokens;
  both shutdown exits were 0. No new permanent implementation-detail test.

Each extraction must pass its ordinary PR CI, including the existing
performance gate, before merge. The PR pages are authoritative for final CI and
merge state; this document does not claim an unobserved green gate or merge.
There is no waiver, benchmark variant, retry-to-green or global machinery in
either extraction. #24/#25 policy handling and blocked #23 remain independent;
this audit does not authorize touching #23 before a merged policy decision.
