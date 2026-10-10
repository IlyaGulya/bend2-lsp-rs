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
cargo nextest archive --workspace --all-features --locked --release \
  --archive-file target/release-tests.tar.zst
cargo test --doc --locked --release
cargo nextest run --archive-file target/release-tests.tar.zst \
  --workspace-remap "$GITHUB_WORKSPACE" -E 'binary(release_e2e)' --profile ci
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

The full release suite is compiled once per native target. Nextest archives
those same test binaries with the identical workspace, feature, lockfile, and
release flags; the mandatory direct-executable E2E runs from that archive, not
from a separately compiled debug suite. Each archive lives outside `dist` at
`target/release-tests.tar.zst` and transfers within the same workflow run as the
internal artifact `native-tests-${channel}-${target}`. Installer E2E downloads
only its exact target's artifact into `native-tests`, requires
`RELEASE_TEST_ARCHIVE` to point to it, and runs:

```sh
cargo nextest run --archive-file "$RELEASE_TEST_ARCHIVE" \
  --workspace-remap "$GITHUB_WORKSPACE" -E 'binary(release_e2e)' --profile ci
```

The checkout is verified against the release source SHA, and both producers and
consumers verify the native host target. `BEND2_LSP_TEST_BINARY` selects the
staged direct executable in native jobs and the actual installed executable in
installer jobs. A missing archive is an error, never a rebuild fallback.
Archives reuse compilation only: every selected test executes again under the
same strict fail-if-flaky policy; test outcomes are not cached. Installer jobs
retain pinned native Rust because the controlled compiler fixture is built at
runtime, as well as the pinned nextest bootstrap.

Publication depends on the entire native and installer matrices succeeding.
Each native runner packages an additional versionless `.tar.gz` (Unix, one
root directory) or `.zip` (Windows, flat) from the exact staged executable.
The publisher verifies extracted bytes against the direct asset and rejects
unexpected members, links, mismatched checksums or identity metadata.

After all native builds pass, pinned cargo-dist 0.33.0 generates only the global
Shell/PowerShell installers; it never rebuilds the binaries or owns publication.
`scripts/dist_installers.py` injects the six verified archive digests into its
manifest. The wrapper uses supported simple-hosting configuration for exact
nightly URLs, and normalizes the pinned GitHub mirror/fallback download routes
to that same tag while retaining the package's real version. A changed route
layout is an error. Cargo-dist 0.33.0 lacks
PowerShell checksum verification, so a version-checked, fail-closed download hook
adds SHA256 verification before extraction. A changed upstream hook is an error,
not permission to omit verification.

Every target then runs its actual installer against an isolated local mirror,
first rejecting a valid archive with altered binary bytes, then installing the
correct bytes and running the existing portable release protocol/lifecycle E2E
suite against the installed copy. No Unix-only latency transport is used on
Windows.
Unprivileged installer PR validation runs the same matrices without publication.

Public assets are six direct executables, six archives, two installers, their
fourteen checksum sidecars, and `SHA256SUMS`. Source/version/channel/target and
generator identity metadata and nextest test archives remain internal. Test
archives have a distinct artifact prefix, so public `${channel}-*` downloads
cannot include them, and they are never published. Remote sizes and SHA256 digests
must match before a draft becomes public; a failed build, installer, E2E or
upload leaves it private.

The quality gate also runs pinned actionlint, ghalint, and zizmor. Privileged
consumers use individually guarded `workflow_run` events; no workflow uses
`pull_request_target`. **The Callgrind comparison covers PRs**; release workflows
do not rerun it on pushes. Mandatory CI and maintainer review enforcement depend
on server-side branch rules. A successful push quality run does not prove those
rules exist.

### CI caches and native tool installation

Quality and native release builds use pinned sccache 0.18.0
with [BuildFetch WebDAV storage](https://buildfetch.com/docs/buildfetch-cache/sccache-remote-storage),
not GitHub's compiler-object cache backend. Rust incremental compilation is
disabled only for these wrapped builds. The wrapper caches eligible Rust library compilation;
linked executables and procedural macros still compile normally. The final
release executable is staged, hashed, and exercised through the same native
and installed-binary gates as before.
Installer E2E reuses the same-run native test archive and does not restore or
save Cargo dependency caches or configure sccache.

All build jobs restore only Cargo registry downloads and Git dependency
checkouts, never Cargo credentials, configuration, rustup wrappers, or measured
target directories. Quality tools and the Iai runner have separate caches
including Cargo installation metadata. Keys include native platform, the running
Rust compiler identity, and the pinned installation recipe; quality tool keys
also include the Go version. Backend configuration changes do not invalidate
download or tool caches. Cargo download keys retain the project lockfile and
tool recipe because both select downloaded dependencies.
Tool restores may fall back to another recipe on the same platform/compiler
(and Go version for quality tools); the installation script still checks or
installs every pinned version before any checks run.
Cold Cargo quality-tool builds also use sccache, with a separate, step-local
`CARGO_TARGET_DIR`. This directory is neither archived nor shared with project
or measured builds. The final tool binaries still link normally; restored
installed binaries avoid that work entirely.
To exercise source installation while preserving the existing installed-tool
cache, dispatch `gh workflow run quality.yml --ref main -f rebuild-tools=true`.
This skips only installed-tool cache restoration and saving, not quality gates
or the compiler cache; tool-stage cache statistics are printed separately.

Only successful trusted `push` or `workflow_dispatch` runs on `main` save
download/tool caches. BuildFetch uses `BUILDFETCH_TOKEN_READWRITE` for these trusted
main builds and `BUILDFETCH_TOKEN_READONLY` for internal PR and publication
`workflow_run` consumers. Fork PRs receive neither token and use only the
job-local compiler cache. GitHub caches still hold Cargo downloads and installed
tools, not sccache objects. The `cache-warm` workflow seeds all six
native platforms from trusted main changes or a manual main dispatch, runs the
full native/installer matrices, and never publishes a release. Its separate
Ubuntu job seeds the pinned Iai runner without running calibration.

Callgrind comparison, calibration, and paired latency builds restore dependency
inputs and tools only: no compiler wrapper, shared objects, or restored measured
targets. Their independent-build and regression policies remain unchanged.
Cache hits and sccache statistics are visible in job logs and summaries.

Add both BuildFetch tokens directly as repository Actions secrets; never commit
or paste them into an issue, PR, or chat. Interactive local commands:

```sh
gh secret set BUILDFETCH_TOKEN_READONLY --repo IlyaGulya/bend2-lsp-rs
gh secret set BUILDFETCH_TOKEN_READWRITE --repo IlyaGulya/bend2-lsp-rs
```

The endpoint, `token-auth` username, and
[pinned WebDAV access modes](https://github.com/mozilla/sccache/blob/v0.18.0/docs/Webdav.md)
are configured by `scripts/ci-cache.sh`. Read-only access is enforced by the
readonly token, not just a client environment flag. Reusable native workflows
receive explicit cache secrets, never inherited release-App credentials.
The endpoint has no trailing slash: pinned OpenDAL joins it with an absolute
WebDAV path. Do not set `SCCACHE_WEBDAV_KEY_PREFIX` to the project ID: it is
already part of the endpoint, so setting it duplicates the ID in request URLs.
The corrected provider route is `/sccache/<projectId>`, not
`/<projectId>/sccache`; the project-generated instructions previously had both errors.
sccache's own input keys distinguish compilers, platforms, and build flags.

The trusted-main `buildfetch-probe` workflow writes a real Rust library, stops the
sccache server, then uses the readonly token in a new server to retrieve the
same artifact and execute a linked consumer. Its JSON statistics are preserved
as `buildfetch-probe-*` artifacts, including a depth-zero request for the
standard WebDAV quota properties. Missing quota properties or unsupported HTTP
responses remain explicitly unknown. Provider-reported quota may cover more
than this project's objects; confirm its scope through BuildFetch usage reporting.
Unknown WebDAV `cache_size` is not zero usage.
Probe I/O failures also preserve info-level sccache diagnostics as `.error.txt`
in the same artifact. Password and Basic-auth credential forms are redacted
before printing or uploading; raw daemon logs stay in the runner's temporary directory.
On writer failure, protocol diagnostics report `MKCOL`, direct `PUT`, and `GET`
statuses for sccache's reserved `.sccache_check` health file at the configured
endpoint. Only status and payload-match metadata are retained;
these requests do not store a compiler artifact or satisfy the roundtrip gate.
For an isolated compiler/cache check without native builds or installer E2E, run
`gh workflow run buildfetch-probe.yml --ref main`. The same standalone workflow
runs on main pushes, independently of `cache-warm`; both use the shared cache helper.
Changes only to the probe script do not trigger the full native cache-warm matrix.
The provider dashboard currently allocates 20 GB to this project. That is a
storage limit, not a measured requirement. The historical approximately 1.85 GiB
GitHub cache observation is not a BuildFetch footprint; measure project-scoped
usage after native warming before changing the allocation.

Native jobs install official nextest 0.9.131 archives through
`scripts/bootstrap_nextest.py`, not twelve independent source builds.
The bootstrap verifies reviewed archive sizes/SHA256 and copies only the
expected regular executable. macOS uses the universal archive; Windows ARM64
selection follows the validated Rust target rather than Python's architecture.

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
   automated approval, merge, or setting change in these files.
5. Protect `main` with a ruleset requiring pull requests, maintainer review,
   up-to-date branches, `quality`, and `compare`. Block force pushes and bypass,
   including bypass by the release App. Review release PRs through the ordinary
   GitHub review process before merging.
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
choose a nightly prerelease. Direct asset names are `bend2-lsp-<target>`, with
`.exe` appended for Windows; the tag and metadata retain the version.
Previously published assets remain unchanged. v0.2.3 and earlier have only
direct executable assets. Releases with installer support additionally contain
`bend2-lsp-<target>.tar.gz` (Unix) or `.zip` (Windows) and versionless
`bend2-lsp-installer.sh` / `bend2-lsp-installer.ps1`.

For a stable release containing installers:

```sh
curl -fsSL https://github.com/IlyaGulya/bend2-lsp-rs/releases/latest/download/bend2-lsp-installer.sh | sh
```

```powershell
irm https://github.com/IlyaGulya/bend2-lsp-rs/releases/latest/download/bend2-lsp-installer.ps1 | iex
```

To pin a version or select a nightly, use `releases/download/<exact-tag>` instead
of `releases/latest/download`. Installation is per-user at `~/.local/bin`
(Windows: `$HOME\.local\bin`) and configures user PATH; reopen the editor/terminal.
Use `INSTALLER_NO_MODIFY_PATH=1` to preserve PATH, `BEND2_LSP_INSTALL_DIR` to
choose a directory, or `BEND2_LSP_UNMANAGED_INSTALL` for isolated flat installation
without profile/registry/receipt changes. Linux GNU archives require glibc 2.39
or newer; no musl fallback is advertised.

Both installers verify embedded SHA256 values before extraction. That does not
authenticate the downloaded installer itself: piping to a shell/`iex` executes
remote code. Download and inspect it first when required by local policy.
Updating means rerunning an installer for the selected release; neither the
server nor an additional updater downloads upgrades in the background.

For manual installation, download the matching direct executable plus its
`.sha256` file (or `SHA256SUMS`) and verify:

```sh
# Linux: substitute the exact downloaded filename.
sha256sum --check bend2-lsp-<target>.sha256
# macOS:
shasum -a 256 --check bend2-lsp-<target>.sha256
chmod +x bend2-lsp-<target>
```

On Windows PowerShell, compare the actual hash against the first field of the
sidecar file, and stop if they differ:

```powershell
$binary = 'bend2-lsp-<target>.exe'
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
