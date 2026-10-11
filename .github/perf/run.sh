#!/usr/bin/env bash
# Workflow-only orchestration. Never invoke collectors outside a hosted job.
set -euo pipefail
[[ "${GITHUB_ACTIONS:-}" == true ]] || { echo 'Hosted measurements only' >&2; exit 1; }
mkdir -p "$PERF_ROOT/logs"

run() {
  { printf '%q ' "$@"; printf '\n'; } >> "$PERF_ROOT/commands.txt"
  "$@"
}

symbols() {
  local source="$1" destination="$2" symbols
  shopt -s nullglob
  for symbols in "$source/"*.pdb "$source/"*.dSYM; do
    cp -RL "$symbols" "$destination/"
  done
}

profile() {
  local scenario="$1" backend="$2" binary="$3" code=0
  local name="profile-$scenario-$backend" directory="$PERF_ROOT/profiles/$scenario/$backend"
  local flags=("--$backend")
  if [[ "$backend" == native ]]; then
    native_kind=cpu
    if [[ "$MODE" == native ]]; then native_kind="$NATIVE_KIND"; fi
    flags=(--native --native-kind "$native_kind")
  fi
  if [[ "$backend" == native-heap ]]; then flags=(--native --native-kind heap); fi
  run "$PERF_TOOL" collect-profile "$scenario" "${flags[@]}" --binary "$binary" \
    --output-dir "$directory" --candidate-revision "$CANDIDATE_SHA" \
    > "$PERF_ROOT/logs/$name.log" 2>&1 || code=$?
  cat "$PERF_ROOT/logs/$name.log"
  "$PERF_TOOL" workflow status "$name" "$code"
  if [[ "$code" != 0 ]]; then failed=1; fi
}

case "${1:?Expected workflow stage}" in
  prepare)
    "$PERF_TOOL" workflow prepare
    test "$(git rev-parse HEAD)" = "$WORKFLOW_SHA"
    test "$ACTUAL_ARCH" = "$EXPECTED_ARCH"
    run git worktree add --detach "$RUNNER_TEMP/perf-base" "$BASE_SHA"
    run git worktree add --detach "$RUNNER_TEMP/perf-candidate" "$CANDIDATE_SHA"
    test "$(git -C "$RUNNER_TEMP/perf-base" rev-parse HEAD)" = "$BASE_SHA"
    test "$(git -C "$RUNNER_TEMP/perf-candidate" rev-parse HEAD)" = "$CANDIDATE_SHA"
    ;;
  tools)
    exe_suffix=''
    if [[ "$RUNNER_OS" == Windows ]]; then exe_suffix='.exe'; fi
    {
      printf 'EXE_SUFFIX=%s\n' "$exe_suffix"
      printf 'PERF_TOOL=%s/perf-tools/release/bend2-perf%s\n' "$RUNNER_TEMP" "$exe_suffix"
      printf 'TIMING_BIN_DIR=%s/binaries/timing\n' "$PERF_ROOT"
      printf 'PROFILE_BIN_DIR=%s/binaries/profile\n' "$PERF_ROOT"
      printf 'MEMORY_BIN_DIR=%s/binaries/heap\n' "$PERF_ROOT"
    } >> "$GITHUB_ENV"
    run rustup toolchain install "1.98.1-$PERF_TARGET" --profile minimal
    run rustup override set "1.98.1-$PERF_TARGET"
    run rustc -vV
    run cargo build --locked --release -p bend2-perf --target-dir "$RUNNER_TEMP/perf-tools" \
      2>&1 | tee "$PERF_ROOT/logs/tool-build.log"
    ;;
  timing)
    mkdir -p "$TIMING_BIN_DIR"
    run cargo build --locked --release --bin bend2-lsp \
      --manifest-path "$RUNNER_TEMP/perf-base/Cargo.toml" --target-dir "$RUNNER_TEMP/timing-base"
    cp "$RUNNER_TEMP/timing-base/release/bend2-lsp$EXE_SUFFIX" "$TIMING_BIN_DIR/main$EXE_SUFFIX"
    run cargo build --locked --release --bin bend2-lsp \
      --manifest-path "$RUNNER_TEMP/perf-candidate/Cargo.toml" --target-dir "$RUNNER_TEMP/timing-candidate"
    cp "$RUNNER_TEMP/timing-candidate/release/bend2-lsp$EXE_SUFFIX" "$TIMING_BIN_DIR/candidate$EXE_SUFFIX"
    pushd "$RUNNER_TEMP/perf-candidate"
    run "$PERF_TOOL" native memory provenance --baseline-revision "$BASE_SHA" \
      --candidate-revision "$CANDIDATE_SHA" --commands-file "$PERF_ROOT/commands.txt" \
      --binary-dir "$TIMING_BIN_DIR" --output "$PERF_ROOT/provenance.json"
    popd
    ;;
  latency)
    run "$PERF_TOOL" native latency --baseline-binary "$TIMING_BIN_DIR/main$EXE_SUFFIX" \
      --candidate-binary "$TIMING_BIN_DIR/candidate$EXE_SUFFIX" \
      --baseline-output "$PERF_ROOT/baseline.json" --candidate-output "$PERF_ROOT/candidate.json" \
      --rounds 7 --samples 32 --warmup 8
    run "$PERF_TOOL" reports latency "$PERF_ROOT/baseline.json" "$PERF_ROOT/candidate.json" \
      --json-output "$PERF_ROOT/latency-report.json" --markdown-output "$PERF_ROOT/latency-report.md"
    ;;
  discovery)
    run "$PERF_TOOL" native discovery --baseline-binary "$TIMING_BIN_DIR/main$EXE_SUFFIX" \
      --candidate-binary "$TIMING_BIN_DIR/candidate$EXE_SUFFIX" \
      --baseline-revision "$BASE_SHA" --candidate-revision "$CANDIDATE_SHA" \
      --output-dir "$PERF_ROOT/discovery" --rounds 7 --samples 32 --warmup 8
    ;;
  symbols)
    mkdir -p "$PROFILE_BIN_DIR"
    if [[ "$RUNNER_OS" == macOS ]]; then export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed; fi
    run env CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
      cargo build --locked --release --bin bend2-lsp \
      --manifest-path "$RUNNER_TEMP/perf-candidate/Cargo.toml" --target-dir "$RUNNER_TEMP/profile-candidate"
    cp "$RUNNER_TEMP/profile-candidate/release/bend2-lsp$EXE_SUFFIX" "$PROFILE_BIN_DIR/"
    symbols "$RUNNER_TEMP/profile-candidate/release" "$PROFILE_BIN_DIR"
    ;;
  heap)
    mkdir -p "$MEMORY_BIN_DIR"
    if [[ "$RUNNER_OS" == macOS ]]; then export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed; fi
    build_targets=(--bin bend2-lsp)
    if [[ "$MODE" == full ]]; then build_targets+=(--example line_index_profile --example folding_allocations); fi
    run env CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
      cargo build --locked --release --features dhat-heap "${build_targets[@]}" \
      --manifest-path "$RUNNER_TEMP/perf-candidate/Cargo.toml" --target-dir "$RUNNER_TEMP/heap-candidate"
    cp "$RUNNER_TEMP/heap-candidate/release/bend2-lsp$EXE_SUFFIX" "$MEMORY_BIN_DIR/"
    symbols "$RUNNER_TEMP/heap-candidate/release" "$MEMORY_BIN_DIR"
    if [[ "$MODE" == full ]]; then
      for example in line_index_profile folding_allocations; do
        cp "$RUNNER_TEMP/heap-candidate/release/examples/$example$EXE_SUFFIX" "$MEMORY_BIN_DIR/"
      done
      symbols "$RUNNER_TEMP/heap-candidate/release/examples" "$MEMORY_BIN_DIR"
      pushd "$RUNNER_TEMP/perf-candidate"
      run "$PERF_TOOL" native memory provenance --baseline-revision "$BASE_SHA" \
        --candidate-revision "$CANDIDATE_SHA" --commands-file "$PERF_ROOT/commands.txt" \
        --binary-dir "$TIMING_BIN_DIR" --binary-dir "$MEMORY_BIN_DIR" \
        --binary-dir "$PROFILE_BIN_DIR" --output "$PERF_ROOT/provenance.json"
      popd
    fi
    ;;
  profiles)
    failed=0
    scenarios=("$SCENARIO")
    if [[ "$MODE" == full ]]; then scenarios=(discovery-10 discovery-1000 discovery-10000 latency); fi
    backends=("$MODE")
    if [[ "$MODE" == full || "$MODE" == verification ]]; then backends=(cpu heap native); fi
    pushd "$RUNNER_TEMP/perf-candidate"
    for scenario in "${scenarios[@]}"; do
      for backend in "${backends[@]}"; do
        binary="$PROFILE_BIN_DIR/bend2-lsp$EXE_SUFFIX"
        if [[ "$backend" == heap ]]; then binary="$MEMORY_BIN_DIR/bend2-lsp$EXE_SUFFIX"; fi
        profile "$scenario" "$backend" "$binary"
        if [[ "$backend" == native && "$RUNNER_OS" != Linux && "$MODE" != native ]]; then
          profile "$scenario" native-heap "$binary"
        fi
      done
    done
    popd
    exit "$failed"
    ;;
  memory)
    run "$PERF_TOOL" native memory collect --binary-dir "$MEMORY_BIN_DIR" \
      --provenance "$PERF_ROOT/provenance.json" --output-dir "$PERF_ROOT/memory"
    ;;
  report)
    { printf '%q ' "$PERF_TOOL" dashboard "$PERF_ROOT" --target-only; printf '\n'; } >> "$PERF_ROOT/commands.txt"
    inventory_status=0
    "$PERF_TOOL" workflow finalize || inventory_status=$?
    "$PERF_TOOL" dashboard "$PERF_ROOT" --target-only
    printf '### Native performance evidence\n\nTarget: %s\n\nDownload this job artifact for index.html, unified-report.json, summary.md, and validated raw evidence.\n' "$PERF_TARGET" >> "$GITHUB_STEP_SUMMARY"
    exit "$inventory_status"
    ;;
  finalize)
    "$PERF_TOOL" workflow finalize
    ;;
  *) echo 'Unknown workflow stage' >&2; exit 1 ;;
esac
