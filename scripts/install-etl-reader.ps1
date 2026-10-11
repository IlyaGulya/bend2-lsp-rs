$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if ($env:GITHUB_ACTIONS -ne "true") { throw "Native profiler tool bootstrap is hosted-only" }
$rust = & rustc -vV
if ($LASTEXITCODE -ne 0) { throw "Cannot identify native Rust host" }
if ($rust -match "host: aarch64-pc-windows-msvc") {
    $rid = "win-arm64"
    $sdkHash = "8272eaab6f06ad658b1976e19d88beed287a601f968b71d5c26b75d10587cf087665c599d2e136a911002f955c97f59aa8692581bbf6b8e7af5f82604c810256"
} elseif ($rust -match "host: x86_64-pc-windows-msvc") {
    $rid = "win-x64"
    $sdkHash = "24b670ad3d923bfcf47df6c3b034152398b42f6dbc388e10d783aee1cfb5e5817d399fc0ae2a12cfa822a55e61d34830ccb15c50ef6efee437ab874bb7c79430"
} else { throw "Unsupported native ETL decoder host" }
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../tools/perf/windows-etl-reader"))
$installRoot = $env:CARGO_INSTALL_ROOT
if (!$installRoot) { $installRoot = $env:CARGO_HOME }
if (!$installRoot) { $installRoot = Join-Path $env:USERPROFILE ".cargo" }
$destination = Join-Path $installRoot "bin/bend2-etl-reader"
$temporary = Join-Path $env:TEMP ("bend2-etl-reader-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    # Exact Microsoft SDK release bytes, not the mutable dotnet-install script.
    $archive = Join-Path $temporary "sdk.zip"
    Invoke-WebRequest -Uri "https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.401/dotnet-sdk-10.0.401-$rid.zip" -OutFile $archive
    if ((Get-FileHash -Algorithm SHA512 $archive).Hash.ToLowerInvariant() -ne $sdkHash) {
        throw "Pinned .NET SDK archive SHA512 mismatch"
    }
    $sdk = Join-Path $temporary "sdk"
    Expand-Archive -LiteralPath $archive -DestinationPath $sdk
    $dotnet = Join-Path $sdk "dotnet.exe"
    $env:DOTNET_ROOT = $sdk
    $env:DOTNET_CLI_HOME = Join-Path $temporary "home"
    $env:DOTNET_CLI_TELEMETRY_OPTOUT = "1"
    $env:DOTNET_NOLOGO = "1"
    $env:NUGET_PACKAGES = Join-Path $temporary "packages"
    $version = & $dotnet --version
    if ($LASTEXITCODE -ne 0 -or $version -ne "10.0.401") { throw "Pinned SDK version mismatch" }
    Push-Location $project
    try {
        # Persistent compiler/MSBuild servers would retain this temporary SDK's DLLs.
        & $dotnet restore --locked-mode --disable-build-servers
        if ($LASTEXITCODE -ne 0) { throw "Locked native decoder package restore failed" }
        & $dotnet publish --no-restore --disable-build-servers --configuration Release --runtime $rid --output $destination
        if ($LASTEXITCODE -ne 0) { throw "Native decoder publish failed" }
    } finally { Pop-Location }
    $executable = Join-Path $destination "bend2-etl-reader.exe"
    & $executable --help
    if ($LASTEXITCODE -ne 0) { throw "Published native decoder failed its CLI smoke" }
    $sources = @{}
    foreach ($name in @("Program.cs", "Bend2EtlReader.csproj", "Directory.Build.props", "global.json", "NuGet.config", "packages.lock.json")) {
        $sources[$name] = (Get-FileHash -Algorithm SHA256 (Join-Path $project $name)).Hash.ToLowerInvariant()
    }
    $identity = @{
        format_version = 1
        sdk_version = "10.0.401"
        sdk_archive_sha512 = $sdkHash
        runtime_version = "10.0.12"
        traceevent_version = "3.2.8"
        target = $rid
        sources = $sources
        executable_sha256 = (Get-FileHash -Algorithm SHA256 $executable).Hash.ToLowerInvariant()
    }
    $identity | ConvertTo-Json -Depth 4 | Set-Content -Encoding ascii ($executable + ".bend-perf-source.json")
    $identity | ConvertTo-Json -Depth 4
    $destination | Out-File -Append -Encoding utf8 $env:GITHUB_PATH
} finally {
    Remove-Item -Recurse -Force $temporary
}
