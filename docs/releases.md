# Binary releases

Two independent workflows consume a successful `quality` run originating from a
`push` to this repository's `main`. Pull-request runs (including same-repository
PRs), forks, manual quality runs, and unsuccessful runs cannot start publication.
Every checkout and binary is tied to the originating quality run's full commit
SHA, not whatever `main` points to when a runner starts.

- **Nightly:** each eligible push builds an immutable prerelease candidate named
  `nightly-YYYY-MM-DD-<full-sha>`. The UTC date comes from the originating quality
  run's creation time, so rerunning that run keeps the identity. There is no
  floating `nightly` tag; nightlies never replace GitHub's latest stable release.
  "Nightly" is the development channel, not a scheduled daily build.
  Immediately before making it public, the publisher checks that the GitHub
  `main` branch API still points to the quality-tested source SHA. Obsolete
  candidates are skipped (an already-created draft may remain private), rather
  than publishing an older build after `main` has advanced. Existing public
  nightlies stay immutable.
- **Stable:** release-please maintains a Rust release PR from Conventional
  Commits. Merging that PR updates `Cargo.toml`, `Cargo.lock`, the release manifest,
  and the generated changelog. After quality succeeds, release-please creates a
  **draft** `vMAJOR.MINOR.PATCH` release and forces its tag into existence. Only a
  draft whose tag resolves to the exact quality-tested SHA is a build candidate.
  Ordinary subsequent commits retaining the same version are not release
  candidates. API-based PR maintenance can observe newer `main` commits, but
  that never substitutes their SHA for the quality-tested release candidate.

The manifest starts at the package's existing `0.1.1`. It is a version baseline,
not a claim that a historical `v0.1.1` release already exists; the first release PR
proposes the next version from eligible commits. There are no fabricated historic
releases or tags. Until the first release PR is merged, release-please may collect
all available Conventional Commit history. If maintainers want a shorter initial
changelog, they can deliberately add a full `bootstrap-sha` to the config before
the first run (the boundary commit is excluded). To choose a particular first
version, use release-please's documented `Release-As: X.Y.Z` commit footer. Review
the proposed version and changelog rather than assuming an automatic first
publication. Pre-1.0 breaking changes bump the minor version; feature commits
also bump the minor version.

## Native platforms and gates

| Executable target | Native GitHub-hosted runner | Extension |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | none |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | none |
| `x86_64-apple-darwin` | `macos-15-intel` | none |
| `aarch64-apple-darwin` | `macos-15` | none |
| `x86_64-pc-windows-msvc` | `windows-2022` | `.exe` |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | `.exe` |

The reusable `release-binaries` workflow checks both runner architecture and the
Rust compiler's host triple; it does not cross-compile or run x86 binaries under
ARM emulation. Each runner installs the pinned native Rust 1.98.1 toolchain and
runs, in order:

```sh
cargo build --locked --release --bin bend2-lsp
# scripts/release.py copies the final executable and hashes its bytes.
# BEND2_LSP_TEST_BINARY points to that exact release asset.
cargo nextest run --workspace --all-features --locked --release --profile ci
cargo test --doc --locked --release
cargo nextest run --locked --test release_e2e --profile ci
```

Executable/version integrity regression tests also run on each native
runner with `python -m unittest discover -s scripts -p test_release.py`
(`python3` on Unix).

Git attributes preserve LF checkout bytes for benchmark source fixtures on
Windows, keeping byte-offset golden comparisons identical across platforms.
Native Rust host verification uses Python rather than Bash-version-specific
command-substitution parsing.

The dedicated portable E2E test exercises the real stdio LSP process using a
controlled compiler fixture. It does not download or certify an upstream Bend
compiler on each platform. Existing Unix integration tests remain Unix-only;
the portable release E2E is mandatory everywhere, including both Windows
architectures. Full release-profile tests additionally run natively on all six
runners. Nextest uses quality's serialized CI profile: retries collect failure
evidence, but `flaky-result = "fail"` rejects a flaky test even if its retry passes.
Doctests still run separately.

Publication depends on the entire matrix succeeding. The publisher verifies
all six executables, their checksums, internal version/channel/SHA/target/filename
metadata, and the tag's resolved commit. Metadata stays in CI artifacts; public
assets are six executables, six checksum files, and `SHA256SUMS`. Assets upload
into a draft and their remote sizes and SHA-256 digests must match before it
becomes public. A failed build, E2E, or upload leaves it draft-only.

The quality gate also runs pinned actionlint, ghalint, and zizmor. Privileged
consumers use individually guarded `workflow_run` events; no workflow uses
`pull_request_target`. The policy evaluator runs default-branch code without
checking out PR code and publishes `protect` on the exact validated PR head.
**The Callgrind comparison and policy evaluation cover PRs**; release workflows
do not rerun them on pushes. Their mandatory enforcement depends on server-side
branch rules. A successful push quality run does not prove those rules exist.

## One-time repository setup

These are administrator operations, not changes performed by preparing these
files. Do not commit a private key or token.

1. Enable Actions and allow the pinned official GitHub actions and
   `googleapis/release-please-action`. Ensure all six runner labels above are
   available under the repository's plan and organization runner policy. ARM
   availability and image versions can change; no runner is silently replaced
   with cross-compilation. Private repositories consume the plan's Actions
   minutes; check billing and concurrent-job limits.
2. Create a GitHub App for release-please with **repository Contents: read/write,
   Issues: read/write, Pull requests: read/write**, and the implicit Metadata:
   read permission. No administration or review-bypass permissions are needed.
   Install it on this repository only. It needs to create/update release PRs,
   release labels, draft releases, and tags. It must not approve its own PRs or
   be granted ruleset bypass.
3. Generate an App private key. Set Actions repository secrets
   `RELEASE_APP_ID` (the App's numeric ID, not its installation ID) and
   `RELEASE_APP_PRIVATE_KEY` (the complete PEM key). The official token action
   discovers the installation and requests a token restricted to this repository
   and the three permissions above. It revokes the token after the job. No
   long-lived PAT or manually supplied installation ID is required.
4. Keep default `GITHUB_TOKEN` workflow permissions **read-only** and "Allow
   GitHub Actions to create and approve pull requests" **disabled**. The App
   token, not `GITHUB_TOKEN`, creates release PRs; this lets their normal PR
   workflows run despite GitHub's recursion restriction on `GITHUB_TOKEN`
   events. Publication jobs alone request `contents: write`. There is no
   automated approval, merge, policy label, or setting change in these files.
5. Create the `policy-approved` label. Protect `main` with a ruleset requiring
   pull requests, independent human review, up-to-date branches, `quality`,
   `compare`, and `protect`. Block force pushes and bypass, including bypass by
   the release App. Release PRs change protected Cargo policy files, so a
   maintainer must review those changes and apply `policy-approved`. The App
   does not apply that label. Label changes automatically retrigger unprivileged
   quality and then trusted policy evaluation. Dispatch `policy-integrity` on the
   default branch with `pr_number` to reevaluate directly without a full quality run.
6. Merge the prepared files through the normal reviewed PR process, then merge
   Conventional Commits (`fix:`, `feat:`, or breaking-change markers). Observe
   the `quality`, `nightly`, and `release` runs. Review and merge the release PR
   when ready for a semantic release. No additional tag push or manual release
   publication is needed.

The release App's key is accessed only after the `workflow_run` event passes
success/push/main/same-repository guards. Build jobs receive no App credentials
and have read-only repository access. No PR checkout or PR artifact is consumed
by a privileged release job. Workflow definitions and helper scripts are trusted
`main` code, so branch protection and human review remain essential.

## Retry and recovery

Use GitHub's **Re-run all jobs** on the failed `nightly` or `release` run after
repairing the external prerequisite, or rerun the corresponding `quality` push
run to retrigger the consumers. A manually dispatched quality run is intentionally
not a publication trigger. Rerun all release jobs rather than only a publisher
whose required artifacts may have expired (workflow artifacts are kept 14 days).

Draft retries upload only missing assets and reject differing existing bytes.
Public release assets and tags are never moved or overwritten. Stable candidate
discovery uses the existing draft and exact tag SHA, so it does not depend on
release-please reporting `release_created` again. Complete public releases stay untouched.
A tag pointing to a different SHA or an incomplete public asset set is an error,
not permission to replace history. Never manually publish a failed draft to
bypass the native matrix. A repaired source change needs a new reviewed commit,
quality run, and appropriate release PR/version, rather than retagging a released
version.

## Installing an executable

Choose a stable release on the repository's GitHub **Releases** page, or explicitly
choose a nightly prerelease. Download the executable matching your OS and CPU
plus its `.sha256` file (or `SHA256SUMS`). Names are `bend2-lsp-<tag>-<target>`,
with `.exe` appended for Windows. New releases contain no archives; previously
published archived releases remain unchanged.

```sh
# Linux: substitute the exact downloaded filename.
sha256sum --check bend2-lsp-<tag>-<target>.sha256
# macOS:
shasum -a 256 --check bend2-lsp-<tag>-<target>.sha256
chmod +x bend2-lsp-<tag>-<target>
```

On Windows PowerShell, compare the actual hash against the first field of the
sidecar file, and stop if they differ:

```powershell
$binary = 'bend2-lsp-<tag>-<target>.exe'
$expected = (Get-Content "$binary.sha256").Split(' ')[0]
if ((Get-FileHash $binary -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
    throw 'Executable checksum mismatch'
}
```

The executable does not bundle the Bend compiler or an editor extension. The
Apache-2.0 license remains available as this repository's `LICENSE`; retain a
copy with redistributed binaries and account for dependency license obligations.
SHA-256 provides integrity relative to the downloaded checksum, not an
independent publisher signature. **No code signing, Apple notarization,
Authenticode signature, or provenance attestation is provided yet.** Windows
SmartScreen or macOS Gatekeeper may warn; follow local security policy rather
than automatically disabling protections.

Place the executable on `PATH` as `bend2-lsp` (Windows: `bend2-lsp.exe`), or point
your editor at its absolute path. On Unix, set its executable permission.
Configure `.bend` files to
use language ID `bend` or `bend2` and launch the server over stdio. The process is
an LSP endpoint, not an interactive shell command or a `--version` health probe.

Binaries are built on the listed operating-system images, not certified for all
older releases. Linux uses GNU/glibc, not musl: Alpine and systems with older
glibc than the build image may not work. macOS artifacts are separate Intel and
Apple Silicon executables, not universal bundles. Windows uses MSVC and may need
the matching Visual C++ runtime. Install an independently supported Bend 2 CLI
and make `bend` available on `PATH`, or set `bend2-lsp.compilerPath`. Upstream
compiler availability—especially on Windows—is a separate prerequisite for
compiler diagnostics and `Base` loading; the server's binary matrix does not
promise an upstream Bend installation on every target.

## Primary references and pinned actions

- [Release-please action and Rust strategy](https://github.com/googleapis/release-please-action/tree/5c625bfb5d1ff62eadeeb3772007f7f66fdcf071)
- [Manifest bootstrap and version baselines](https://github.com/googleapis/release-please/blob/main/docs/manifest-releaser.md)
- [Official configuration schema (`draft`, `force-tag-creation`)](https://github.com/googleapis/release-please/blob/main/schemas/config.json)
- [GitHub App token action and permission inputs](https://github.com/actions/create-github-app-token/tree/fee1f7d63c2ff003460e3d139729b119787bc349)
- [GitHub-hosted native runner labels and limitations](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [Workflow-run security and event behavior](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_run)
- [Why App-created PRs trigger checks](https://docs.github.com/en/actions/how-tos/writing-workflows/choosing-when-your-workflow-runs/triggering-a-workflow)

Action revisions were resolved from GitHub's repository commit API, not guessed:
checkout `v4.2.2` → `11bd71901bbe5b1630ceea73d27597364c9af683`, upload-artifact
`v4.6.2` → `ea165f8d65b6e75b540449e92b4886f43607fa02`, download-artifact `v4.3.0`
→ `d3f86a106a0bac45b974a628896c90dbdf5c8093`, create-github-app-token `v2` →
`fee1f7d63c2ff003460e3d139729b119787bc349`, and release-please-action `v4` →
`5c625bfb5d1ff62eadeeb3772007f7f66fdcf071` (action package version 4.4.1).
