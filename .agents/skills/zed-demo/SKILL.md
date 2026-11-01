---
name: zed-demo
description: "Use when recording, improving, reviewing or attaching editor-feature demo videos in this repository: headless Zed, visible mouse/keyboard input, PR walkthroughs, or requests such as 'сними демо', 'запиши видео' and 'сделай нагляднее'. Not a replacement for compiler-semantic or LSP E2E tests."
---

# Zed feature demos

Use the repository's real input-driven recorder. Scripts execute the workflow;
this skill defines the expected presentation, evidence and safety boundaries.

The repository root is `../../..` relative to this skill directory. Read the
[recording guide](../../../docs/editor-demos.md) before executing commands.
It is the source of truth for prerequisites, supported source revisions, flags,
output paths and scene extension points. Run commands from that repository root:

```sh
python3 scripts/zed_demo.py --help
```

## Expected style

- **One clear capability per clip.** Use a small meaningful sample. Do not make
  the viewer search through a large project or infer what the clip demonstrates.
- **Before → real action → visible result.** Establish the initial warning or
  editor state, show the actual gesture, then hold the changed state long enough
  to read it. For unsaved behavior, keep the dirty-buffer indicator visible.
- **Show the interaction.** Use a clearly visible, high-contrast pointer, click
  rings for real clicks, and large keycaps/typed-text cues from the input journal.
  Do not add a key badge to a wait/result frame or invent an input from a diff.
- **Frame the relevant code.** Use readable text at the embedded PR-player size,
  not only when zooming a full-resolution image. Show both files/panes when the
  claim is about an imported dependency or a cross-file result.
- **Short, concrete captions.** Describe what the user did and what changed.
  Prefer plain English for public PR demos unless requested otherwise. Avoid
  implementation terminology, debug coordinate notes and unrelated UI clutter.
- **Deliberate pacing, honest timing.** Pause at the baseline, menu choice and
  final result; avoid long idle stretches. Presentation holds are scripted for
  readability, not an analysis-latency measurement. Never imply they are one.
- **Show the real menu and consequences.** An explicit dangerous fix must be
  visibly selected and confirmed, not applied invisibly before the filmed step.
  Its potentially dangerous status must remain clear.

## Recording workflow

1. Identify the exact user-visible claim and the state that proves it. Reuse an
   existing scene if it demonstrates that claim; otherwise extend the native
   scene and its assertions using the guide. Do not relabel an unrelated clip.
2. Resolve actual tool/source paths. Do not reuse temporary paths from old chat
   logs without checking them. A compiler needs its complete distribution, not
   merely its executable. Check for a compatible built driver or owned cache
   before starting another expensive build.
3. Use `scripts/zed_demo.py prepare`, `record` and `render` as documented. Keep
   installation, dependency downloads and cold builds outside the filmed steps.
   Extend the existing scripts rather than creating another recorder or DSL.
4. Route every filmed edit, click and menu selection through genuine GPUI input.
   Never call direct editor-edit, selection or action-handler APIs while claiming
   to demonstrate a mouse/key gesture. Never synthesize diagnostics or menus.
5. Verify the actual buffer text, diagnostic transition, dirty state and unchanged
   saved fixtures. A successful lint result does not excuse a compiler/setup error
   elsewhere on screen. Repair the setup; do not crop, suppress or paint over it.
6. Inspect the produced MP4 at a practical viewing size, including the menu and
   the final decoded frame. Check its proof and full-decode result. Automated
   checks do not establish readability by themselves.

## Safety and publication

- Use the isolated, windowless Workspace and private fixture/profile directories.
  Never drive the user's installed Zed, global mouse/keyboard, desktop capture,
  personal projects or personal editor settings. Do not open a desktop editor as
  a fallback when headless recording is unavailable.
- Recording currently requires the documented macOS Metal implementation. State
  unsupported prerequisites/platforms; do not substitute a fake recording or
  claim that installed Zed has a compatible headless CLI.
- Keep autosave and format-on-save off in the bundled live-lint scenes. Never
  invoke `--fix-dangerously`; demonstrate an explicitly requested LSP Code Action.
- Use explicit run/build-cache directories. Reuse compatible prepared state;
  never silently wipe a foreign cache or prune unrelated targets/volumes.
- Keep input journals, native reports, source identities and movie proofs. State
  only versions/build provenance actually established by those tools and records.
- Review frames and captions for private paths, source content, credentials and
  irrelevant errors before sharing. Synthetic unit-test images are not footage.
- **Recording is not permission to publish.** Only when the user authorizes an
  upload, inspect `publish --preview`, then use the script's native PR attachments.
  Never create a release, merge a PR or commit media as part of recording.
- Finish with the playable artifacts, what to watch in each, exercised checks and
  any real limitation. Do not call a readable walkthrough a replacement for E2Es.
