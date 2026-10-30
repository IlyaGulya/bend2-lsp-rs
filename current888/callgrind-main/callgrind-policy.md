# Paired Callgrind admission

Method: `paired-median-105-v1`. Status: **regression**. Each comparison uses 105 fresh-process pairs on one host. Bounds are normalized allowance-adjusted residuals, not editor latency or synthetic count pairs.

| Workload | Event | Samples | Median residual | Lower bound | Upper bound | Classification |
| --- | --- | ---: | --- | --- | --- | --- |
| ascii_offset_conversion [prewarmed] | Ir | 105 | -1/61 | -1/61 | -1/61 | passed |
| ascii_offset_conversion [prewarmed] | I1mr | 105 | -4/3 | -4/3 | -4/3 | passed |
| ascii_offset_conversion [prewarmed] | ILmr | 105 | -4/3 | -4/3 | -4/3 | passed |
| ascii_position_conversion [prewarmed] | Ir | 105 | -2/141 | -2/141 | -2/141 | passed |
| ascii_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| ascii_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| call_hierarchy_warm [large] | Ir | 105 | -258941/452404 | -258941/452404 | -258941/452404 | passed |
| call_hierarchy_warm [large] | I1mr | 105 | 16/17 | 16/17 | 16/17 | regression |
| call_hierarchy_warm [large] | ILmr | 105 | -7/13 | -7/13 | -7/13 | passed |
| call_hierarchy_warm [medium] | Ir | 105 | -86192/151686 | -86192/151686 | -86192/151686 | passed |
| call_hierarchy_warm [medium] | I1mr | 105 | 18/18 | 18/18 | 18/18 | regression |
| call_hierarchy_warm [medium] | ILmr | 105 | -7/13 | -7/13 | -7/13 | passed |
| call_hierarchy_warm [small] | Ir | 105 | -980/2003 | -980/2003 | -980/2003 | passed |
| call_hierarchy_warm [small] | I1mr | 105 | 3/20 | 3/20 | 3/20 | regression |
| call_hierarchy_warm [small] | ILmr | 105 | -13/19 | -13/19 | -13/19 | passed |
| cold_completion_context_build [default] | Ir | 105 | -12717/66662 | -12727/66662 | -12717/66701 | passed |
| cold_completion_context_build [default] | I1mr | 105 | 150/675 | 150/675 | 150/675 | regression |
| cold_completion_context_build [default] | ILmr | 105 | 146/648 | 146/648 | 146/648 | regression |
| cold_snapshot_build_large [default] | Ir | 105 | -12139185/28557368 | -12139819/28557930 | -12138815/28556712 | passed |
| cold_snapshot_build_large [default] | I1mr | 105 | 289/758 | 289/758 | 289/758 | regression |
| cold_snapshot_build_large [default] | ILmr | 105 | 106/673 | 106/673 | 106/673 | regression |
| cold_snapshot_build_medium [default] | Ir | 105 | -1790040/7129499 | -1789856/7128326 | -1789650/7128175 | passed |
| cold_snapshot_build_medium [default] | I1mr | 105 | 212/776 | 212/776 | 212/776 | regression |
| cold_snapshot_build_medium [default] | ILmr | 105 | 93/679 | 93/679 | 93/679 | regression |
| cold_snapshot_build_small [default] | Ir | 105 | -120765/665471 | -120813/665497 | -120755/665621 | passed |
| cold_snapshot_build_small [default] | I1mr | 105 | 349/886 | 349/886 | 349/886 | regression |
| cold_snapshot_build_small [default] | ILmr | 105 | 199/827 | 199/827 | 199/827 | regression |
| completion_warm [large] | Ir | 105 | -10630/360362 | -10630/360362 | -10630/360362 | passed |
| completion_warm [large] | I1mr | 105 | 7/50 | 7/50 | 7/50 | regression |
| completion_warm [large] | ILmr | 105 | -2/31 | -2/31 | -2/31 | passed |
| completion_warm [medium] | Ir | 105 | 1833/110396 | 1833/110396 | 1833/110396 | regression |
| completion_warm [medium] | I1mr | 105 | 7/48 | 7/48 | 7/48 | regression |
| completion_warm [medium] | ILmr | 105 | -1/30 | -1/30 | -1/30 | passed |
| completion_warm [small] | Ir | 105 | -288/19757 | -288/19757 | -288/19757 | passed |
| completion_warm [small] | I1mr | 105 | -10/47 | -10/47 | -10/47 | passed |
| completion_warm [small] | ILmr | 105 | -10/39 | -10/39 | -10/39 | passed |
| constructor_completion_warm [empty] | Ir | 105 | -309/1240 | -309/1240 | -309/1240 | passed |
| constructor_completion_warm [empty] | I1mr | 105 | -4/36 | -4/36 | -4/36 | passed |
| constructor_completion_warm [empty] | ILmr | 105 | -6/32 | -6/32 | -6/32 | passed |
| constructor_completion_warm [prefix] | Ir | 105 | -302/1286 | -302/1286 | -302/1286 | passed |
| constructor_completion_warm [prefix] | I1mr | 105 | -3/36 | -3/36 | -3/36 | passed |
| constructor_completion_warm [prefix] | ILmr | 105 | -6/33 | -6/33 | -6/33 | passed |
| constructor_completion_warm [subsequence] | Ir | 105 | -193/1079 | -193/1079 | -193/1079 | passed |
| constructor_completion_warm [subsequence] | I1mr | 105 | -2/36 | -2/36 | -2/36 | passed |
| constructor_completion_warm [subsequence] | ILmr | 105 | -5/33 | -5/33 | -5/33 | passed |
| constructor_definition_warm [prewarmed] | Ir | 105 | -282/555 | -282/555 | -282/555 | passed |
| constructor_definition_warm [prewarmed] | I1mr | 105 | 5/9 | 5/9 | 5/9 | regression |
| constructor_definition_warm [prewarmed] | ILmr | 105 | -2/8 | -2/8 | -2/8 | passed |
| folding_10000_lines [ten_thousand_lines] | Ir | 105 | -661143/1454954 | -661143/1454954 | -661143/1454954 | passed |
| folding_10000_lines [ten_thousand_lines] | I1mr | 105 | -4/34 | -4/34 | -4/34 | passed |
| folding_10000_lines [ten_thousand_lines] | ILmr | 105 | -17/34 | -17/34 | -17/34 | passed |
| folding_1000_lines [thousand_lines] | Ir | 105 | -66290/146363 | -66290/146363 | -66290/146363 | passed |
| folding_1000_lines [thousand_lines] | I1mr | 105 | -9/36 | -9/36 | -9/36 | passed |
| folding_1000_lines [thousand_lines] | ILmr | 105 | -11/28 | -11/28 | -11/28 | passed |
| folding_100_lines [hundred_lines] | Ir | 105 | -5567/15372 | -5567/15372 | -5567/15372 | passed |
| folding_100_lines [hundred_lines] | I1mr | 105 | -12/36 | -12/36 | -12/36 | passed |
| folding_100_lines [hundred_lines] | ILmr | 105 | -10/28 | -10/28 | -10/28 | passed |
| identifier_ranges_warm [large] | Ir | 105 | -220/605 | -220/605 | -220/605 | passed |
| identifier_ranges_warm [large] | I1mr | 105 | 12/14 | 12/14 | 12/14 | regression |
| identifier_ranges_warm [large] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [medium] | Ir | 105 | -216/601 | -216/601 | -216/601 | passed |
| identifier_ranges_warm [medium] | I1mr | 105 | 15/11 | 15/11 | 15/11 | regression |
| identifier_ranges_warm [medium] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [small] | Ir | 105 | -215/590 | -215/590 | -215/590 | passed |
| identifier_ranges_warm [small] | I1mr | 105 | 13/7 | 13/7 | 13/7 | regression |
| identifier_ranges_warm [small] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| import_completion_candidates_warm [exact] | Ir | 105 | -4822/128056 | -4824/128058 | -4822/128056 | passed |
| import_completion_candidates_warm [exact] | I1mr | 105 | -22/131 | -22/131 | -22/131 | passed |
| import_completion_candidates_warm [exact] | ILmr | 105 | -9/93 | -9/93 | -9/93 | passed |
| import_completion_candidates_warm [fuzzy] | Ir | 105 | -4257/130290 | -4257/130290 | -4229/130290 | passed |
| import_completion_candidates_warm [fuzzy] | I1mr | 105 | -21/133 | -21/133 | -21/133 | passed |
| import_completion_candidates_warm [fuzzy] | ILmr | 105 | -8/95 | -8/95 | -8/95 | passed |
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
| references_warm [large] | I1mr | 105 | 24/13 | 24/13 | 24/13 | regression |
| references_warm [large] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [medium] | Ir | 105 | -9485/36499 | -9485/36499 | -9485/36499 | passed |
| references_warm [medium] | I1mr | 105 | 25/14 | 25/14 | 25/14 | regression |
| references_warm [medium] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [small] | Ir | 105 | -657/1797 | -657/1797 | -657/1797 | passed |
| references_warm [small] | I1mr | 105 | 10/16 | 10/16 | 10/16 | regression |
| references_warm [small] | ILmr | 105 | -6/15 | -6/15 | -6/15 | passed |
| scoped_completion_warm [large] | Ir | 105 | -57606/408394 | -57606/408394 | -57606/408394 | passed |
| scoped_completion_warm [large] | I1mr | 105 | 11/90 | 11/90 | 11/90 | regression |
| scoped_completion_warm [large] | ILmr | 105 | 0/68 | 0/68 | 0/68 | passed |
| scoped_completion_warm [medium] | Ir | 105 | -13962/131845 | -13962/131845 | -13962/131845 | passed |
| scoped_completion_warm [medium] | I1mr | 105 | 5/89 | 5/89 | 5/89 | regression |
| scoped_completion_warm [medium] | ILmr | 105 | -2/70 | -2/70 | -2/70 | passed |
| scoped_completion_warm [small] | Ir | 105 | -2311/32179 | -2311/32179 | -2311/32179 | passed |
| scoped_completion_warm [small] | I1mr | 105 | 0/87 | 0/87 | 0/87 | passed |
| scoped_completion_warm [small] | ILmr | 105 | -8/76 | -8/76 | -8/76 | passed |
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
| unicode_offset_conversion [prewarmed] | I1mr | 105 | -3/3 | -3/3 | -3/3 | passed |
| unicode_offset_conversion [prewarmed] | ILmr | 105 | -3/3 | -3/3 | -3/3 | passed |
| unicode_position_conversion [prewarmed] | Ir | 105 | -6/306 | -6/306 | -6/306 | passed |
| unicode_position_conversion [prewarmed] | I1mr | 105 | -3/2 | -3/2 | -3/2 | passed |
| unicode_position_conversion [prewarmed] | ILmr | 105 | -3/2 | -3/2 | -3/2 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | Ir | 105 | -764518/5446876 | -777134/5461273 | -736145/5413544 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | I1mr | 105 | 5204/11964 | 5184/11941 | 5205/11946 | regression |
| workspace_burst_revision_invalidation [sixteen_revisions] | ILmr | 105 | -6/188 | -6/187 | -5/185 | passed |
| workspace_incremental_invalidation [hundred_files] | Ir | 105 | -6802/680935 | -35538/716226 | -2110/676611 | passed |
| workspace_incremental_invalidation [hundred_files] | I1mr | 105 | 373/867 | 369/865 | 372/860 | regression |
| workspace_incremental_invalidation [hundred_files] | ILmr | 105 | -1/149 | -2/150 | -1/151 | passed |
| workspace_initial_build [hundred_files] | Ir | 105 | -6792430/38963437 | -6796249/38970894 | -6790023/38968647 | passed |
| workspace_initial_build [hundred_files] | I1mr | 105 | 48716/101282 | 48684/101305 | 48795/101385 | regression |
| workspace_initial_build [hundred_files] | ILmr | 105 | 478/1125 | 478/1125 | 479/1126 | regression |
| workspace_orphan_graph_release [hundred_files] | Ir | 105 | -6513376/40065796 | -6518473/40065645 | -6505649/40067652 | passed |
| workspace_orphan_graph_release [hundred_files] | I1mr | 105 | 49124/101956 | 49127/102050 | 49155/101961 | regression |
| workspace_orphan_graph_release [hundred_files] | ILmr | 105 | 553/1219 | 553/1219 | 554/1220 | regression |
| workspace_references [hundred_files] | Ir | 105 | -315411/1920609 | -316878/1919668 | -314052/1919748 | passed |
| workspace_references [hundred_files] | I1mr | 105 | 48/250 | 46/250 | 49/251 | regression |
| workspace_references [hundred_files] | ILmr | 105 | 12/150 | 11/150 | 12/150 | regression |
| workspace_symbol_response_warm [ten_thousand_symbols] | Ir | 105 | -606928/30346433 | -606928/30346433 | -606928/30346433 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | I1mr | 105 | -2/131 | -2/131 | -2/131 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | ILmr | 105 | -1/130 | -1/130 | -1/130 | passed |
| workspace_symbols_warm [case_insensitive] | Ir | 105 | -324039/1695860 | -325228/1696720 | -322983/1697790 | passed |
| workspace_symbols_warm [case_insensitive] | I1mr | 105 | 6/182 | 5/182 | 7/181 | regression |
| workspace_symbols_warm [case_insensitive] | ILmr | 105 | 18/117 | 17/117 | 18/117 | regression |
| workspace_symbols_warm [empty] | Ir | 105 | -346608/1660403 | -347713/1662497 | -346411/1661030 | passed |
| workspace_symbols_warm [empty] | I1mr | 105 | 21/169 | 20/168 | 21/168 | regression |
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
