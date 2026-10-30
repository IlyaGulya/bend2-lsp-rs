# Paired Callgrind admission

Method: `paired-median-105-v1`. Status: **inconclusive**. Each comparison uses 105 fresh-process pairs on one host. Bounds are normalized allowance-adjusted residuals, not editor latency or synthetic count pairs.

| Workload | Event | Samples | Median residual | Lower bound | Upper bound | Classification |
| --- | --- | ---: | --- | --- | --- | --- |
| ascii_offset_conversion [prewarmed] | Ir | 105 | -1/61 | -1/61 | -1/61 | passed |
| ascii_offset_conversion [prewarmed] | I1mr | 105 | -4/3 | -4/3 | -4/3 | passed |
| ascii_offset_conversion [prewarmed] | ILmr | 105 | -4/3 | -4/3 | -4/3 | passed |
| ascii_position_conversion [prewarmed] | Ir | 105 | -2/141 | -2/141 | -2/141 | passed |
| ascii_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| ascii_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| call_hierarchy_warm [large] | Ir | 105 | -258946/452404 | -258946/452404 | -258946/452404 | passed |
| call_hierarchy_warm [large] | I1mr | 105 | 18/17 | 18/17 | 18/17 | regression |
| call_hierarchy_warm [large] | ILmr | 105 | -7/13 | -7/13 | -7/13 | passed |
| call_hierarchy_warm [medium] | Ir | 105 | -86188/151686 | -86188/151686 | -86188/151686 | passed |
| call_hierarchy_warm [medium] | I1mr | 105 | 22/18 | 22/18 | 22/18 | regression |
| call_hierarchy_warm [medium] | ILmr | 105 | -7/13 | -7/13 | -7/13 | passed |
| call_hierarchy_warm [small] | Ir | 105 | -980/2003 | -980/2003 | -980/2003 | passed |
| call_hierarchy_warm [small] | I1mr | 105 | 8/20 | 8/20 | 8/20 | regression |
| call_hierarchy_warm [small] | ILmr | 105 | -13/19 | -13/19 | -13/19 | passed |
| cold_completion_context_build [default] | Ir | 105 | -12355/66649 | -12365/66654 | -12354/66688 | passed |
| cold_completion_context_build [default] | I1mr | 105 | 164/675 | 164/675 | 164/675 | regression |
| cold_completion_context_build [default] | ILmr | 105 | 152/648 | 152/648 | 152/648 | regression |
| cold_snapshot_build_large [default] | Ir | 105 | -12110887/28556480 | -12111790/28557685 | -12110516/28556169 | passed |
| cold_snapshot_build_large [default] | I1mr | 105 | 290/758 | 290/758 | 290/758 | regression |
| cold_snapshot_build_large [default] | ILmr | 105 | 105/673 | 105/673 | 105/673 | regression |
| cold_snapshot_build_medium [default] | Ir | 105 | -1780036/7128262 | -1780382/7128867 | -1779973/7128312 | passed |
| cold_snapshot_build_medium [default] | I1mr | 105 | 188/776 | 188/776 | 188/776 | regression |
| cold_snapshot_build_medium [default] | ILmr | 105 | 92/679 | 92/679 | 92/679 | regression |
| cold_snapshot_build_small [default] | Ir | 105 | -119379/665361 | -119465/665552 | -119395/665570 | passed |
| cold_snapshot_build_small [default] | I1mr | 105 | 309/886 | 309/886 | 309/886 | regression |
| cold_snapshot_build_small [default] | ILmr | 105 | 195/827 | 195/827 | 195/827 | regression |
| completion_warm [large] | Ir | 105 | -5344/360362 | -5344/360362 | -5344/360362 | passed |
| completion_warm [large] | I1mr | 105 | 5/50 | 5/50 | 5/50 | regression |
| completion_warm [large] | ILmr | 105 | -5/31 | -5/31 | -5/31 | passed |
| completion_warm [medium] | Ir | 105 | -446/110396 | -446/110396 | -446/110396 | passed |
| completion_warm [medium] | I1mr | 105 | 5/48 | 5/48 | 5/48 | regression |
| completion_warm [medium] | ILmr | 105 | -2/30 | -2/30 | -2/30 | passed |
| completion_warm [small] | Ir | 105 | 146/19757 | 146/19757 | 146/19757 | regression |
| completion_warm [small] | I1mr | 105 | -13/47 | -13/47 | -13/47 | passed |
| completion_warm [small] | ILmr | 105 | -14/39 | -14/39 | -14/39 | passed |
| constructor_completion_warm [empty] | Ir | 105 | -309/1240 | -309/1240 | -309/1240 | passed |
| constructor_completion_warm [empty] | I1mr | 105 | -1/36 | -1/36 | -1/36 | passed |
| constructor_completion_warm [empty] | ILmr | 105 | -6/32 | -6/32 | -6/32 | passed |
| constructor_completion_warm [prefix] | Ir | 105 | -318/1286 | -318/1286 | -318/1286 | passed |
| constructor_completion_warm [prefix] | I1mr | 105 | -1/36 | -1/36 | -1/36 | passed |
| constructor_completion_warm [prefix] | ILmr | 105 | -7/33 | -7/33 | -7/33 | passed |
| constructor_completion_warm [subsequence] | Ir | 105 | -193/1079 | -193/1079 | -193/1079 | passed |
| constructor_completion_warm [subsequence] | I1mr | 105 | 1/36 | 1/36 | 1/36 | regression |
| constructor_completion_warm [subsequence] | ILmr | 105 | -5/33 | -5/33 | -5/33 | passed |
| constructor_definition_warm [prewarmed] | Ir | 105 | -274/555 | -274/555 | -274/555 | passed |
| constructor_definition_warm [prewarmed] | I1mr | 105 | 2/9 | 2/9 | 2/9 | regression |
| constructor_definition_warm [prewarmed] | ILmr | 105 | 0/8 | 0/8 | 0/8 | passed |
| folding_10000_lines [ten_thousand_lines] | Ir | 105 | -661143/1454954 | -661143/1454954 | -661143/1454954 | passed |
| folding_10000_lines [ten_thousand_lines] | I1mr | 105 | -6/34 | -6/34 | -6/34 | passed |
| folding_10000_lines [ten_thousand_lines] | ILmr | 105 | -16/34 | -16/34 | -16/34 | passed |
| folding_1000_lines [thousand_lines] | Ir | 105 | -66290/146363 | -66290/146363 | -66290/146363 | passed |
| folding_1000_lines [thousand_lines] | I1mr | 105 | -9/36 | -9/36 | -9/36 | passed |
| folding_1000_lines [thousand_lines] | ILmr | 105 | -10/28 | -10/28 | -10/28 | passed |
| folding_100_lines [hundred_lines] | Ir | 105 | -5567/15372 | -5567/15372 | -5567/15372 | passed |
| folding_100_lines [hundred_lines] | I1mr | 105 | -9/36 | -9/36 | -9/36 | passed |
| folding_100_lines [hundred_lines] | ILmr | 105 | -9/28 | -9/28 | -9/28 | passed |
| identifier_ranges_warm [large] | Ir | 105 | -220/605 | -220/605 | -220/605 | passed |
| identifier_ranges_warm [large] | I1mr | 105 | 12/14 | 12/14 | 12/14 | regression |
| identifier_ranges_warm [large] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [medium] | Ir | 105 | -216/601 | -216/601 | -216/601 | passed |
| identifier_ranges_warm [medium] | I1mr | 105 | 15/11 | 15/11 | 15/11 | regression |
| identifier_ranges_warm [medium] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [small] | Ir | 105 | -215/590 | -215/590 | -215/590 | passed |
| identifier_ranges_warm [small] | I1mr | 105 | 18/7 | 18/7 | 18/7 | regression |
| identifier_ranges_warm [small] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| import_completion_candidates_warm [exact] | Ir | 105 | -8298/128056 | -8300/128056 | -8295/128056 | passed |
| import_completion_candidates_warm [exact] | I1mr | 105 | -17/131 | -17/131 | -17/131 | passed |
| import_completion_candidates_warm [exact] | ILmr | 105 | -5/93 | -5/93 | -5/93 | passed |
| import_completion_candidates_warm [fuzzy] | Ir | 105 | -7478/130234 | -7500/130290 | -7472/130290 | passed |
| import_completion_candidates_warm [fuzzy] | I1mr | 105 | -18/133 | -18/133 | -18/133 | passed |
| import_completion_candidates_warm [fuzzy] | ILmr | 105 | -6/95 | -6/95 | -6/95 | passed |
| import_completion_candidates_warm [no_match] | Ir | 105 | -18882/28619 | -18882/28619 | -18882/28619 | passed |
| import_completion_candidates_warm [no_match] | I1mr | 105 | -10/34 | -10/34 | -10/34 | passed |
| import_completion_candidates_warm [no_match] | ILmr | 105 | -16/34 | -16/34 | -16/34 | passed |
| inlay_hints_warm [large] | Ir | 105 | -1099/58394 | -1099/58394 | -1099/58394 | passed |
| inlay_hints_warm [large] | I1mr | 105 | -3/7 | -3/7 | -3/7 | passed |
| inlay_hints_warm [large] | ILmr | 105 | -4/7 | -4/7 | -4/7 | passed |
| inlay_hints_warm [medium] | Ir | 105 | -296/20186 | -296/20186 | -296/20186 | passed |
| inlay_hints_warm [medium] | I1mr | 105 | -4/9 | -4/9 | -4/9 | passed |
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
| pattern_completion_context_warm [nested] | I1mr | 105 | -2/4 | -2/4 | -2/4 | passed |
| pattern_completion_context_warm [nested] | ILmr | 105 | -2/4 | -2/4 | -2/4 | passed |
| reference_location_order_warm [large] | Ir | 105 | -284336/14216843 | -284336/14216843 | -284336/14216843 | passed |
| reference_location_order_warm [large] | I1mr | 105 | -1/133 | -1/133 | -1/133 | passed |
| reference_location_order_warm [large] | ILmr | 105 | -3/133 | -3/133 | -3/133 | passed |
| reference_location_order_warm [medium] | Ir | 105 | -17470/873518 | -17470/873518 | -17470/873518 | passed |
| reference_location_order_warm [medium] | I1mr | 105 | 0/138 | 0/138 | 0/138 | passed |
| reference_location_order_warm [medium] | ILmr | 105 | -4/138 | -4/138 | -4/138 | passed |
| reference_location_order_warm [small] | Ir | 105 | -681/34077 | -681/34077 | -681/34077 | passed |
| reference_location_order_warm [small] | I1mr | 105 | -2/105 | -2/105 | -2/105 | passed |
| reference_location_order_warm [small] | ILmr | 105 | -3/105 | -3/105 | -3/105 | passed |
| references_warm [large] | Ir | 105 | -28831/106817 | -28831/106817 | -28831/106817 | passed |
| references_warm [large] | I1mr | 105 | 25/13 | 25/13 | 25/13 | regression |
| references_warm [large] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [medium] | Ir | 105 | -9481/36499 | -9481/36499 | -9481/36499 | passed |
| references_warm [medium] | I1mr | 105 | 29/14 | 29/14 | 29/14 | regression |
| references_warm [medium] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [small] | Ir | 105 | -657/1797 | -657/1797 | -657/1797 | passed |
| references_warm [small] | I1mr | 105 | 15/16 | 15/16 | 15/16 | regression |
| references_warm [small] | ILmr | 105 | -6/15 | -6/15 | -6/15 | passed |
| scoped_completion_warm [large] | Ir | 105 | -43049/408394 | -43049/408394 | -43049/408394 | passed |
| scoped_completion_warm [large] | I1mr | 105 | 6/90 | 6/90 | 6/90 | regression |
| scoped_completion_warm [large] | ILmr | 105 | -5/68 | -5/68 | -5/68 | passed |
| scoped_completion_warm [medium] | Ir | 105 | -13728/131845 | -13728/131845 | -13728/131845 | passed |
| scoped_completion_warm [medium] | I1mr | 105 | 4/89 | 4/89 | 4/89 | regression |
| scoped_completion_warm [medium] | ILmr | 105 | -5/70 | -5/70 | -5/70 | passed |
| scoped_completion_warm [small] | Ir | 105 | -2280/32179 | -2280/32179 | -2280/32179 | passed |
| scoped_completion_warm [small] | I1mr | 105 | -4/87 | -4/87 | -4/87 | passed |
| scoped_completion_warm [small] | ILmr | 105 | -13/76 | -13/76 | -13/76 | passed |
| semantic_tokens_warm [large] | Ir | 105 | -248115/456332 | -248115/456332 | -248115/456332 | passed |
| semantic_tokens_warm [large] | I1mr | 105 | -11/14 | -11/14 | -11/14 | passed |
| semantic_tokens_warm [large] | ILmr | 105 | -12/14 | -12/14 | -12/14 | passed |
| semantic_tokens_warm [medium] | Ir | 105 | -82687/152835 | -82687/152835 | -82687/152835 | passed |
| semantic_tokens_warm [medium] | I1mr | 105 | -12/16 | -12/16 | -12/16 | passed |
| semantic_tokens_warm [medium] | ILmr | 105 | -12/14 | -12/14 | -12/14 | passed |
| semantic_tokens_warm [small] | Ir | 105 | -9179/16083 | -9179/16083 | -9179/16083 | passed |
| semantic_tokens_warm [small] | I1mr | 105 | -19/21 | -19/21 | -19/21 | passed |
| semantic_tokens_warm [small] | ILmr | 105 | -18/20 | -18/20 | -18/20 | passed |
| unicode_offset_conversion [prewarmed] | Ir | 105 | -2213/110689 | -2213/110689 | -2213/110689 | passed |
| unicode_offset_conversion [prewarmed] | I1mr | 105 | -3/3 | -3/3 | -3/3 | passed |
| unicode_offset_conversion [prewarmed] | ILmr | 105 | -3/3 | -3/3 | -3/3 | passed |
| unicode_position_conversion [prewarmed] | Ir | 105 | -6/306 | -6/306 | -6/306 | passed |
| unicode_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| unicode_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | Ir | 105 | -816490/5431190 | -827332/5461328 | -792635/5401260 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | I1mr | 105 | 5091/11945 | 5090/11961 | 5097/11949 | regression |
| workspace_burst_revision_invalidation [sixteen_revisions] | ILmr | 105 | -4/186 | -5/188 | -4/187 | passed |
| workspace_incremental_invalidation [hundred_files] | Ir | 105 | -38769/719471 | -54320/679565 | -25875/651111 | passed |
| workspace_incremental_invalidation [hundred_files] | I1mr | 105 | 387/866 | 385/866 | 388/864 | regression |
| workspace_incremental_invalidation [hundred_files] | ILmr | 105 | 0/150 | 0/151 | 1/150 | inconclusive |
| workspace_initial_build [hundred_files] | Ir | 105 | -6740047/38969848 | -6742519/38967375 | -6734304/38966377 | passed |
| workspace_initial_build [hundred_files] | I1mr | 105 | 46676/101252 | 46642/101281 | 46740/101270 | regression |
| workspace_initial_build [hundred_files] | ILmr | 105 | 484/1125 | 484/1126 | 485/1126 | regression |
| workspace_orphan_graph_release [hundred_files] | Ir | 105 | -6405486/40060218 | -6413983/40069596 | -6402956/40067482 | passed |
| workspace_orphan_graph_release [hundred_files] | I1mr | 105 | 47027/101983 | 46975/102005 | 47103/101932 | regression |
| workspace_orphan_graph_release [hundred_files] | ILmr | 105 | 554/1220 | 553/1219 | 554/1219 | regression |
| workspace_references [hundred_files] | Ir | 105 | -316912/1920967 | -377767/1981436 | -314070/1920875 | passed |
| workspace_references [hundred_files] | I1mr | 105 | 58/252 | 57/250 | 59/250 | regression |
| workspace_references [hundred_files] | ILmr | 105 | 9/150 | 9/150 | 9/150 | regression |
| workspace_symbol_response_warm [ten_thousand_symbols] | Ir | 105 | -606928/30346433 | -606928/30346433 | -606928/30346433 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | I1mr | 105 | -5/131 | -5/131 | -5/131 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | ILmr | 105 | -4/130 | -4/130 | -4/130 | passed |
| workspace_symbols_warm [case_insensitive] | Ir | 105 | -481716/1696542 | -483022/1696724 | -373009/1698741 | passed |
| workspace_symbols_warm [case_insensitive] | I1mr | 105 | 15/182 | 15/182 | 16/181 | regression |
| workspace_symbols_warm [case_insensitive] | ILmr | 105 | 19/117 | 19/117 | 19/117 | regression |
| workspace_symbols_warm [empty] | Ir | 105 | -297664/1663292 | -298900/1662644 | -295672/1658691 | passed |
| workspace_symbols_warm [empty] | I1mr | 105 | 39/169 | 38/169 | 39/168 | regression |
| workspace_symbols_warm [empty] | ILmr | 105 | 24/107 | 24/107 | 24/107 | regression |

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
- semantic_views_preparsed_update [eight_views]
- semantic_views_preparsed_update [export_change]
- semantic_views_preparsed_update [one_view]
- semantic_views_preparsed_update [two_views]
- signature_help_warm [prewarmed]
