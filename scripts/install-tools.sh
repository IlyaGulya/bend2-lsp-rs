#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Release-only generator; ordinary quality-tool installation remains unchanged.
if [[ "${1:-}" == "dist" ]]; then
  # The upstream 0.33.0 release is not published on crates.io. The global job
  # uses Linux x86_64; pin both the official binary release and its exact bytes.
  test "$(uname -s)" = Linux
  test "$(uname -m)" = x86_64
  temporary="$(mktemp -d)"
  trap 'rm -rf "$temporary"' EXIT
  archive="cargo-dist-x86_64-unknown-linux-gnu.tar.xz"
  curl -fsSL "https://github.com/axodotdev/cargo-dist/releases/download/v0.33.0/$archive" --output "$temporary/$archive"
  printf '%s  %s\n' '4b3f0a5f0ebbdb798f6db649d01b32ba1518376b6f7a0502b7d92b75cc2c8293' "$temporary/$archive" | sha256sum --check -
  tar -xJf "$temporary/$archive" -C "$temporary"
  mkdir -p "$HOME/.cargo/bin"
  install -m 755 "$temporary/cargo-dist-x86_64-unknown-linux-gnu/dist" "$HOME/.cargo/bin/dist"
  exit 0
fi

# Hosted diagnostic tooling is deliberately separate from existing quality tools.
# Official source pin: https://github.com/mstange/samply/releases/tag/samply-v0.13.1
if [[ "${1:-}" == "profiling" ]]; then
  if [[ "${GITHUB_ACTIONS:-}" != "true" ]]; then
    printf '%s\n' 'Profiling bootstrap is hosted-only; use cargo perf doctor for local prerequisites.' >&2
    exit 1
  fi
  samply_revision=da75c28f367454c621e690eeb4e44ec2ebb29a78
  rustc_identity="$(rustc -vV)"
  if [[ "$rustc_identity" == *$'\nhost: aarch64-pc-windows-msvc\n'* ]]; then
    # The pinned importer drops ARM64 SampleProf CPU deltas and mistakes split
    # kernel/user StackWalk records for independent samples. Only ARM64 is patched.
    patch_path=scripts/patches/samply-windows-arm64.patch
    patch_sha256=37bc36692372474829e099ce5faad0ca765184c76c01b31544fae5bd7365de51
    printf '%s  %s\n' "$patch_sha256" "$patch_path" | sha256sum --check -
    temporary="$(mktemp -d)"
    trap 'rm -rf "$temporary"' EXIT
    git -C "$temporary" init --quiet
    git -C "$temporary" config core.autocrlf false
    git -C "$temporary" remote add origin https://github.com/mstange/samply
    git -C "$temporary" fetch --depth 1 origin "$samply_revision"
    git -C "$temporary" checkout --detach --quiet FETCH_HEAD
    test "$(git -C "$temporary" rev-parse HEAD)" = "$samply_revision"
    git -C "$temporary" apply "$PWD/$patch_path"
    cargo test --locked --manifest-path "$temporary/samply/Cargo.toml" \
      --target aarch64-pc-windows-msvc bend2_arm64_regressions
    cargo install --locked --force --path "$temporary/samply" \
      --target aarch64-pc-windows-msvc samply
    BEND_PERF_SAMPLY_PATCH_SHA256="$patch_sha256" powershell.exe -NoProfile -NonInteractive -Command '
      $ErrorActionPreference = "Stop"
      $sampler = (Get-Command samply.exe -CommandType Application).Source
      $identity = @{
        format_version=1
        upstream_revision="da75c28f367454c621e690eeb4e44ec2ebb29a78"
        patch_path="scripts/patches/samply-windows-arm64.patch"
        patch_sha256=$env:BEND_PERF_SAMPLY_PATCH_SHA256
        target="aarch64-pc-windows-msvc"
        executable_sha256=(Get-FileHash -Algorithm SHA256 $sampler).Hash.ToLowerInvariant()
      }
      $identity | ConvertTo-Json | Set-Content -Encoding ascii ($sampler + ".bend-perf-source.json")
      $identity | ConvertTo-Json
    '
  else
    cargo install --locked --git https://github.com/mstange/samply \
      --rev "$samply_revision" samply
  fi
  case "$(uname -s)" in
    Darwin)
      # Explicit requested installation action, not an implicit doctor elevation.
      samply setup -y
      ;;
    MINGW*|MSYS*|CYGWIN*)
      # WPT contains xperf, required by samply's ETW backend. No runas/UAC.
      # ADK 10.1.26100.9457 (September 2026), official Microsoft fixed URL,
      # resolved from https://learn.microsoft.com/windows-hardware/get-started/adk-install.
      powershell.exe -NoProfile -NonInteractive -Command '
        $ErrorActionPreference = "Stop"
        $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if (!$admin) { throw "Windows profiling bootstrap requires an explicitly elevated hosted runner; no elevation is attempted." }
        $temporary = Join-Path $env:TEMP ("bend2-wpt-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Path $temporary | Out-Null
        try {
          $installer = Join-Path $temporary "adksetup.exe"
          Invoke-WebRequest -Uri "https://download.microsoft.com/download/8e0c0f5a-abb5-4358-a51b-168eb40b1590/adk/adksetup.exe" -OutFile $installer
          $hash = (Get-FileHash -Algorithm SHA256 $installer).Hash.ToLowerInvariant()
          if ($hash -ne "ac6a930fdb5c2980ba5fefe606d47edaafcf5f647b4337411500d158ea77300f") { throw "Pinned Microsoft ADK installer SHA256 mismatch" }
          $signature = Get-AuthenticodeSignature $installer
          if ($signature.Status -ne "Valid" -or $signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") { throw "Microsoft ADK installer signature is not valid" }
          $process = Start-Process -FilePath $installer -ArgumentList @("/quiet", "/norestart", "/features", "OptionId.WindowsPerformanceToolkit") -PassThru
          if (!$process.WaitForExit(1200000)) { $process.Kill(); $process.WaitForExit(); throw "ADK installation timed out" }
          if ($process.ExitCode -notin @(0,3010)) { throw "ADK WPT installation failed: $($process.ExitCode)" }
          $toolkit = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\Windows Performance Toolkit"
          $xperf = Join-Path $toolkit "xperf.exe"
          if (!(Test-Path $xperf)) { throw "ADK installation did not provide xperf.exe" }
          $toolkit | Out-File -Append -Encoding utf8 $env:GITHUB_PATH
          @{source="Microsoft ADK 10.1.26100.9457"; installer_sha256=$hash; xperf_path=$xperf; xperf_sha256=(Get-FileHash -Algorithm SHA256 $xperf).Hash; xperf_version=(Get-Item $xperf).VersionInfo.FileVersion} | ConvertTo-Json
        } finally {
          Remove-Item -Recurse -Force $temporary
        }
      '
      ;;
    Linux) ;;
    *)
      printf '%s\n' 'Unsupported hosted profiling platform' >&2
      exit 1
      ;;
  esac
  exit 0
fi

source scripts/check-tool-version.sh

# A dedicated install root lets CI restore binaries together with Cargo's
# installation metadata without caching rustup proxies or Cargo credentials.
if [[ "${1:-}" == "performance" ]]; then
  cargo install --locked --version 0.16.1 iai-callgrind-runner
  exit 0
fi

cargo install --locked --version 0.9.131 cargo-nextest
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.6.45 cargo-hack
cargo install --locked --version 1.30.1 zizmor
go_bin="${GOBIN:-$(go env GOPATH)/bin}"
if [[ ! -x "$go_bin/ghalint" ]] || ! check_tool_version ghalint 1.5.6 "$("$go_bin/ghalint" version)"; then
  go install -ldflags='-X main.version=1.5.6' github.com/suzuki-shunsuke/ghalint/cmd/ghalint@v1.5.6
fi
if [[ ! -x "$go_bin/actionlint" ]] || ! check_tool_version actionlint 1.7.12 "$("$go_bin/actionlint" --version)"; then
  go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12
fi
check_tool_version ghalint 1.5.6 "$("$go_bin/ghalint" version)"
check_tool_version actionlint 1.7.12 "$("$go_bin/actionlint" --version)"
