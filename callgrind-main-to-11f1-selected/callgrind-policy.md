# Paired Callgrind admission

Method: `paired-median-105-v1`. Status: **regression**. Each comparison uses 105 fresh-process pairs on one host. Bounds are normalized allowance-adjusted residuals, not editor latency or synthetic count pairs.

| Workload | Event | Samples | Median residual | Lower bound | Upper bound | Classification |
| --- | --- | ---: | --- | --- | --- | --- |
| ascii_offset_conversion [prewarmed] | Ir | 105 | -1/61 | -1/61 | -1/61 | passed |
| ascii_offset_conversion [prewarmed] | I1mr | 105 | -3/3 | -3/3 | -3/3 | passed |
| ascii_offset_conversion [prewarmed] | ILmr | 105 | -3/3 | -3/3 | -3/3 | passed |
| ascii_position_conversion [prewarmed] | Ir | 105 | -2/141 | -2/141 | -2/141 | passed |
| ascii_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| ascii_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| call_hierarchy_warm [large] | Ir | 105 | -258941/452404 | -258941/452404 | -258941/452404 | passed |
| call_hierarchy_warm [large] | I1mr | 105 | 17/17 | 17/17 | 17/17 | regression |
| call_hierarchy_warm [large] | ILmr | 105 | -8/13 | -8/13 | -8/13 | passed |
| call_hierarchy_warm [medium] | Ir | 105 | -86188/151686 | -86188/151686 | -86188/151686 | passed |
| call_hierarchy_warm [medium] | I1mr | 105 | 20/18 | 20/18 | 20/18 | regression |
| call_hierarchy_warm [medium] | ILmr | 105 | -8/13 | -8/13 | -8/13 | passed |
| call_hierarchy_warm [small] | Ir | 105 | -980/2003 | -980/2003 | -980/2003 | passed |
| call_hierarchy_warm [small] | I1mr | 105 | 7/20 | 7/20 | 7/20 | regression |
| call_hierarchy_warm [small] | ILmr | 105 | -14/19 | -14/19 | -14/19 | passed |
| cold_completion_context_build [default] | Ir | 105 | -12725/66701 | -12724/66659 | -12722/66701 | passed |
| cold_completion_context_build [default] | I1mr | 105 | 168/675 | 168/675 | 168/675 | regression |
| cold_completion_context_build [default] | ILmr | 105 | 151/648 | 151/648 | 151/648 | regression |
| cold_snapshot_build_large [default] | Ir | 105 | -12138787/28556535 | -12138856/28555978 | -12138533/28556273 | passed |
| cold_snapshot_build_large [default] | I1mr | 105 | 544/758 | 544/758 | 544/758 | regression |
| cold_snapshot_build_large [default] | ILmr | 105 | 115/673 | 115/673 | 115/673 | regression |
| cold_snapshot_build_medium [default] | Ir | 105 | -1789590/7128385 | -1789855/7129170 | -1789406/7128018 | passed |
| cold_snapshot_build_medium [default] | I1mr | 105 | 402/776 | 402/776 | 402/776 | regression |
| cold_snapshot_build_medium [default] | ILmr | 105 | 102/679 | 102/679 | 102/679 | regression |
| cold_snapshot_build_small [default] | Ir | 105 | -120678/665315 | -120739/665443 | -120675/665452 | passed |
| cold_snapshot_build_small [default] | I1mr | 105 | 524/886 | 524/886 | 524/886 | regression |
| cold_snapshot_build_small [default] | ILmr | 105 | 195/827 | 195/827 | 195/827 | regression |
| completion_warm [large] | Ir | 105 | -14227/360362 | -14227/360362 | -14227/360362 | passed |
| completion_warm [large] | I1mr | 105 | 5/50 | 5/50 | 5/50 | regression |
| completion_warm [large] | ILmr | 105 | -1/31 | -1/31 | -1/31 | passed |
| completion_warm [medium] | Ir | 105 | 693/110396 | 693/110396 | 693/110396 | regression |
| completion_warm [medium] | I1mr | 105 | 4/48 | 4/48 | 4/48 | regression |
| completion_warm [medium] | ILmr | 105 | 0/30 | 0/30 | 0/30 | passed |
| completion_warm [small] | Ir | 105 | -220/19757 | -220/19757 | -220/19757 | passed |
| completion_warm [small] | I1mr | 105 | -11/47 | -11/47 | -11/47 | passed |
| completion_warm [small] | ILmr | 105 | -11/39 | -11/39 | -11/39 | passed |
| constructor_completion_warm [empty] | Ir | 105 | -309/1240 | -309/1240 | -309/1240 | passed |
| constructor_completion_warm [empty] | I1mr | 105 | -1/36 | -1/36 | -1/36 | passed |
| constructor_completion_warm [empty] | ILmr | 105 | -3/32 | -3/32 | -3/32 | passed |
| constructor_completion_warm [prefix] | Ir | 105 | -318/1286 | -318/1286 | -318/1286 | passed |
| constructor_completion_warm [prefix] | I1mr | 105 | -1/36 | -1/36 | -1/36 | passed |
| constructor_completion_warm [prefix] | ILmr | 105 | -4/33 | -4/33 | -4/33 | passed |
| constructor_completion_warm [subsequence] | Ir | 105 | -193/1079 | -193/1079 | -193/1079 | passed |
| constructor_completion_warm [subsequence] | I1mr | 105 | 1/36 | 1/36 | 1/36 | regression |
| constructor_completion_warm [subsequence] | ILmr | 105 | -2/33 | -2/33 | -2/33 | passed |
| constructor_definition_warm [prewarmed] | Ir | 105 | -282/555 | -282/555 | -282/555 | passed |
| constructor_definition_warm [prewarmed] | I1mr | 105 | 6/9 | 6/9 | 6/9 | regression |
| constructor_definition_warm [prewarmed] | ILmr | 105 | -2/8 | -2/8 | -2/8 | passed |
| folding_10000_lines [ten_thousand_lines] | Ir | 105 | -661143/1454954 | -661143/1454954 | -661143/1454954 | passed |
| folding_10000_lines [ten_thousand_lines] | I1mr | 105 | -6/34 | -6/34 | -6/34 | passed |
| folding_10000_lines [ten_thousand_lines] | ILmr | 105 | -16/34 | -16/34 | -16/34 | passed |
| folding_1000_lines [thousand_lines] | Ir | 105 | -66290/146363 | -66290/146363 | -66290/146363 | passed |
| folding_1000_lines [thousand_lines] | I1mr | 105 | -10/36 | -10/36 | -10/36 | passed |
| folding_1000_lines [thousand_lines] | ILmr | 105 | -10/28 | -10/28 | -10/28 | passed |
| folding_100_lines [hundred_lines] | Ir | 105 | -5567/15372 | -5567/15372 | -5567/15372 | passed |
| folding_100_lines [hundred_lines] | I1mr | 105 | -11/36 | -11/36 | -11/36 | passed |
| folding_100_lines [hundred_lines] | ILmr | 105 | -9/28 | -9/28 | -9/28 | passed |
| identifier_ranges_warm [large] | Ir | 105 | -220/605 | -220/605 | -220/605 | passed |
| identifier_ranges_warm [large] | I1mr | 105 | 13/14 | 13/14 | 13/14 | regression |
| identifier_ranges_warm [large] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [medium] | Ir | 105 | -216/601 | -216/601 | -216/601 | passed |
| identifier_ranges_warm [medium] | I1mr | 105 | 16/11 | 16/11 | 16/11 | regression |
| identifier_ranges_warm [medium] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [small] | Ir | 105 | -215/590 | -215/590 | -215/590 | passed |
| identifier_ranges_warm [small] | I1mr | 105 | 18/7 | 18/7 | 18/7 | regression |
| identifier_ranges_warm [small] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| import_completion_candidates_warm [exact] | Ir | 105 | -7246/128060 | -7248/128056 | -7242/128056 | passed |
| import_completion_candidates_warm [exact] | I1mr | 105 | -22/130 | -23/131 | -22/131 | passed |
| import_completion_candidates_warm [exact] | ILmr | 105 | -10/93 | -10/93 | -10/93 | passed |
| import_completion_candidates_warm [fuzzy] | Ir | 105 | -6610/130290 | -6631/130290 | -6581/130234 | passed |
| import_completion_candidates_warm [fuzzy] | I1mr | 105 | -20/133 | -21/133 | -20/133 | passed |
| import_completion_candidates_warm [fuzzy] | ILmr | 105 | -9/95 | -9/95 | -9/95 | passed |
| import_completion_candidates_warm [no_match] | Ir | 105 | -18882/28619 | -18882/28619 | -18882/28619 | passed |
| import_completion_candidates_warm [no_match] | I1mr | 105 | -11/34 | -11/34 | -11/34 | passed |
| import_completion_candidates_warm [no_match] | ILmr | 105 | -17/34 | -17/34 | -17/34 | passed |
| inlay_hints_warm [large] | Ir | 105 | -1094/58394 | -1094/58394 | -1094/58394 | passed |
| inlay_hints_warm [large] | I1mr | 105 | -4/7 | -4/7 | -4/7 | passed |
| inlay_hints_warm [large] | ILmr | 105 | -4/7 | -4/7 | -4/7 | passed |
| inlay_hints_warm [medium] | Ir | 105 | -296/20186 | -296/20186 | -296/20186 | passed |
| inlay_hints_warm [medium] | I1mr | 105 | -5/9 | -5/9 | -5/9 | passed |
| inlay_hints_warm [medium] | ILmr | 105 | -4/7 | -4/7 | -4/7 | passed |
| inlay_hints_warm [small] | Ir | 105 | -337/1812 | -337/1812 | -337/1812 | passed |
| inlay_hints_warm [small] | I1mr | 105 | -11/14 | -11/14 | -11/14 | passed |
| inlay_hints_warm [small] | ILmr | 105 | -10/13 | -10/13 | -10/13 | passed |
| parameter_annotation_warm [builtin] | Ir | 105 | -294/308 | -294/308 | -294/308 | passed |
| parameter_annotation_warm [builtin] | I1mr | 105 | -24/23 | -24/23 | -24/23 | passed |
| parameter_annotation_warm [builtin] | ILmr | 105 | -23/22 | -23/22 | -23/22 | passed |
| parameter_annotation_warm [nested] | Ir | 105 | -354/367 | -354/367 | -354/367 | passed |
| parameter_annotation_warm [nested] | I1mr | 105 | -23/22 | -23/22 | -23/22 | passed |
| parameter_annotation_warm [nested] | ILmr | 105 | -22/21 | -22/21 | -22/21 | passed |
| parameter_annotation_warm [qualified] | Ir | 105 | -359/372 | -359/372 | -359/372 | passed |
| parameter_annotation_warm [qualified] | I1mr | 105 | -24/23 | -24/23 | -24/23 | passed |
| parameter_annotation_warm [qualified] | ILmr | 105 | -23/22 | -23/22 | -23/22 | passed |
| pattern_completion_context_warm [nested] | Ir | 105 | -1/52 | -1/52 | -1/52 | passed |
| pattern_completion_context_warm [nested] | I1mr | 105 | -3/4 | -3/4 | -3/4 | passed |
| pattern_completion_context_warm [nested] | ILmr | 105 | -3/4 | -3/4 | -3/4 | passed |
| reference_location_order_warm [large] | Ir | 105 | -284336/14216843 | -284336/14216843 | -284336/14216843 | passed |
| reference_location_order_warm [large] | I1mr | 105 | -1/133 | -1/133 | -1/133 | passed |
| reference_location_order_warm [large] | ILmr | 105 | -3/133 | -3/133 | -3/133 | passed |
| reference_location_order_warm [medium] | Ir | 105 | -17470/873518 | -17470/873518 | -17470/873518 | passed |
| reference_location_order_warm [medium] | I1mr | 105 | 0/138 | 0/138 | 0/138 | passed |
| reference_location_order_warm [medium] | ILmr | 105 | -4/138 | -4/138 | -4/138 | passed |
| reference_location_order_warm [small] | Ir | 105 | -681/34077 | -681/34077 | -681/34077 | passed |
| reference_location_order_warm [small] | I1mr | 105 | -2/105 | -2/105 | -2/105 | passed |
| reference_location_order_warm [small] | ILmr | 105 | -3/105 | -3/105 | -3/105 | passed |
| references_warm [large] | Ir | 105 | -28826/106817 | -28826/106817 | -28826/106817 | passed |
| references_warm [large] | I1mr | 105 | 25/13 | 25/13 | 25/13 | regression |
| references_warm [large] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [medium] | Ir | 105 | -9481/36499 | -9481/36499 | -9481/36499 | passed |
| references_warm [medium] | I1mr | 105 | 28/14 | 28/14 | 28/14 | regression |
| references_warm [medium] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [small] | Ir | 105 | -657/1797 | -657/1797 | -657/1797 | passed |
| references_warm [small] | I1mr | 105 | 15/16 | 15/16 | 15/16 | regression |
| references_warm [small] | ILmr | 105 | -6/15 | -6/15 | -6/15 | passed |
| scoped_completion_warm [large] | Ir | 105 | -57606/408394 | -57606/408394 | -57606/408394 | passed |
| scoped_completion_warm [large] | I1mr | 105 | 10/90 | 10/90 | 10/90 | regression |
| scoped_completion_warm [large] | ILmr | 105 | 1/68 | 1/68 | 1/68 | regression |
| scoped_completion_warm [medium] | Ir | 105 | -13962/131845 | -13962/131845 | -13962/131845 | passed |
| scoped_completion_warm [medium] | I1mr | 105 | 6/89 | 6/89 | 6/89 | regression |
| scoped_completion_warm [medium] | ILmr | 105 | -1/70 | -1/70 | -1/70 | passed |
| scoped_completion_warm [small] | Ir | 105 | -2311/32179 | -2311/32179 | -2311/32179 | passed |
| scoped_completion_warm [small] | I1mr | 105 | 4/87 | 4/87 | 4/87 | regression |
| scoped_completion_warm [small] | ILmr | 105 | -7/76 | -7/76 | -7/76 | passed |
| semantic_tokens_warm [large] | Ir | 105 | -248110/456332 | -248110/456332 | -248110/456332 | passed |
| semantic_tokens_warm [large] | I1mr | 105 | -11/14 | -11/14 | -11/14 | passed |
| semantic_tokens_warm [large] | ILmr | 105 | -11/14 | -11/14 | -11/14 | passed |
| semantic_tokens_warm [medium] | Ir | 105 | -82687/152835 | -82687/152835 | -82687/152835 | passed |
| semantic_tokens_warm [medium] | I1mr | 105 | -12/16 | -12/16 | -12/16 | passed |
| semantic_tokens_warm [medium] | ILmr | 105 | -11/14 | -11/14 | -11/14 | passed |
| semantic_tokens_warm [small] | Ir | 105 | -9174/16083 | -9174/16083 | -9174/16083 | passed |
| semantic_tokens_warm [small] | I1mr | 105 | -18/21 | -18/21 | -18/21 | passed |
| semantic_tokens_warm [small] | ILmr | 105 | -17/20 | -17/20 | -17/20 | passed |
| unicode_offset_conversion [prewarmed] | Ir | 105 | -2213/110689 | -2213/110689 | -2213/110689 | passed |
| unicode_offset_conversion [prewarmed] | I1mr | 105 | -4/3 | -4/3 | -4/3 | passed |
| unicode_offset_conversion [prewarmed] | ILmr | 105 | -4/3 | -4/3 | -4/3 | passed |
| unicode_position_conversion [prewarmed] | Ir | 105 | -6/306 | -6/306 | -6/306 | passed |
| unicode_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| unicode_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | Ir | 105 | -786399/5463194 | -792100/5463291 | -778796/5452984 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | I1mr | 105 | 5801/11945 | 5792/11953 | 5809/11947 | regression |
| workspace_burst_revision_invalidation [sixteen_revisions] | ILmr | 105 | -6/186 | -7/188 | -6/187 | passed |
| workspace_incremental_invalidation [hundred_files] | Ir | 105 | -34279/709588 | -46385/718811 | -7824/683154 | passed |
| workspace_incremental_invalidation [hundred_files] | I1mr | 105 | 413/866 | 420/885 | 417/867 | regression |
| workspace_incremental_invalidation [hundred_files] | ILmr | 105 | -2/152 | -2/150 | -1/150 | passed |
| workspace_initial_build [hundred_files] | Ir | 105 | -6875839/38973376 | -6876851/38971236 | -6872414/38973049 | passed |
| workspace_initial_build [hundred_files] | I1mr | 105 | 54612/101275 | 54556/101247 | 54715/101352 | regression |
| workspace_initial_build [hundred_files] | ILmr | 105 | 499/1126 | 498/1125 | 499/1126 | regression |
| workspace_orphan_graph_release [hundred_files] | Ir | 105 | -6550799/40068473 | -6551720/40026637 | -6543288/40068113 | passed |
| workspace_orphan_graph_release [hundred_files] | I1mr | 105 | 54881/101935 | 54841/101927 | 54943/101938 | regression |
| workspace_orphan_graph_release [hundred_files] | ILmr | 105 | 567/1220 | 566/1219 | 567/1219 | regression |
| workspace_references [hundred_files] | Ir | 105 | -342766/1920005 | -347294/1920136 | -322126/1919410 | passed |
| workspace_references [hundred_files] | I1mr | 105 | 51/252 | 50/250 | 52/252 | regression |
| workspace_references [hundred_files] | ILmr | 105 | 10/150 | 10/150 | 10/150 | regression |
| workspace_symbol_response_warm [ten_thousand_symbols] | Ir | 105 | -606928/30346433 | -606928/30346433 | -606928/30346433 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | I1mr | 105 | -3/131 | -3/131 | -3/131 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | ILmr | 105 | -3/130 | -3/130 | -3/130 | passed |
| workspace_symbols_warm [case_insensitive] | Ir | 105 | -328988/1696646 | -350937/1697088 | -326815/1697024 | passed |
| workspace_symbols_warm [case_insensitive] | I1mr | 105 | 14/182 | 13/182 | 15/182 | regression |
| workspace_symbols_warm [case_insensitive] | ILmr | 105 | 19/117 | 19/117 | 19/117 | regression |
| workspace_symbols_warm [empty] | Ir | 105 | -347199/1662367 | -347587/1661735 | -346615/1662175 | passed |
| workspace_symbols_warm [empty] | I1mr | 105 | 26/169 | 25/168 | 27/170 | regression |
| workspace_symbols_warm [empty] | ILmr | 105 | 23/107 | 23/107 | 23/107 | regression |

## New candidate workloads

These workloads have no baseline comparison and do not substitute for any baseline workload.
- cold_snapshot_build_metadata [default]
- declaration_documentation_warm [prewarmed]
- declaration_hover_warm [prewarmed]
- declaration_metadata_warm [prewarmed]
- semantic_completion_warm [unrelated_100]
- semantic_completion_warm [unrelated_1000]
- semantic_completion_warm [unrelated_10000]
- semantic_incoming_calls_warm [unrelated_100]
- semantic_incoming_calls_warm [unrelated_1000]
- semantic_incoming_calls_warm [unrelated_10000]
- semantic_outgoing_calls_warm [unrelated_100]
- semantic_outgoing_calls_warm [unrelated_1000]
- semantic_outgoing_calls_warm [unrelated_10000]
- semantic_references_warm [unrelated_100]
- semantic_references_warm [unrelated_1000]
- semantic_references_warm [unrelated_10000]
- semantic_rename_preflight_warm [declarations_100]
- semantic_rename_preflight_warm [declarations_1000]
- semantic_rename_preflight_warm [declarations_10000]
- semantic_resolve_warm [unrelated_100]
- semantic_resolve_warm [unrelated_1000]
- semantic_resolve_warm [unrelated_10000]
- semantic_shared_source_ownership [eight_views]
- semantic_shared_source_ownership [one_view]
- semantic_shared_source_ownership [two_views]
- semantic_sparse_relevant_views_warm [unrelated_files_100]
- semantic_sparse_relevant_views_warm [unrelated_files_1000]
- semantic_sparse_relevant_views_warm [unrelated_files_10000]
- semantic_sparse_view_reuse_warm [unrelated_files_100]
- semantic_sparse_view_reuse_warm [unrelated_files_1000]
- semantic_sparse_view_reuse_warm [unrelated_files_10000]
- semantic_view_capture [declarations_100]
- semantic_view_capture [declarations_1000]
- semantic_view_capture [declarations_10000]
- semantic_view_cold [declarations_100]
- semantic_view_cold [declarations_1000]
- semantic_view_cold [declarations_10000]
- semantic_view_reuse_warm [unrelated_100]
- semantic_view_reuse_warm [unrelated_1000]
- semantic_view_reuse_warm [unrelated_10000]
- semantic_views_cold [shared_dependency_2]
- semantic_views_cold [shared_dependency_64]
- semantic_views_cold [shared_dependency_8]
- semantic_views_preparsed_update [eight_views]
- semantic_views_preparsed_update [export_change]
- semantic_views_preparsed_update [one_view]
- semantic_views_preparsed_update [two_views]
- signature_help_warm [prewarmed]
