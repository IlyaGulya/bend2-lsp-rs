# Input-driven Zed demos

Use `python3 scripts/zed_demo.py --help`. This is a developer recording tool,
not a flag for the installed Zed application and not a replacement for LSP E2Es.
It does not open desktop windows, install an extension, change personal Zed
settings, or record global mouse/keyboard input.

## Prerequisites

- Python 3.11 or newer. Preparation, recording and capture-authority tests use
  only the standard library.
- macOS for recording: the bundled driver uses Zed's windowless Metal renderer.
- A Zed source checkout. Tested with Zed 1.22.0 at
  `76659a55a8c10ed355a070f8764a0b1733e3c115`. The caller's checkout is read-only;
  the build command creates a separate clone inside the explicit build cache.
  Other revisions must still expose the required headless APIs and compile the
  driver; they are not claimed to be supported merely because a version matches.
- The already-installed Rust 1.98.1 toolchain, Git and macOS build prerequisites
  for Zed. Rustup auto-install is disabled. Cargo may fetch build dependencies.
- A built `bend2-lsp` with live bend-lint SDK 0.3.0 support, Bun 1.4.4 and the
  matching SDK `src/linter/seam.ts`, a **complete** Bend compiler distribution,
  and the Bend Zed extension checkout with `grammars/bend2.wasm` already built.
  The fixtures select `bend-lint-rules` 0.3.0.0. These scripts do not build,
  download or install the LSP, SDK, compiler or extension.
- Rendering: FFmpeg, FFprobe and the explicitly installed optional dependency
  from `scripts/zed_demo_requirements.txt`. Use an external virtual environment:

  ```sh
  python3 -m venv "$DEMO_VENV"
  "$DEMO_VENV/bin/python" -m pip install -r scripts/zed_demo_requirements.txt
  ```

- Publishing only: an authenticated GitHub CLI supporting `gh pr comment --attach`.

A compiler executable alone is insufficient: keep its `bend2/base.bend` and the
rest of the distribution. A successful lint response does not excuse a broken
compiler installation. The walkthrough refuses captured error diagnostics.

## Prepare, record, render

Run from this repository. Set the variables below to **absolute paths** to the
prerequisites and output directories you selected; no machine-specific paths
are embedded in the scripts. `RUN` must be new or empty, never your own project.
`BUILD_CACHE` must be outside the Zed source checkout and reused across runs.
`LINT_CACHE` can point to a prepared SDK cache shared by the three fixtures.

```sh
python3 scripts/zed_demo.py prepare \
  --run "$RUN" \
  --lsp "$LSP" \
  --lint "$BUN" \
  --lint-arg "$BEND_LINT_SEAM" \
  --compiler "$BEND" \
  --lint-cache "$LINT_CACHE"

python3 scripts/zed_demo.py record \
  --run "$RUN" \
  --zed-source "$ZED_SOURCE" \
  --build-cache "$BUILD_CACHE" \
  --extension "$BEND_EXTENSION"

"$DEMO_VENV/bin/python" scripts/zed_demo.py render --run "$RUN"
```

`prepare` copies only the bundled sample projects and writes private project
settings. `record` launches an isolated windowless Workspace with the real LSP.
Every filmed edit, click and menu selection goes through genuine GPUI input
routing. No save command is sent. Source files, lint configuration and project
settings must still match their preparation baseline before and after capture.

The first Zed build is large: allow roughly 10 GiB for its target directory.
Later runs reuse the explicit cache; changes to the bundled driver refresh only
its owned files and rebuild into the same target. A foreign cache or changed
upstream revision is rejected, never silently wiped. SDK cold preparation is a
separate cost: reuse a prepared lint cache rather than treating clip duration as
an analysis benchmark.

If you already have this capture driver built, replace `--build-cache` with
`--driver "$DRIVER"`. Still pass `--zed-source`: Zed needs the checkout for
runtime assets. The recording receipt identifies the supplied binary by hash;
it does not invent a build revision for an arbitrary prebuilt driver.

Repeat `--scene NAME` on `record` or `render` to select scenarios:

| Scene | What it demonstrates |
| --- | --- |
| `clip1-unsaved-entry` | Delete `+`, re-indent through real input, and see live lint change without saving. |
| `clip2-imported-type` | Edit an imported function in a second pane; its caller loses the semantic finding while unrelated text findings remain. |
| `clip3-dangerous-action` | Open real Code Actions with Cmd-., select the dangerous formatting fix with Enter, and keep the resulting edit unsaved. |

A capture directory is not overwritten. After a failed attempt, inspect
`interactive-captures/SCENE/driver.log` and use a fresh prepared run. The record
command has a per-scene process deadline (`--timeout`, default 600 seconds),
stops only its own child process tree on timeout/interruption, and keeps failure
evidence instead of retrying or weakening the scene's assertions.
Interrupting a cold build also terminates and reaps its owned Cargo process
group, so compilation cannot continue writing to the cache after recording exits.

Rendering can run separately from recording. On macOS it uses system SF fonts;
on other platforms pass `--font` and `--mono-font` explicitly. Tool locations
can be set with `--ffmpeg` and `--ffprobe`.

## Inspect and publish

The run contains:

- `baselines/manifest.json`: byte identities of the saved fixtures and settings.
- `interactive-captures/SCENE/`: native PNGs, the original `raw-report.json`,
  a relocatable `report.json` with input journal and per-image hashes, plus the
  recording receipt and driver log.
- `interactive-videos/SCENE.mp4`: chapter captions, visible pointer/click cue,
  actual key/typed-text HUD, and the real editor frames.
- `interactive-videos/SCENE.proof.json`: source/image identities, input summary,
  composition coordinates, chapter timeline and encoded-media checks.
- `interactive-videos/index.json`: the verified movie inventory.

The renderer refuses missing/unhandled key events, false key badges, escaped
image paths, invalid geometry and invalid presentation holds. It checks H.264,
YUV420p, 30 fps, duration/frame-count agreement, fast-start and a full error-fatal
MP4 decode. **Watch the result too**: confirm readable source, the real menu,
correct final diagnostics and no irrelevant setup errors.

The cursor and key HUD annotate the recorded editor-event journal; they are not
a recording of the system cursor or global input. Presentation holds are
scripted for readability, not measured latency.

Publication is a separate, explicitly authorized action. First inspect the
comment without uploading anything:

```sh
python3 scripts/zed_demo.py publish --run "$RUN" --pr "$PR_URL" --preview
python3 scripts/zed_demo.py publish --run "$RUN" --pr "$PR_URL"
```

The command revalidates the capture and movie identities, then uses native
GitHub CLI attachments so the PR contains playable videos. It does not commit
media, create a release, merge a PR or install anything. Do not publish merely
because recording succeeded. See the [GitHub CLI attachment contract](https://docs.github.com/en/github-cli/github-cli/attaching-files-with-github-cli).

## Add a walkthrough / hand off to another agent

- Start with `scripts/zed_demo_assets/fixtures/`; add a small deterministic
  sample rather than capturing a personal project.
- Add the scene name to `FIXTURE_SCENES` in `scripts/zed_demo_build.py`.
- Extend `bend2_lint_demo_interactions.rs` for inputs, captions and expected
  transitions. `bend2_lint_demo_scenes.rs` owns shared capture/report helpers;
  `bend2_lint_demo.rs` boots the actual editor and language server.
- Dispatch mouse/key/IME events and assert the resulting buffer, menu,
  diagnostics and disk state. Never call direct edit/action handlers while
  claiming to demonstrate a user key press. Never draw fake diagnostics or menus.
- Keep all editor state under the run directory. Leave autosave and format-on-save
  disabled. Do not automate the user's installed Zed or alter personal settings.
- Run the packaged workflow, inspect actual frames and playback, then run
  `./scripts/quality`. Publish only with explicit authorization.

The project-local [zed-demo skill](../.agents/skills/zed-demo/SKILL.md) defines
the presentation style and safety contract; this guide owns the technical steps.
Repository `AGENTS.md` routes editor-demo requests to that skill, including for
clients that do not automatically list project skills.

A concise handoff is: “Use the zed-demo skill to record this feature.” The agent
does not need the previous chat. If its client has no skill loader, it can read
the linked `SKILL.md` directly.

Capture-authority regressions can be run without Pillow or an editor build:

```sh
python3 -m unittest discover -s scripts -p 'test_zed_demo*.py'
```

These unit fixtures are deliberately synthetic protocol inputs, not editor
evidence. Real editor footage remains the smoke proof. Optional Zed build
licensing and source provenance are in
[`scripts/zed_demo_assets/NOTICE`](../scripts/zed_demo_assets/NOTICE).
