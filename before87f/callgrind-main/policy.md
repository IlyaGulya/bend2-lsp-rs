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
| call_hierarchy_warm [large] | I1mr | 105 | 14/17 | 14/17 | 14/17 | regression |
| call_hierarchy_warm [large] | ILmr | 105 | -8/13 | -8/13 | -8/13 | passed |
| call_hierarchy_warm [medium] | Ir | 105 | -86192/151686 | -86192/151686 | -86192/151686 | passed |
| call_hierarchy_warm [medium] | I1mr | 105 | 16/18 | 16/18 | 16/18 | regression |
| call_hierarchy_warm [medium] | ILmr | 105 | -8/13 | -8/13 | -8/13 | passed |
| call_hierarchy_warm [small] | Ir | 105 | -980/2003 | -980/2003 | -980/2003 | passed |
| call_hierarchy_warm [small] | I1mr | 105 | 2/20 | 2/20 | 2/20 | regression |
| call_hierarchy_warm [small] | ILmr | 105 | -14/19 | -14/19 | -14/19 | passed |
| cold_completion_context_build [default] | Ir | 105 | -12713/66649 | -12719/66662 | -12716/66738 | passed |
| cold_completion_context_build [default] | I1mr | 105 | 158/675 | 157/675 | 158/675 | regression |
| cold_completion_context_build [default] | ILmr | 105 | 153/648 | 152/648 | 153/648 | regression |
| cold_snapshot_build_large [default] | Ir | 105 | -12138658/28556068 | -12139537/28557427 | -12138902/28557164 | passed |
| cold_snapshot_build_large [default] | I1mr | 105 | 295/758 | 295/758 | 295/758 | regression |
| cold_snapshot_build_large [default] | ILmr | 105 | 112/673 | 112/673 | 112/673 | regression |
| cold_snapshot_build_medium [default] | Ir | 105 | -1789671/7128684 | -1789648/7128031 | -1789388/7128184 | passed |
| cold_snapshot_build_medium [default] | I1mr | 105 | 215/776 | 215/776 | 215/776 | regression |
| cold_snapshot_build_medium [default] | ILmr | 105 | 99/679 | 99/679 | 99/679 | regression |
| cold_snapshot_build_small [default] | Ir | 105 | -120773/665633 | -120803/665585 | -120659/665401 | passed |
| cold_snapshot_build_small [default] | I1mr | 105 | 352/886 | 352/886 | 352/886 | regression |
| cold_snapshot_build_small [default] | ILmr | 105 | 200/827 | 200/827 | 200/827 | regression |
| completion_warm [large] | Ir | 105 | -19413/360362 | -19413/360362 | -19413/360362 | passed |
| completion_warm [large] | I1mr | 105 | 6/50 | 6/50 | 6/50 | regression |
| completion_warm [large] | ILmr | 105 | -4/31 | -4/31 | -4/31 | passed |
| completion_warm [medium] | Ir | 105 | -374/110396 | -374/110396 | -374/110396 | passed |
| completion_warm [medium] | I1mr | 105 | 3/48 | 3/48 | 3/48 | regression |
| completion_warm [medium] | ILmr | 105 | -3/30 | -3/30 | -3/30 | passed |
| completion_warm [small] | Ir | 105 | 870/19757 | 870/19757 | 870/19757 | regression |
| completion_warm [small] | I1mr | 105 | -13/47 | -13/47 | -13/47 | passed |
| completion_warm [small] | ILmr | 105 | -14/39 | -14/39 | -14/39 | passed |
| constructor_completion_warm [empty] | Ir | 105 | -309/1240 | -309/1240 | -309/1240 | passed |
| constructor_completion_warm [empty] | I1mr | 105 | -2/36 | -2/36 | -2/36 | passed |
| constructor_completion_warm [empty] | ILmr | 105 | -5/32 | -5/32 | -5/32 | passed |
| constructor_completion_warm [prefix] | Ir | 105 | -302/1286 | -302/1286 | -302/1286 | passed |
| constructor_completion_warm [prefix] | I1mr | 105 | -1/36 | -1/36 | -1/36 | passed |
| constructor_completion_warm [prefix] | ILmr | 105 | -5/33 | -5/33 | -5/33 | passed |
| constructor_completion_warm [subsequence] | Ir | 105 | -193/1079 | -193/1079 | -193/1079 | passed |
| constructor_completion_warm [subsequence] | I1mr | 105 | 2/36 | 2/36 | 2/36 | regression |
| constructor_completion_warm [subsequence] | ILmr | 105 | -2/33 | -2/33 | -2/33 | passed |
| constructor_definition_warm [prewarmed] | Ir | 105 | -282/555 | -282/555 | -282/555 | passed |
| constructor_definition_warm [prewarmed] | I1mr | 105 | 5/9 | 5/9 | 5/9 | regression |
| constructor_definition_warm [prewarmed] | ILmr | 105 | -3/8 | -3/8 | -3/8 | passed |
| folding_10000_lines [ten_thousand_lines] | Ir | 105 | -661143/1454954 | -661143/1454954 | -661143/1454954 | passed |
| folding_10000_lines [ten_thousand_lines] | I1mr | 105 | 0/34 | 0/34 | 0/34 | passed |
| folding_10000_lines [ten_thousand_lines] | ILmr | 105 | -15/34 | -15/34 | -15/34 | passed |
| folding_1000_lines [thousand_lines] | Ir | 105 | -66290/146363 | -66290/146363 | -66290/146363 | passed |
| folding_1000_lines [thousand_lines] | I1mr | 105 | -6/36 | -6/36 | -6/36 | passed |
| folding_1000_lines [thousand_lines] | ILmr | 105 | -9/28 | -9/28 | -9/28 | passed |
| folding_100_lines [hundred_lines] | Ir | 105 | -5567/15372 | -5567/15372 | -5567/15372 | passed |
| folding_100_lines [hundred_lines] | I1mr | 105 | -9/36 | -9/36 | -9/36 | passed |
| folding_100_lines [hundred_lines] | ILmr | 105 | -8/28 | -8/28 | -8/28 | passed |
| identifier_ranges_warm [large] | Ir | 105 | -220/605 | -220/605 | -220/605 | passed |
| identifier_ranges_warm [large] | I1mr | 105 | 11/14 | 11/14 | 11/14 | regression |
| identifier_ranges_warm [large] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [medium] | Ir | 105 | -216/601 | -216/601 | -216/601 | passed |
| identifier_ranges_warm [medium] | I1mr | 105 | 14/11 | 14/11 | 14/11 | regression |
| identifier_ranges_warm [medium] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| identifier_ranges_warm [small] | Ir | 105 | -215/590 | -215/590 | -215/590 | passed |
| identifier_ranges_warm [small] | I1mr | 105 | 13/7 | 13/7 | 13/7 | regression |
| identifier_ranges_warm [small] | ILmr | 105 | -1/7 | -1/7 | -1/7 | passed |
| import_completion_candidates_warm [exact] | Ir | 105 | -8195/128064 | -8195/128060 | -8193/128058 | passed |
| import_completion_candidates_warm [exact] | I1mr | 105 | -21/131 | -21/131 | -21/131 | passed |
| import_completion_candidates_warm [exact] | ILmr | 105 | -6/93 | -6/93 | -6/93 | passed |
| import_completion_candidates_warm [fuzzy] | Ir | 105 | -7626/130290 | -7626/130290 | -7605/130290 | passed |
| import_completion_candidates_warm [fuzzy] | I1mr | 105 | -21/133 | -21/133 | -21/133 | passed |
| import_completion_candidates_warm [fuzzy] | ILmr | 105 | -6/95 | -6/95 | -6/95 | passed |
| import_completion_candidates_warm [no_match] | Ir | 105 | -18882/28619 | -18882/28619 | -18882/28619 | passed |
| import_completion_candidates_warm [no_match] | I1mr | 105 | -9/34 | -9/34 | -9/34 | passed |
| import_completion_candidates_warm [no_match] | ILmr | 105 | -15/34 | -15/34 | -15/34 | passed |
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
| references_warm [large] | I1mr | 105 | 23/13 | 23/13 | 23/13 | regression |
| references_warm [large] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [medium] | Ir | 105 | -9485/36499 | -9485/36499 | -9485/36499 | passed |
| references_warm [medium] | I1mr | 105 | 24/14 | 24/14 | 24/14 | regression |
| references_warm [medium] | ILmr | 105 | 0/9 | 0/9 | 0/9 | passed |
| references_warm [small] | Ir | 105 | -657/1797 | -657/1797 | -657/1797 | passed |
| references_warm [small] | I1mr | 105 | 10/16 | 10/16 | 10/16 | regression |
| references_warm [small] | ILmr | 105 | -6/15 | -6/15 | -6/15 | passed |
| scoped_completion_warm [large] | Ir | 105 | -57606/408394 | -57606/408394 | -57606/408394 | passed |
| scoped_completion_warm [large] | I1mr | 105 | 17/90 | 17/90 | 17/90 | regression |
| scoped_completion_warm [large] | ILmr | 105 | 4/68 | 4/68 | 4/68 | regression |
| scoped_completion_warm [medium] | Ir | 105 | -13962/131845 | -13962/131845 | -13962/131845 | passed |
| scoped_completion_warm [medium] | I1mr | 105 | 10/89 | 10/89 | 10/89 | regression |
| scoped_completion_warm [medium] | ILmr | 105 | 2/70 | 2/70 | 2/70 | regression |
| scoped_completion_warm [small] | Ir | 105 | -2311/32179 | -2311/32179 | -2311/32179 | passed |
| scoped_completion_warm [small] | I1mr | 105 | 7/87 | 7/87 | 7/87 | regression |
| scoped_completion_warm [small] | ILmr | 105 | -4/76 | -4/76 | -4/76 | passed |
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
| workspace_burst_revision_invalidation [sixteen_revisions] | Ir | 105 | -743767/5421191 | -779029/5458755 | -731592/5410182 | passed |
| workspace_burst_revision_invalidation [sixteen_revisions] | I1mr | 105 | 5291/11940 | 5279/11932 | 5300/11932 | regression |
| workspace_burst_revision_invalidation [sixteen_revisions] | ILmr | 105 | -10/187 | -10/186 | -9/185 | passed |
| workspace_incremental_invalidation [hundred_files] | Ir | 105 | -30966/642257 | -41117/718094 | -4133/683978 | passed |
| workspace_incremental_invalidation [hundred_files] | I1mr | 105 | 384/874 | 378/871 | 386/871 | regression |
| workspace_incremental_invalidation [hundred_files] | ILmr | 105 | -4/151 | -4/150 | -3/149 | passed |
| workspace_initial_build [hundred_files] | Ir | 105 | -6790513/38968716 | -6795030/38970118 | -6785935/38959136 | passed |
| workspace_initial_build [hundred_files] | I1mr | 105 | 44322/101270 | 44265/101278 | 44361/101251 | regression |
| workspace_initial_build [hundred_files] | ILmr | 105 | 476/1126 | 475/1125 | 476/1126 | regression |
| workspace_orphan_graph_release [hundred_files] | Ir | 105 | -6497106/40034496 | -6508463/40064689 | -6484145/40024273 | passed |
| workspace_orphan_graph_release [hundred_files] | I1mr | 105 | 44776/101933 | 44714/101930 | 44885/102001 | regression |
| workspace_orphan_graph_release [hundred_files] | ILmr | 105 | 547/1219 | 546/1219 | 547/1219 | regression |
| workspace_references [hundred_files] | Ir | 105 | -317067/1921303 | -318043/1920092 | -315276/1919597 | passed |
| workspace_references [hundred_files] | I1mr | 105 | 45/254 | 44/255 | 46/253 | regression |
| workspace_references [hundred_files] | ILmr | 105 | 10/151 | 9/150 | 10/150 | regression |
| workspace_symbol_response_warm [ten_thousand_symbols] | Ir | 105 | -606928/30346433 | -606928/30346433 | -606928/30346433 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | I1mr | 105 | -6/131 | -6/131 | -6/131 | passed |
| workspace_symbol_response_warm [ten_thousand_symbols] | ILmr | 105 | -5/130 | -5/130 | -5/130 | passed |
| workspace_symbols_warm [case_insensitive] | Ir | 105 | -324967/1698214 | -326536/1695038 | -324039/1698691 | passed |
| workspace_symbols_warm [case_insensitive] | I1mr | 105 | 9/182 | 9/182 | 10/182 | regression |
| workspace_symbols_warm [case_insensitive] | ILmr | 105 | 18/117 | 18/117 | 18/117 | regression |
| workspace_symbols_warm [empty] | Ir | 105 | -348254/1664658 | -348170/1661031 | -347490/1663902 | passed |
| workspace_symbols_warm [empty] | I1mr | 105 | 22/168 | 21/168 | 22/168 | regression |
| workspace_symbols_warm [empty] | ILmr | 105 | 23/107 | 22/107 | 23/107 | regression |

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
