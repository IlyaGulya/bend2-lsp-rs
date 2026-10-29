#Requires -Version 7.2
<#
Investigation-only diagnostic for the Windows x64 failure of

  tests/bend_lint_protocol.rs::timeout_kills_analyzer_and_its_descendant_without_blocking_compiler

at tests/bend_lint_protocol.rs:234 ("fixture did not create address"), observed in
quality run 40863668428 on windows-2022 X64.

The script never changes the test policy, the 500 ms timeout, the assertion set, or the
concurrency of the original test binary. It does substitute the ORIGINAL C7 test file
with the diagnostic producer's instrumented copy of the same file (stderr capture and
buffered LSP message drain only on an already-failing wait_file), so this run is NOT
byte-identical-test qualification and never implies the original quality failure passed.
It only:

  * pins and hashes the immutable system under test (C7) and its own producer commit,
  * proves every producer byte except tests/bend_lint_protocol.rs matches C7, records the
    original and instrumented test hashes plus their unified diff, and installs the
    instrumented copy inside the throwaway C7 checkout before compiling,
  * starts a bounded observer in a separate PowerShell process,
  * runs the ORIGINAL test target exactly once with TMP/TEMP redirected to a dedicated
    tree inside the default temp parent,
  * preserves the original cargo exit status,
  * records process starts/stops, control-file publication, and file contents for the
    fixture's own control directory,
  * writes a bounded, machine-readable summary that always says the original quality
    verdict is unchanged.

Phases are driven by .github/workflows/windows-lint-startup-diagnostic.yml. There is no
retry loop, no test filter, and no re-run-to-pass path anywhere in this file.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('prepare-root', 'prepare', 'start', 'build', 'run', 'stop', 'finalize', 'collect')]
    [string]$Phase,

    [string]$Root,
    [string]$Sut,
    [string]$Producer,
    [string]$ScriptSha256,
    [string]$ExpectedTestSha256,
    [string]$ExpectedProducerRevision,

    [int]$ReadyTimeoutSeconds = 120,
    [int]$StopTimeoutSeconds = 90,
    [int]$CollectorDurationSeconds = 2400,
    [int]$LeaseSeconds = 1200,
    [int]$MaxRecords = 60000,
    [int]$MaxSnapshotRecords = 4000,
    [int]$MaxContentBytes = 4096,
    [int]$MaxCallbackRecords = 4000,
    [int]$MaxCallbackBytes = 8388608,
    [int]$MaxEventBytes = 67108864
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$script:ExpectedSutRevision = 'c7d0b2c2a9ee88a329433ef01ce496c3892f3985'
$script:TestName = 'timeout_kills_analyzer_and_its_descendant_without_blocking_compiler'
$script:ExpectedWindowsTests = 29
$script:BuildCommand = 'cargo test --locked --test bend_lint_protocol --no-run'
$script:RunCommand = 'cargo test --locked --test bend_lint_protocol'
$script:CriticalFiles = [ordered]@{
    'tests/bend_lint_protocol.rs'          = 'ae2b2b8225a95c809395faaff7dc1c188eb0281ba9119d27e3db4e24abb83dcb'
    'tests/fixtures/bend_lint_process.rs'  = '17477b2aefdb7479fffe2df47d0b5e8dd2f109d81f9a3de2118e59d5589a0f27'
    'tests/support/lsp_client.rs'          = 'a73a7cf5ce2ff0bb3e565af6b5e75a06245c76cf1a5b6aa93a810ce6ee5f1d1a'
    'tests/support/mod.rs'                 = '0a01f064c90e847af5dada685a0f94194ebcbefd843f633a1cf3d3ec6ba39dae'
}
# Control files the fixture and the test publish. Only these names may be read, and only
# from <temp>/<session>/control. Bodies are bounded by -MaxContentBytes.
$script:ContentAllowlist = @(
    'address', 'descendant-address', 'address.pending', 'descendant-address.pending',
    'calls', 'cwd', 'arguments', 'started', 'exit-code', 'response.json',
    'compiler.stderr', 'block', 'release', 'spawn-descendant', 'compiler-block',
    'compiler-release'
)
$script:ProbeDirName = '_collector-probe-dir'
$script:ProcessNamePattern = '^(bend2-lsp|bend lint|bend_lint_proto)'
$script:SubstitutedTestRelative = 'tests/bend_lint_protocol.rs'
$script:DiagnosticScriptRelative = 'scripts/windows-lint-startup-diagnostic.ps1'
$script:DiagnosticWorkflowRelative = '.github/workflows/windows-lint-startup-diagnostic.yml'
$script:ContentNamePattern = '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'
$script:SessionFileExtensions = @('.bend', '.json', '.log', '.txt', '.err', '.out', '.stderr', '.messages')
$script:Limits = @(
    'The tested server pipes the analyzer stderr to Stdio::null in the ORIGINAL C7 test. Only the diagnostic producer copy of the test may capture it, and this diagnostic never invents stderr that was not captured.',
    'This run substitutes the producer test file for tests/bend_lint_protocol.rs inside a throwaway C7 checkout. It is NOT byte-identical-test qualification: the assertions, cases, default concurrency and 500 ms timeout are unchanged, but the test file bytes are not.',
    'FileSystemWatcher notifications are best effort: an internal-buffer overflow produces one error record and blanket notification, so state is also re-diffed every 200 ms and re-read once at finalize.',
    'A process can still be invisible when it starts and exits between two observations. Win32_ProcessStartTrace/StopTrace are exact when the subscription is permitted; the 400 ms Get-Process diff is a cross-check with coarse timestamps and can miss a sub-500 ms fixture process entirely. Control-file publication is therefore the corroborating signal, not a substitute for the decisive instrumented LSP messages.',
    'Win32_ProcessStartTrace/StopTrace require an elevated Windows session; when the subscription fails the collector records the error and relies on the polling cross-check.',
    'Only the fixture image (bend lint fixture.exe) command line is recorded, bounded to 512 characters: its own arguments are part of the tested contract. Runner, cargo, and test-binary command lines are never recorded.',
    'No environment variables are dumped. Only TMP/TEMP, the diagnostic root, and BEND_NO_TELEMETRY are written to evidence.',
    'Content reads are bounded to files inside the dedicated temp tree: control files, direct session and project files with a small extension set, at most 40 entries per session and 4096 bytes each. Nothing outside that tree is read.',
    'File-watcher callback records are capped at MaxCallbackRecords records and MaxCallbackBytes payload bytes for the whole run. When the cap is reached, callbacks stop appending and the dropped record/byte counts plus the overflow flag are recorded in the collector summary, the analysis and missed_observations.',
    'Main-loop event records are capped by MaxRecords, the snapshot cap and a MaxEventBytes file-size guard; reaching either limit stops further records and is reported as a dropped count, a snapshot_limit_reached/event_byte_limit_reached flag and one lifecycle note.',
    'The instrumented test drains at most 32 buffered LSP messages and 16 KiB per control/stderr file after the original 10 s deadline; a relevant message beyond that prefix is not captured, and absence of a timeout message is not proof that cancellation never happened.',
    'Control-file content is read at observation and at finalize time; a read does not prove what the file contained at an earlier event timestamp. Defensive content reads are limited by the collector budget and the finalize scan.',
    'Observing costs CPU and one WMI query per fixture start; this can perturb timing and is recorded as an observer effect, never as a cause.',
    'The test runs with TMP/TEMP pointing at <default temp parent>\bend2-lint-startup-diagnostic-<run id>-<attempt>\temp instead of the default temp root itself. This fixed extra prefix lengthens temp paths and is the unavoidable cost of isolating evidence to one tree.',
    'This workflow is investigation-only. It cannot replace, supersede, or reinterpret the quality workflow result.'
)

# ---------------------------------------------------------------------------------------
# Small shared helpers
# ---------------------------------------------------------------------------------------

function Write-TextFile {
    param([string]$Path, [string]$Text)
    $directory = [IO.Path]::GetDirectoryName($Path)
    if ($directory -and -not [IO.Directory]::Exists($directory)) {
        [void][IO.Directory]::CreateDirectory($directory)
    }
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}

function Write-JsonFile {
    param([string]$Path, $Object)
    $json = $Object | ConvertTo-Json -Depth 14
    Write-TextFile -Path $Path -Text ($json + "`n")
}

function Read-JsonFile {
    param([string]$Path)
    if (-not [IO.File]::Exists($Path)) { return $null }
    try {
        return (Get-Content -LiteralPath $Path -Raw -ErrorAction Stop | ConvertFrom-Json -AsHashtable -ErrorAction Stop)
    } catch {
        return $null
    }
}

function Get-DiagSha256 {
    param([string]$Path)
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try {
        $hash = [Security.Cryptography.SHA256]::HashData($stream)
    } finally {
        $stream.Dispose()
    }
    return ([BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

function Get-StringSha256 {
    param([string]$Text)
    $bytes = [Text.Encoding]::UTF8.GetBytes($Text)
    return ([BitConverter]::ToString([Security.Cryptography.SHA256]::HashData($bytes)) -replace '-', '').ToLowerInvariant()
}

function Add-DiagLine {
    param([string]$Path, [string]$Line)
    $payload = $Line + "`n"
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        try {
            [IO.File]::AppendAllText($Path, $payload, [Text.UTF8Encoding]::new($false))
            return $true
        } catch {
            Start-Sleep -Milliseconds 5
        }
    }
    return $false
}

function Get-DiagPaths {
    param([string]$DiagnosticRoot)
    $root = [IO.Path]::GetFullPath($DiagnosticRoot)
    $temp = Join-Path $root 'temp'
    $observe = Join-Path $root 'observe'
    return [ordered]@{
        root            = $root
        temp            = $temp
        observe         = $observe
        logs            = Join-Path $root 'logs'
        events          = Join-Path $observe 'events.jsonl'
        ready           = Join-Path $observe 'collector-ready.json'
        summary         = Join-Path $observe 'collector-summary.json'
        lifecycle       = Join-Path $observe 'collector-lifecycle.json'
        readiness       = Join-Path $root 'collector-readiness.json'
        pid_file        = Join-Path $observe 'collector.pid'
        stop_file       = Join-Path $observe 'stop-requested'
        lease_file      = Join-Path $observe 'collector-lease'
        stdout_log      = Join-Path $observe 'collector.out.log'
        stderr_log      = Join-Path $observe 'collector.err.log'
        append_failures = Join-Path $observe 'append-failures.log'
        probe_observed  = Join-Path $observe 'probe-observed.txt'
        context         = Join-Path $root 'run-context.json'
        before          = Join-Path $root 'source-identity-before.json'
        after           = Join-Path $root 'source-identity-after.json'
        run_plan        = Join-Path $root 'run-plan.json'
        substitution    = Join-Path $root 'test-source-substitution.json'
        instrumented_diff = Join-Path $root 'instrumented-test.diff'
        build_json      = Join-Path $root 'build.json'
        run_start       = Join-Path $root 'run-start.json'
        run_exit        = Join-Path $root 'run-exit.json'
        diagnostics     = Join-Path $root 'diagnostic-summary.json'
        timelines       = Join-Path $root 'control-timelines.json'
        analyze_fails   = Join-Path $root 'analysis-failures.log'
        collector_identity = Join-Path $observe 'collector-identity.json'
        enforcement     = Join-Path $root 'identity-enforcement.json'
        bootstrap       = Join-Path $root 'bootstrap.json'
    }
}

function Get-NormalizedPath {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) { return $null }
    try {
        return [IO.Path]::GetFullPath($Path)
    } catch {
        return $null
    }
}

function Get-TempRelative {
    param([string]$Path, [string]$TempRoot)
    if ([string]::IsNullOrEmpty($Path) -or [string]::IsNullOrEmpty($TempRoot)) { return $null }
    $base = $TempRoot.TrimEnd([char]'\', [char]'/')
    if (-not $Path.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) { return $null }
    $rest = $Path.Substring($base.Length).TrimStart([char]'\', [char]'/')
    return ($rest -replace '\\', '/')
}

function Get-ControlRelative {
    param([string]$ControlPath, [string]$TempRoot)
    $relative = Get-TempRelative -Path $ControlPath -TempRoot $TempRoot
    if (-not $relative) { return $null }
    $relative = $relative.TrimEnd('/')
    if ($relative -notmatch '^[^/]+/control$') { return $null }
    return $relative
}

function ConvertTo-UtcDateTime {
    param([string]$Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return $null }
    try {
        $parsed = [DateTime]::Parse($Text, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind)
        return $parsed.ToUniversalTime()
    } catch {
        return $null
    }
}

# ConvertFrom-Json turns ISO timestamps into DateTime values, so every timestamp read from
# an event record is normalized back to one comparable ISO UTC representation.
function ConvertTo-IsoUtc {
    param($Value)
    if ($null -eq $Value) { return $null }
    if ($Value -is [DateTime]) { return $Value.ToUniversalTime().ToString('o') }
    if ($Value -is [DateTimeOffset]) { return $Value.UtcDateTime.ToString('o') }
    $text = [string]$Value
    if ([string]::IsNullOrWhiteSpace($text)) { return $null }
    $parsed = ConvertTo-UtcDateTime -Text $text
    if ($parsed) { return $parsed.ToString('o') }
    return $text
}

function Test-FocusControl {
    param($Files)
    $names = @($Files | Where-Object { $_.scope -eq 'control' } | ForEach-Object { $_.name })
    return (($names -contains 'spawn-descendant') -and ($names -contains 'compiler.stderr'))
}

# Parses a Windows command line the way the MSVC CRT and Rust's Command do, so quoted
# paths containing spaces survive. Bounded by the caller's 512-character record.
function Split-CommandLine {
    param([string]$CommandLine)
    $result = New-Object 'System.Collections.Generic.List[string]'
    if ([string]::IsNullOrEmpty($CommandLine)) { return $result }
    $current = New-Object Text.StringBuilder
    $backslash = [char]92
    $quote = [char]34
    $tab = [char]9
    $space = [char]32
    $inQuotes = $false
    $hasToken = $false
    $index = 0
    $length = $CommandLine.Length
    while ($index -lt $length) {
        $character = $CommandLine[$index]
        if ($character -eq $backslash) {
            $slashes = 0
            while ($index -lt $length -and $CommandLine[$index] -eq $backslash) {
                $slashes++
                $index++
            }
            if ($index -lt $length -and $CommandLine[$index] -eq $quote) {
                $pairs = [int][Math]::Floor($slashes / 2)
                if ($pairs -gt 0) { [void]$current.Append($backslash, $pairs) }
                if ($slashes % 2 -eq 0) { $inQuotes = -not $inQuotes } else { [void]$current.Append($quote) }
                $hasToken = $true
                $index++
            } else {
                [void]$current.Append($backslash, $slashes)
                $hasToken = $true
            }
            continue
        }
        if ($character -eq $quote) {
            $inQuotes = -not $inQuotes
            $hasToken = $true
            $index++
            continue
        }
        if (($character -eq $space -or $character -eq $tab) -and -not $inQuotes) {
            if ($hasToken -or $current.Length -gt 0) {
                $result.Add($current.ToString())
                [void]$current.Clear()
                $hasToken = $false
            }
            $index++
            continue
        }
        [void]$current.Append($character)
        $hasToken = $true
        $index++
    }
    if ($hasToken -or $current.Length -gt 0) { $result.Add($current.ToString()) }
    return $result
}

# Classifies one fixture command line. The fixture image serves four roles: the
# version/--help probes, the compiler (--check-only), the analyzer (--json), and the
# descendant (--descendant). Its arguments carry the session path, which maps the
# process back to <temp>/<session>/control without reading any environment.
function Get-FixtureInvocation {
    param([string]$CommandLine, [string]$TempRoot)
    $info = [ordered]@{
        role             = 'unknown'
        control_relative = $null
        argv_count       = 0
        note             = $null
    }
    $argv = Split-CommandLine -CommandLine $CommandLine
    $info.argv_count = $argv.Count
    if ($argv.Count -lt 2) {
        $info.role = 'probe-or-unknown'
        return $info
    }
    $argumentList = @($argv | Select-Object -Skip 1)
    if ($argumentList -contains '--descendant') {
        $info.role = 'descendant'
        $idx = [Array]::IndexOf($argumentList, '--descendant')
        if ($idx -ge 0 -and ($idx + 1) -lt $argumentList.Count) {
            $control = [IO.Path]::GetDirectoryName($argumentList[$idx + 1])
            $info.control_relative = Get-ControlRelative -ControlPath $control -TempRoot $TempRoot
        }
        return $info
    }
    if ($argumentList -contains '--check-only') { $info.role = 'compiler' }
    elseif ($argumentList -contains '--json') { $info.role = 'lint' }
    else { $info.role = 'probe' }
    foreach ($argument in $argumentList) {
        $relative = Get-TempRelative -Path $argument -TempRoot $TempRoot
        if (-not $relative) { continue }
        $parts = $relative -split '/'
        if ($parts.Count -ge 2 -and $parts[0] -ne $script:ProbeDirName) {
            $info.control_relative = "$($parts[0])/control"
        }
        break
    }
    return $info
}

function Read-DiagContent {
    param([string]$Path, [int]$MaxBytes)
    $result = [ordered]@{ exists = $false; bytes = $null; text = $null; truncated = $false; error = $null }
    try {
        $info = [IO.FileInfo]::new($Path)
        if (-not $info.Exists) { return $result }
        $result.exists = $true
        $result.bytes = $info.Length
        if ($info.Length -gt $MaxBytes) {
            $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
            try {
                $buffer = New-Object byte[] $MaxBytes
                $read = $stream.Read($buffer, 0, $MaxBytes)
            } finally {
                $stream.Dispose()
            }
            $result.text = [Text.Encoding]::UTF8.GetString($buffer, 0, $read)
            $result.truncated = $true
        } else {
            $result.text = [IO.File]::ReadAllText($Path)
        }
    } catch {
        $result.error = $_.Exception.Message
    }
    return $result
}

function Get-DiagStderrTail {
    param([string]$Path, [int]$Lines = 40)
    if (-not $Path -or -not [IO.File]::Exists($Path)) { return $null }
    try {
        $tail = Get-Content -LiteralPath $Path -Tail $Lines -ErrorAction SilentlyContinue
        return ((@($tail) | ForEach-Object { [string]$_ }) -join "`n")
    } catch {
        return $null
    }
}

function Get-GitIdentity {
    param([string]$Repository, [string[]]$GitArgs)
    $output = & git -C $Repository @GitArgs 2>&1
    $code = $LASTEXITCODE
    return [ordered]@{
        exit_code = $code
        output    = ((@($output) | ForEach-Object { [string]$_ }) -join "`n").Trim()
    }
}

# ---------------------------------------------------------------------------------------
# Identity recording (before / after)
# ---------------------------------------------------------------------------------------

# Complete tracked-file inventory of one checkout. Every git-tracked path is hashed, so
# production inputs outside src/tests (vendor, benches, tools, .cargo, workflows) are part
# of the identity. The substituted test file and this diagnostic's own new files are
# excluded from the combined digest and reported separately.
function Get-TrackedInventory {
    param([string]$Repository, [string[]]$ExcludeRelative)
    $output = & git -C $Repository ls-files -z 2>$null
    $code = $LASTEXITCODE
    $paths = @()
    if ($code -eq 0 -and $output) {
        $paths = @(($output -join "`n") -split "`0" | Where-Object { $_ })
    }
    $entries = New-Object 'System.Collections.Generic.List[object]'
    $lines = New-Object 'System.Collections.Generic.List[string]'
    $missing = New-Object 'System.Collections.Generic.List[string]'
    $coveredBytes = [long]0
    $excludedIdentity = $null
    foreach ($relative in ($paths | Sort-Object)) {
        if ($ExcludeRelative -contains $relative) { continue }
        $absolute = Join-Path $Repository $relative
        if (-not [IO.File]::Exists($absolute)) {
            $missing.Add($relative)
            $lines.Add(('MISSING  {0}' -f $relative))
            continue
        }
        $info = [IO.FileInfo]::new($absolute)
        $sha = Get-DiagSha256 -Path $absolute
        $entry = [ordered]@{ path = $relative; bytes = $info.Length; sha256 = $sha }
        $entries.Add($entry)
        $coveredBytes += $info.Length
        $lines.Add(('{0}  {1}' -f $sha, $relative))
    }
    $testPath = Join-Path $Repository $script:SubstitutedTestRelative
    if ([IO.File]::Exists($testPath)) {
        $excludedIdentity = [ordered]@{
            path   = $script:SubstitutedTestRelative
            bytes  = ([IO.FileInfo]::new($testPath)).Length
            sha256 = (Get-DiagSha256 -Path $testPath)
        }
    }
    return [ordered]@{
        algorithm        = 'sha256'
        combined         = (Get-StringSha256 -Text ($lines -join "`n"))
        inventory_sha256 = (Get-StringSha256 -Text ((@($entries | ForEach-Object { $_.path }) | Sort-Object) -join "`n"))
        tracked_count    = $paths.Count
        covered_count    = $entries.Count
        missing_count    = $missing.Count
        missing_paths    = @($missing | Select-Object -First 20)
        covered_bytes    = $coveredBytes
        excluded_paths   = @($ExcludeRelative)
        excluded_identity = $excludedIdentity
        git_exit_code    = $code
        files            = $entries
    }
}

function Get-SourceIdentity {
    param([string]$Repository, [string]$ScriptPath, [string]$ProducerRepository)
    $identity = [ordered]@{
        schema            = 1
        generated_utc     = [DateTime]::UtcNow.ToString('o')
        repository        = $Repository
        head              = $null
        branch            = $null
        status_porcelain  = $null
        blob_identities   = [ordered]@{}
        digest            = $null
        instrumented_test = $null
        modified_tracked_paths = @()
        critical_files    = [ordered]@{}
        rustc             = $null
        cargo             = $null
        target_pre_existing = [IO.Directory]::Exists((Join-Path $Repository 'target'))
        cargo_incremental = $env:CARGO_INCREMENTAL
        rustc_wrapper     = $env:RUSTC_WRAPPER
    }
    $head = Get-GitIdentity -Repository $Repository -GitArgs @('rev-parse', 'HEAD')
    $identity.head = $head.output
    $branch = Get-GitIdentity -Repository $Repository -GitArgs @('rev-parse', '--abbrev-ref', 'HEAD')
    $identity.branch = $branch.output
    $status = Get-GitIdentity -Repository $Repository -GitArgs @('status', '--porcelain=v1', '--untracked-files=no')
    $identity.status_porcelain = $status.output
    foreach ($relative in $script:CriticalFiles.Keys) {
        $result = Get-GitIdentity -Repository $Repository -GitArgs @('ls-files', '-s', '--', $relative)
        $blob = $null
        if ($result.exit_code -eq 0 -and $result.output) {
            $fields = $result.output -split '\s+'
            if ($fields.Count -ge 2) { $blob = $fields[1] }
        }
        $identity.blob_identities[$relative] = $blob
        $absolute = Join-Path $Repository $relative
        $digest = $null
        if ([IO.File]::Exists($absolute)) { $digest = Get-DiagSha256 -Path $absolute }
        $expected = $script:CriticalFiles[$relative]
        $identity.critical_files[$relative] = [ordered]@{
            sha256           = $digest
            expected_sha256  = $expected
            matches_expected = ($digest -eq $expected)
        }
    }
    $identity.digest = Get-TrackedInventory -Repository $Repository -ExcludeRelative @(
        $script:SubstitutedTestRelative,
        $script:DiagnosticScriptRelative,
        $script:DiagnosticWorkflowRelative
    )
    $identity.instrumented_test = $identity.digest.excluded_identity
    $modified = @()
    foreach ($statusLine in ($identity.status_porcelain -split "`n")) {
        if ($statusLine -match '^\s*(?<code>\S{1,2})\s+(?<path>.+)$') {
            $modified += ($Matches['path'] -replace '\\', '/')
        }
    }
    $identity.modified_tracked_paths = $modified
    Push-Location $Repository
    try {
        $identity.rustc = ((& rustc -vV 2>&1) | ForEach-Object { [string]$_ }) -join "`n"
        $identity.cargo = ((& cargo -V 2>&1) | ForEach-Object { [string]$_ }) -join "`n"
    } catch {
        $identity.rustc = $null
    } finally {
        Pop-Location
    }
    if ($ProducerRepository) {
        $producerHead = Get-GitIdentity -Repository $ProducerRepository -GitArgs @('rev-parse', 'HEAD')
        $producerStatus = Get-GitIdentity -Repository $ProducerRepository -GitArgs @('status', '--porcelain=v1', '--untracked-files=no')
        $producerDigest = Get-TrackedInventory -Repository $ProducerRepository -ExcludeRelative @(
            $script:SubstitutedTestRelative,
            $script:DiagnosticScriptRelative,
            $script:DiagnosticWorkflowRelative
        )
        $producerCritical = [ordered]@{}
        foreach ($relative in $script:CriticalFiles.Keys) {
            $absolute = Join-Path $ProducerRepository $relative
            $digest = $null
            if ([IO.File]::Exists($absolute)) { $digest = Get-DiagSha256 -Path $absolute }
            $producerCritical[$relative] = [ordered]@{
                sha256           = $digest
                expected_sha256  = $script:CriticalFiles[$relative]
                matches_expected = ($digest -eq $script:CriticalFiles[$relative])
            }
        }
        $scriptHash = Get-DiagSha256 -Path $ScriptPath
        $identity['producer'] = [ordered]@{
            repository       = $ProducerRepository
            head             = $producerHead.output
            status_porcelain = $producerStatus.output
            digest           = $producerDigest
            critical_files   = $producerCritical
            instrumented_test = $producerDigest.excluded_identity
            test_expected     = $ExpectedTestSha256
            test_matches      = ($producerDigest.excluded_identity -and $producerDigest.excluded_identity.sha256 -eq $ExpectedTestSha256.ToLowerInvariant())
            script_path      = $ScriptPath
            script_sha256    = $scriptHash
            script_expected  = $ScriptSha256
            script_matches   = ($scriptHash -eq $ScriptSha256.ToLowerInvariant())
        }
        $workflow = Join-Path $ProducerRepository $script:DiagnosticWorkflowRelative
        $identity.producer['workflow_file'] = [ordered]@{
            path   = $script:DiagnosticWorkflowRelative
            sha256 = if ([IO.File]::Exists($workflow)) { Get-DiagSha256 -Path $workflow } else { $null }
        }
    }
    return $identity
}

function Compare-Identity {
    param($Before, $After, [string]$ExpectedInstrumentedSha256, [string]$OriginalTestSha256)
    if (-not $Before -or -not $After) {
        return [ordered]@{
            comparable = $false; unchanged = $null
            digest_before = $null; digest_after = $null
            head_before = $null; head_after = $null
            status_before = $null; status_after = $null
            other_tracked_modifications = @()
            instrumented_test_sha256 = $null
            instrumented_test_expected = $ExpectedInstrumentedSha256
            production_unchanged = $null
        }
    }
    $other = @()
    foreach ($path in @($After.modified_tracked_paths)) {
        if ($path -and $path -ne $script:SubstitutedTestRelative) { $other += $path }
    }
    $beforeModified = @($Before.modified_tracked_paths)
    $productionUnchanged = ($Before.digest.combined -eq $After.digest.combined)
    $countsMatch = ($Before.digest.covered_count -eq $After.digest.covered_count)
    $inventoryMatch = ($Before.digest.inventory_sha256 -eq $After.digest.inventory_sha256)
    $headUnchanged = ($Before.head -eq $After.head)
    $instrumentedSha = $null
    if ($After.instrumented_test) { $instrumentedSha = $After.instrumented_test.sha256 }
    $instrumentedIntact = $true
    if ($ExpectedInstrumentedSha256) { $instrumentedIntact = ($instrumentedSha -eq $ExpectedInstrumentedSha256) }
    $differsFromOriginal = $null
    if ($OriginalTestSha256 -and $instrumentedSha) { $differsFromOriginal = ($instrumentedSha -ne $OriginalTestSha256) }
    return [ordered]@{
        comparable                    = $true
        head_before                   = $Before.head
        head_after                    = $After.head
        head_unchanged                = $headUnchanged
        digest_before                 = $Before.digest.combined
        digest_after                  = $After.digest.combined
        production_unchanged          = $productionUnchanged
        tracked_counts_before         = $Before.digest.covered_count
        tracked_counts_after          = $After.digest.covered_count
        tracked_counts_match          = $countsMatch
        inventory_sha256_before       = $Before.digest.inventory_sha256
        inventory_sha256_after        = $After.digest.inventory_sha256
        inventory_match               = $inventoryMatch
        digest_excluded_paths         = $Before.digest.excluded_paths
        status_before                 = $Before.status_porcelain
        status_after                  = $After.status_porcelain
        modified_tracked_paths        = @($After.modified_tracked_paths)
        other_tracked_modifications   = $other
        only_allowed_modification     = ($beforeModified.Count -eq 0 -and $other.Count -eq 0)
        instrumented_test_sha256      = $instrumentedSha
        instrumented_test_expected    = $ExpectedInstrumentedSha256
        instrumented_test_intact      = $instrumentedIntact
        instrumented_differs_from_original = $differsFromOriginal
        unchanged                     = ($productionUnchanged -and $countsMatch -and $inventoryMatch -and $headUnchanged -and ($other.Count -eq 0) -and $instrumentedIntact)
    }
}

# ---------------------------------------------------------------------------------------
# prepare-root
# ---------------------------------------------------------------------------------------

function Invoke-PrepareRoot {
    $base = $null
    if ($Root) {
        $base = $Root
    } else {
        $runId = if ($env:GITHUB_RUN_ID) { $env:GITHUB_RUN_ID } else { [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') }
        $attempt = if ($env:GITHUB_RUN_ATTEMPT) { $env:GITHUB_RUN_ATTEMPT } else { '0' }
        $defaultParent = [IO.Path]::GetTempPath()
        $base = Join-Path $defaultParent ("bend2-lint-startup-diagnostic-{0}-{1}" -f $runId, $attempt)
    }
    $root = [IO.Path]::GetFullPath($base)
    $paths = Get-DiagPaths -DiagnosticRoot $root
    if ([IO.Directory]::Exists($root)) {
        # The workflow creates the root and its own bootstrap envelope before any producer
        # code runs, so an existing root is accepted only when that envelope is present.
        if (-not [IO.File]::Exists($paths.bootstrap)) {
            throw "diagnostic root already exists without a workflow bootstrap envelope: $root"
        }
    }
    foreach ($name in @('temp', 'observe', 'logs')) {
        [void][IO.Directory]::CreateDirectory((Join-Path $root $name))
    }
    $context = [ordered]@{
        schema             = 1
        kind               = 'windows-lint-startup-diagnostic'
        diagnostic_only    = $true
        generated_utc      = [DateTime]::UtcNow.ToString('o')
        root               = $paths.root
        temp_root_for_test = $paths.temp
        default_temp_parent = [IO.Path]::GetTempPath()
        runner             = [ordered]@{
            os                 = $env:RUNNER_OS
            arch               = $env:RUNNER_ARCH
            name               = $env:RUNNER_NAME
            image_os           = $env:ImageOS
            image_version      = $env:ImageVersion
        }
        github             = [ordered]@{
            repository   = $env:GITHUB_REPOSITORY
            workflow     = $env:GITHUB_WORKFLOW
            workflow_ref = $env:GITHUB_WORKFLOW_REF
            run_id       = $env:GITHUB_RUN_ID
            run_attempt  = $env:GITHUB_RUN_ATTEMPT
            event_name   = $env:GITHUB_EVENT_NAME
            sha          = $env:GITHUB_SHA
            ref          = $env:GITHUB_REF
        }
        powershell         = [ordered]@{
            version = $PSVersionTable.PSVersion.ToString()
            edition = $PSVersionTable.PSEdition
            script  = $PSCommandPath
            sha256  = (Get-DiagSha256 -Path $PSCommandPath)
        }
        parameters         = [ordered]@{
            phase                    = $Phase
            ready_timeout_seconds    = $ReadyTimeoutSeconds
            stop_timeout_seconds     = $StopTimeoutSeconds
            collector_duration_seconds = $CollectorDurationSeconds
            lease_seconds            = $LeaseSeconds
            max_records              = $MaxRecords
            max_snapshot_records     = $MaxSnapshotRecords
            max_content_bytes        = $MaxContentBytes
            max_callback_records     = $MaxCallbackRecords
            max_callback_bytes       = $MaxCallbackBytes
            max_event_bytes          = $MaxEventBytes
        }
        limits             = $script:Limits
        statement          = 'Investigation-only diagnostic. It never replaces the quality workflow result and never reruns the test to obtain a pass.'
    }
    Write-JsonFile -Path $paths.context -Object $context
    Write-Host ("diagnostic root: {0}" -f $paths.root)
}

# ---------------------------------------------------------------------------------------
# Test-source substitution: original C7 bytes vs the diagnostic producer copy
# ---------------------------------------------------------------------------------------

function Get-TestSourceFacts {
    param([string]$Text, [string]$Path)
    $attributes = 0
    $cfgGated = 0
    $pendingCfg = $false
    $ignore = $false
    foreach ($line in ($Text -split "`n")) {
        if ($line -match '^#\[cfg\(') { $pendingCfg = $true; continue }
        if ($line -match '^#\[test\]\s*$') {
            $attributes++
            if ($pendingCfg) { $cfgGated++ }
            $pendingCfg = $false
            continue
        }
        if ($line -match '#\[ignore') { $ignore = $true }
        if ($line -notmatch '^\s*(//|$)') { $pendingCfg = $false }
    }
    return [ordered]@{
        path                     = $Path
        bytes                    = [Text.Encoding]::UTF8.GetByteCount($Text)
        sha256                   = (Get-DiagSha256 -Path $Path)
        test_attributes          = $attributes
        cfg_gated                = $cfgGated
        expected_windows_tests   = ($attributes - $cfgGated)
        has_named_timeout_test   = $Text.Contains('fn ' + $script:TestName + '(')
        has_500ms_configuration  = $Text.Contains('configure(true, false, 500);')
        has_ignore_attribute     = $ignore
        has_test_threads_override = ($Text -match 'test-threads')
    }
}

function Get-UnifiedDiff {
    param([string]$LeftPath, [string]$RightPath, [int]$MaxBytes = 131072)
    $output = & git diff --no-index --unified=3 --no-color -- $LeftPath $RightPath 2>&1
    $code = $LASTEXITCODE
    $text = ((@($output) | ForEach-Object { [string]$_ }) -join "`n")
    $added = 0
    $removed = 0
    foreach ($line in ($text -split "`n")) {
        if ($line.StartsWith('+++') -or $line.StartsWith('---')) { continue }
        if ($line.StartsWith('+')) { $added++ } elseif ($line.StartsWith('-')) { $removed++ }
    }
    $truncated = $false
    $fullLength = $text.Length
    if ($text.Length -gt $MaxBytes) {
        $text = $text.Substring(0, $MaxBytes)
        $truncated = $true
    }
    return [ordered]@{
        git_exit_code = $code
        added_lines   = $added
        removed_lines = $removed
        full_length   = $fullLength
        truncated     = $truncated
        sha256        = (Get-StringSha256 -Text $text)
        text          = $text
    }
}

function Get-TestSourceSubstitution {
    param([string]$SutPath, [string]$ProducerPath, $Paths)
    $originalPath = Join-Path $SutPath $script:SubstitutedTestRelative
    $instrumentedPath = Join-Path $ProducerPath $script:SubstitutedTestRelative
    if (-not [IO.File]::Exists($originalPath)) { throw "original C7 test file is missing: $originalPath" }
    if (-not [IO.File]::Exists($instrumentedPath)) { throw "producer test file is missing: $instrumentedPath" }
    $originalFacts = Get-TestSourceFacts -Text ([IO.File]::ReadAllText($originalPath)) -Path $originalPath
    $instrumentedFacts = Get-TestSourceFacts -Text ([IO.File]::ReadAllText($instrumentedPath)) -Path $instrumentedPath
    $diff = Get-UnifiedDiff -LeftPath $originalPath -RightPath $instrumentedPath
    Write-TextFile -Path $Paths.instrumented_diff -Text ($diff.text + "`n")
    $changed = ($originalFacts.sha256 -ne $instrumentedFacts.sha256)

    # These invariants are the test contract: 29 Windows cases, the named case, the 500 ms
    # configuration, default concurrency, and no ignored cases. Instrumentation may add
    # capture code, never a case, an ignore, or a thread-count override.
    $violations = @()
    if ($instrumentedFacts.expected_windows_tests -ne $script:ExpectedWindowsTests) {
        $violations += ("expected {0} Windows tests, found {1}" -f $script:ExpectedWindowsTests, $instrumentedFacts.expected_windows_tests)
    }
    if ($instrumentedFacts.test_attributes -ne $originalFacts.test_attributes) {
        $violations += ("test attribute count changed from {0} to {1}" -f $originalFacts.test_attributes, $instrumentedFacts.test_attributes)
    }
    if ($instrumentedFacts.cfg_gated -ne $originalFacts.cfg_gated) {
        $violations += ("cfg-gated test count changed from {0} to {1}" -f $originalFacts.cfg_gated, $instrumentedFacts.cfg_gated)
    }
    if (-not $instrumentedFacts.has_named_timeout_test) { $violations += ("named test is missing: {0}" -f $script:TestName) }
    if (-not $instrumentedFacts.has_500ms_configuration) { $violations += 'the 500 ms lint configuration line is missing or changed' }
    if ($instrumentedFacts.has_ignore_attribute) { $violations += 'an #[ignore] attribute is present' }
    if ($instrumentedFacts.has_test_threads_override) { $violations += 'a test-threads override is present' }
    if ($violations.Count -gt 0) {
        throw ("instrumented test violates the diagnosed contract: {0}" -f ($violations -join '; '))
    }

    $installedUtc = [DateTime]::UtcNow.ToString('o')
    [IO.File]::Copy($instrumentedPath, $originalPath, $true)
    $copiedSha = Get-DiagSha256 -Path $originalPath

    return [ordered]@{
        schema          = 1
        diagnostic_only = $true
        statement       = 'The instrumented producer copy of the test replaced the C7 test file inside a throwaway checkout. This is not byte-identical-test qualification; the cases, assertions, default concurrency and 500 ms timeout are unchanged.'
        changed         = $changed
        original        = $originalFacts
        instrumented    = $instrumentedFacts
        diff            = [ordered]@{
            file          = 'instrumented-test.diff'
            added_lines   = $diff.added_lines
            removed_lines = $diff.removed_lines
            full_length   = $diff.full_length
            truncated     = $diff.truncated
            sha256        = $diff.sha256
            git_exit_code = $diff.git_exit_code
        }
        installed_utc   = $installedUtc
        target_path     = (Join-Path $SutPath $script:SubstitutedTestRelative)
        verification    = [ordered]@{
            copied_sha256            = $copiedSha
            matches_instrumented     = ($copiedSha -eq $instrumentedFacts.sha256)
            differs_from_original    = ($copiedSha -ne $originalFacts.sha256)
        }
        contract = [ordered]@{
            expected_windows_tests = $script:ExpectedWindowsTests
            timeout_ms             = 500
            concurrency_override   = $false
            ignore_attributes      = $false
            violations             = $violations
        }
    }
}

function Invoke-Prepare {
    if (-not $Root) { throw '-Root is required' }
    if (-not $Sut) { throw '-Sut is required' }
    if (-not $Producer) { throw '-Producer is required: the instrumented test source lives in the producer checkout' }
    if (-not $ScriptSha256) { throw '-ScriptSha256 is required: the producer script must be pinned by digest' }
    if (-not $ExpectedTestSha256) { throw '-ExpectedTestSha256 is required: the instrumented test must be pinned by digest' }
    if ($ScriptSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw '-ScriptSha256 must be a 64-character hex SHA-256' }
    if ($ExpectedTestSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw '-ExpectedTestSha256 must be a 64-character hex SHA-256' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    $sutPath = Get-NormalizedPath -Path $Sut
    $producerPath = Get-NormalizedPath -Path $Producer
    foreach ($repository in @($sutPath, $producerPath)) {
        if (-not $repository -or -not [IO.Directory]::Exists((Join-Path $repository '.git'))) {
            throw "checkout is not a git repository: $repository"
        }
    }
    $before = Get-SourceIdentity -Repository $sutPath -ScriptPath $PSCommandPath -ProducerRepository $producerPath

    if ($before.head -ne $script:ExpectedSutRevision) {
        throw ("SUT revision mismatch: expected {0}, found {1}" -f $script:ExpectedSutRevision, $before.head)
    }
    if ($before.status_porcelain) {
        throw ("SUT working tree is not clean before the run: {0}" -f $before.status_porcelain)
    }
    foreach ($relative in $script:CriticalFiles.Keys) {
        $entry = $before.critical_files[$relative]
        if (-not $entry.matches_expected) {
            throw ("original C7 file digest mismatch for {0}: expected {1}, found {2}" -f $relative, $entry.expected_sha256, $entry.sha256)
        }
    }
    $expectedProducerCommit = $ExpectedProducerRevision
    if ($ExpectedProducerRevision) {
        foreach ($candidate in @($ExpectedProducerRevision, "origin/$ExpectedProducerRevision", "refs/remotes/origin/$ExpectedProducerRevision")) {
            $resolved = Get-GitIdentity -Repository $producerPath -GitArgs @('rev-parse', '--verify', "$candidate^{commit}")
            if ($resolved.exit_code -eq 0 -and $resolved.output) {
                $expectedProducerCommit = $resolved.output
                break
            }
        }
    }
    if (-not $before.producer.head -or $before.producer.head -ne $expectedProducerCommit) {
        throw ("producer revision mismatch: expected {0} (resolved {1}), found {2}" -f $ExpectedProducerRevision, $expectedProducerCommit, $before.producer.head)
    }
    $before.producer['expected_revision'] = $ExpectedProducerRevision
    $before.producer['expected_commit'] = $expectedProducerCommit
    if (-not $before.producer.script_matches) {
        throw ("producer script digest mismatch: expected {0}, found {1}" -f $ScriptSha256, $before.producer.script_sha256)
    }
    if (-not $before.producer.test_matches) {
        throw ("instrumented test digest mismatch: expected {0}, found {1}" -f $ExpectedTestSha256, $before.producer.instrumented_test.sha256)
    }
    if ($before.producer.digest.covered_count -le 0 -or $before.producer.digest.combined -notmatch '^[0-9a-f]{64}$') {
        throw 'producer tracked inventory is empty or has no digest'
    }
    if ($before.digest.covered_count -le 0 -or $before.digest.combined -notmatch '^[0-9a-f]{64}$') {
        throw 'C7 tracked inventory is empty or has no digest'
    }
    if ($before.producer.status_porcelain) {
        throw ("producer working tree is not clean: {0}" -f $before.producer.status_porcelain)
    }
    # Production proof: every producer byte except the substituted test file must equal C7.
    if ($before.producer.digest.combined -ne $before.digest.combined) {
        throw ("producer bytes differ from C7 outside {0}: producer {1}, C7 {2}" -f $script:SubstitutedTestRelative, $before.producer.digest.combined, $before.digest.combined)
    }
    if ($before.target_pre_existing) {
        throw 'the SUT target directory already exists: this diagnostic requires a cold target directory (no object cache)'
    }
    if ($before.cargo_incremental -or $before.rustc_wrapper) {
        throw 'CARGO_INCREMENTAL or RUSTC_WRAPPER is set: refusing to observe an unaudited build configuration'
    }

    $substitution = Get-TestSourceSubstitution -SutPath $sutPath -ProducerPath $producerPath -Paths $paths
    if (-not $substitution.verification.matches_instrumented) {
        throw ("substitution failed: {0} is {1}, expected {2}" -f $script:SubstitutedTestRelative, $substitution.verification.copied_sha256, $substitution.instrumented.sha256)
    }
    $postDigest = Get-TrackedInventory -Repository $sutPath -ExcludeRelative @(
        $script:SubstitutedTestRelative,
        $script:DiagnosticScriptRelative,
        $script:DiagnosticWorkflowRelative
    )
    if ($postDigest.combined -ne $before.digest.combined -or $postDigest.covered_count -ne $before.digest.covered_count) {
        throw 'substituting the test file changed the non-test tracked inventory; refusing to build'
    }
    $substitution['post_substitution_digest'] = $postDigest.combined
    $substitution['production_digest'] = $before.digest.combined
    $substitution['production_digest_matches_c7'] = ($before.producer.digest.combined -eq $before.digest.combined)
    $substitution['expected_test_sha256'] = $ExpectedTestSha256.ToLowerInvariant()

    Write-JsonFile -Path $paths.before -Object $before
    Write-JsonFile -Path $paths.substitution -Object $substitution
    $plan = [ordered]@{
        schema            = 1
        diagnostic_only   = $true
        sut_revision      = $script:ExpectedSutRevision
        producer_revision = $before.producer.head
        script_sha256     = $before.producer.script_sha256
        build_command     = $script:BuildCommand
        run_command       = $script:RunCommand
        working_directory = $sutPath
        environment_overrides = [ordered]@{
            TMP                = $paths.temp
            TEMP               = $paths.temp
            BEND_NO_TELEMETRY  = $env:BEND_NO_TELEMETRY
        }
        test_source = [ordered]@{
            mode             = if ($substitution.changed) { 'instrumented-producer-copy' } else { 'original-c7' }
            original_sha256  = $substitution.original.sha256
            instrumented_sha256 = $substitution.instrumented.sha256
            changed          = $substitution.changed
            diff             = $substitution.diff
            installed_utc    = $substitution.installed_utc
            test_attributes  = $substitution.instrumented.test_attributes
            cfg_gated        = $substitution.instrumented.cfg_gated
            expected_windows_tests = $script:ExpectedWindowsTests
            contract         = $substitution.contract
        }
        byte_identical_test_qualification = $false
        test_timeout_ms_untouched = 500
        concurrency_untouched     = $true
        assertions_untouched      = $true
        retries                   = 0
        rerun_to_pass             = $false
        notes = @(
            'The original test target runs exactly once. There is no retry, no filter, and no --test-threads override.',
            'The build step only prebuilds; it is the same cargo target and the run step executes the identical cargo command as quality.yml.',
            'The test file is the diagnostic producer copy. Assertions, cases, default concurrency and the 500 ms timeout are unchanged, but this run is not byte-identical-test qualification.',
            'The original cargo exit code is preserved by -Phase run and recorded in run-exit.json.'
        )
    }
    Write-JsonFile -Path $paths.run_plan -Object $plan
    Write-Host ("SUT {0} clean; producer {1}; test source {2} ({3} -> {4})" -f $before.head, $before.producer.head, $plan.test_source.mode, $substitution.original.sha256.Substring(0, 12), $substitution.instrumented.sha256.Substring(0, 12))
}

# ---------------------------------------------------------------------------------------
# Collector helpers (run inside the observer process)
# ---------------------------------------------------------------------------------------

function Initialize-CollectorState {
    param($Paths)
    $state = [ordered]@{
        paths                  = $Paths
        qpc_freq               = [Diagnostics.Stopwatch]::Frequency
        written                = 0
        dropped                = 0
        snapshot_records       = 0
        snapshot_limit_reached = $false
        snapshot_signatures    = @{}
        byte_limit_reached     = $false
        cim_filtered           = 0
        cim_start_events       = 0
        cim_stop_events        = 0
        cmdline_queries        = 0
        cmdline_failures       = 0
        poll_samples           = 0
        poll_starts            = 0
        poll_stops             = 0
        poll_seen              = @{}
        error_records          = 0
    }
    return $state
}

function Write-DiagRecord {
    param($State, [hashtable]$Record)
    if ($State.written -ge $MaxRecords -or $State.byte_limit_reached) {
        $State.dropped++
        return
    }
    if ($State.written -gt 0 -and ($State.written % 100 -eq 0)) {
        try {
            if (([IO.FileInfo]::new($State.paths.events)).Length -gt $MaxEventBytes) {
                $State.byte_limit_reached = $true
                $note = [ordered]@{
                    schema = 1
                    qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
                    freq   = $State.qpc_freq
                    ts_utc = [DateTime]::UtcNow.ToString('o')
                    pid    = $PID
                    source = 'lifecycle'
                    note   = 'event-byte-limit-reached'
                    detail = ("events file exceeded {0} bytes" -f $MaxEventBytes)
                }
                Add-DiagLine -Path $State.paths.events -Line ($note | ConvertTo-Json -Compress -Depth 4) | Out-Null
                $State.dropped++
                return
            }
        } catch {
        }
    }
    $line = [ordered]@{
        schema = 1
        qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
        freq   = $State.qpc_freq
        ts_utc = [DateTime]::UtcNow.ToString('o')
        pid    = $PID
    }
    foreach ($key in $Record.Keys) { $line[$key] = $Record[$key] }
    if (Add-DiagLine -Path $State.paths.events -Line ($line | ConvertTo-Json -Compress -Depth 6)) {
        $State.written++
    } else {
        $State.dropped++
    }
}

function Test-ContentEligibleName {
    param([string]$Name)
    if (-not $Name) { return $false }
    if ($script:ContentAllowlist -contains $Name) { return $true }
    return ($Name -match $script:ContentNamePattern)
}

# Bounded scan of one session tempdir: control files first, then direct session files and
# one project level with a small extension set. Nothing outside the dedicated temp tree is
# considered, and at most 40 entries are returned per session.
function Get-SessionScanEntries {
    param([string]$BaseDir, [int]$MaxEntries = 40)
    $entries = New-Object 'System.Collections.Generic.List[object]'
    $control = [IO.Path]::Combine($BaseDir, 'control')
    if ([IO.Directory]::Exists($control)) {
        try {
            foreach ($file in ([IO.Directory]::GetFiles($control) | Sort-Object)) {
                if ($entries.Count -ge $MaxEntries) { break }
                $name = [IO.Path]::GetFileName($file)
                if (Test-ContentEligibleName -Name $name) {
                    $entries.Add([ordered]@{ scope = 'control'; name = $name; path = $file })
                } elseif ($name -match '^(bend lint fixture.*\.exe|bend2-lsp.*\.exe)$') {
                    $entries.Add([ordered]@{ scope = 'control-image'; name = $name; path = $file })
                }
            }
        } catch {
        }
    }
    $directories = @($BaseDir) + @([IO.Directory]::GetDirectories($BaseDir))
    foreach ($directory in $directories) {
        if ($entries.Count -ge $MaxEntries) { break }
        if ([IO.Path]::GetFileName($directory) -eq 'control') { continue }
        try {
            $files = [IO.Directory]::GetFiles($directory) | Sort-Object
        } catch {
            continue
        }
        foreach ($file in $files) {
            if ($entries.Count -ge $MaxEntries) { break }
            $name = [IO.Path]::GetFileName($file)
            $extension = [IO.Path]::GetExtension($name).ToLowerInvariant()
            if (-not ($script:SessionFileExtensions -contains $extension)) { continue }
            if (-not (Test-ContentEligibleName -Name $name)) { continue }
            $scope = if ($directory -eq $BaseDir) { 'session' } else { 'project' }
            $entries.Add([ordered]@{ scope = $scope; name = $name; path = $file })
        }
    }
    return $entries
}

function Invoke-ControlSnapshot {
    param($State, [switch]$Force)
    if ($State.snapshot_limit_reached) { return }
    $tempRoot = $State.paths.temp
    if (-not [IO.Directory]::Exists($tempRoot)) { return }
    foreach ($baseDir in ([IO.Directory]::GetDirectories($tempRoot) | Sort-Object)) {
        $baseName = [IO.Path]::GetFileName($baseDir)
        $entries = Get-SessionScanEntries -BaseDir $baseDir
        if ($entries.Count -eq 0) { continue }
        $signatureParts = New-Object 'System.Collections.Generic.List[string]'
        $observed = New-Object 'System.Collections.Generic.List[object]'
        foreach ($entry in $entries) {
            $bytes = $null
            $lastWrite = $null
            try {
                $info = [IO.FileInfo]::new($entry.path)
                if ($info.Exists) {
                    $bytes = $info.Length
                    $lastWrite = $info.LastWriteTimeUtc.ToString('o')
                }
            } catch {
            }
            $signatureParts.Add(('{0}|{1}|{2}|{3}' -f $entry.scope, $entry.name, $bytes, $lastWrite))
            $observed.Add([ordered]@{ scope = $entry.scope; name = $entry.name; path = $entry.path; bytes = $bytes; last_write_utc = $lastWrite })
        }
        $signature = ('{0}::{1}' -f $baseName, ($signatureParts -join ';'))
        if (-not $Force -and $State.snapshot_signatures.Contains($baseName) -and $State.snapshot_signatures[$baseName] -eq $signature) {
            continue
        }
        $State.snapshot_signatures[$baseName] = $signature
        if ($State.snapshot_records -ge $MaxSnapshotRecords) {
            $State.snapshot_limit_reached = $true
            Write-DiagRecord -State $State -Record @{
                source = 'lifecycle'
                note   = 'snapshot-limit-reached'
                detail = ("{0} snapshot records" -f $State.snapshot_records)
            }
            return
        }
        $State.snapshot_records++
        $files = New-Object 'System.Collections.Generic.List[object]'
        foreach ($observation in $observed) {
            $entry = [ordered]@{
                scope          = $observation.scope
                name           = $observation.name
                bytes          = $observation.bytes
                last_write_utc = $observation.last_write_utc
                content        = $null
            }
            if ($observation.scope -ne 'control-image') {
                $entry['content'] = Read-DiagContent -Path $observation.path -MaxBytes $MaxContentBytes
            }
            $files.Add($entry)
        }
        Write-DiagRecord -State $State -Record @{
            source            = 'snapshot'
            control_relative  = "$baseName/control"
            session_relative  = $baseName
            signature_sha256  = (Get-StringSha256 -Text $signature)
            focus             = (Test-FocusControl -Files $files)
            files             = $files.ToArray()
        }
    }
}

function Invoke-CimDrain {
    param($State)
    foreach ($sourceIdentifier in @('diag.cim.start', 'diag.cim.stop')) {
        $events = $null
        try {
            $events = Get-Event -SourceIdentifier $sourceIdentifier -ErrorAction SilentlyContinue
        } catch {
            $events = $null
        }
        if (-not $events) { continue }
        foreach ($eventItem in @($events)) {
            try {
                $cimEvent = $eventItem.SourceEventArgs.NewEvent
                if ($cimEvent) {
                    $name = [string]$cimEvent.ProcessName
                    if ($name -match $script:ProcessNamePattern) {
                        $createdUtc = $null
                        try {
                            $createdUtc = [DateTime]::FromFileTimeUtc([int64]$cimEvent.TIME_CREATED).ToString('o')
                        } catch {
                        }
                        if ($sourceIdentifier -eq 'diag.cim.start') {
                            $State.cim_start_events++
                            $record = @{
                                source            = 'cim-start'
                                name              = $name
                                process_pid       = [int]$cimEvent.ProcessID
                                ppid              = [int]$cimEvent.ParentProcessID
                                process_start_utc = $createdUtc
                                command_line      = $null
                                control_relative  = $null
                                role              = $null
                                note              = $null
                            }
                            if ($name -match '^bend lint') {
                                $State.cmdline_queries++
                                try {
                                    $process = Get-CimInstance -ClassName Win32_Process -Filter ("ProcessId = {0}" -f [int]$cimEvent.ProcessID) -ErrorAction Stop
                                    if ($process -and $process.CommandLine) {
                                        $text = [string]$process.CommandLine
                                        if ($text.Length -gt 512) { $text = $text.Substring(0, 512) + '...' }
                                        $record.command_line = $text
                                        $invocation = Get-FixtureInvocation -CommandLine $text -TempRoot $State.paths.temp
                                        $record.role = $invocation.role
                                        $record.control_relative = $invocation.control_relative
                                    }
                                } catch {
                                    $State.cmdline_failures++
                                }
                            }
                            Write-DiagRecord -State $State -Record $record
                        } else {
                            $State.cim_stop_events++
                            $exitStatus = $null
                            try { $exitStatus = [int]$cimEvent.ExitStatus } catch { }
                            Write-DiagRecord -State $State -Record @{
                                source            = 'cim-stop'
                                name              = $name
                                process_pid       = [int]$cimEvent.ProcessID
                                ppid              = [int]$cimEvent.ParentProcessID
                                process_exit_utc  = $createdUtc
                                exit_status       = $exitStatus
                                note              = $null
                            }
                        }
                    } else {
                        $State.cim_filtered++
                    }
                }
            } catch {
                if ($State.error_records -lt 50) {
                    $State.error_records++
                    Write-DiagRecord -State $State -Record @{ source = 'error'; note = 'cim-drain'; detail = $_.Exception.Message }
                }
            } finally {
                try { Remove-Event -EventIdentifier $eventItem.EventIdentifier -ErrorAction SilentlyContinue } catch { }
            }
        }
    }
}

function Invoke-ProcessPoll {
    param($State)
    $State.poll_samples++
    $live = @{}
    $processes = @()
    try {
        $processes = Get-Process -Name @('bend2-lsp*', 'bend lint fixture*', 'bend_lint_protocol*') -ErrorAction SilentlyContinue
    } catch {
        $processes = @()
    }
    foreach ($process in @($processes)) {
        if (-not $process) { continue }
        $live[$process.Id] = $true
        if (-not $State.poll_seen.Contains($process.Id)) {
            $State.poll_seen[$process.Id] = $true
            $State.poll_starts++
            $startUtc = $null
            try { $startUtc = $process.StartTime.ToUniversalTime().ToString('o') } catch { }
            $parentId = $null
            try { if ($process.Parent) { $parentId = $process.Parent.Id } } catch { }
            Write-DiagRecord -State $State -Record @{
                source            = 'poll-start'
                name              = $process.ProcessName
                process_pid       = $process.Id
                ppid              = $parentId
                process_start_utc = $startUtc
                note              = 'cross-check'
            }
        }
    }
    foreach ($id in @($State.poll_seen.Keys)) {
        if (-not $live.Contains($id)) {
            $State.poll_seen.Remove($id)
            $State.poll_stops++
            Write-DiagRecord -State $State -Record @{
                source           = 'poll-stop'
                process_pid      = $id
                process_exit_utc = [DateTime]::UtcNow.ToString('o')
                note             = 'cross-check'
            }
        }
    }
}

# ---------------------------------------------------------------------------------------
# collect
# ---------------------------------------------------------------------------------------

function Invoke-Collect {
    if (-not $Root) { throw '-Root is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    [IO.File]::WriteAllText($paths.pid_file, [string]$PID, [Text.UTF8Encoding]::new($false))
    $state = Initialize-CollectorState -Paths $paths
    $startedUtc = [DateTime]::UtcNow
    $stopReason = 'unknown'
    $watcher = $null
    $subscriptions = @()
    $cimStartSubscribed = $false
    $cimStopSubscribed = $false
    $cimError = $null
    $probeObserved = $false
    $probeSnapshotObserved = $false
    $probeName = '_collector-probe-{0}.txt' -f [guid]::NewGuid().ToString('N')

    # Event actions run in their own runspaces; they read this process's environment and
    # append one bounded line each. Classification and enrichment happen here.
    $env:DIAG_EVENTS_FILE = $paths.events
    $env:DIAG_APPEND_FAILURES = $paths.append_failures
    $env:DIAG_TEMP_ROOT = $paths.temp
    $env:DIAG_PROBE_DIR = $script:ProbeDirName
    $env:DIAG_MAX_CALLBACK_RECORDS = [string]$MaxCallbackRecords
    $env:DIAG_MAX_CALLBACK_BYTES = [string]$MaxCallbackBytes
    # Shared hard budget for watcher callbacks, which run in separate action runspaces.
    # The lockable hashtable lives in the process AppDomain so every callback sees it.
    $callbackBudget = @{
        records        = [long]0
        bytes          = [long]0
        droppedRecords = [long]0
        droppedBytes   = [long]0
        overflow       = [long]0
    }
    [AppDomain]::CurrentDomain.SetData('diag.callbackBudget', $callbackBudget)

    $fileAction = {
        try {
            $path = $EventArgs.FullPath
            $old = $null
            if ($EventArgs.PSObject.Properties['OldFullPath']) { $old = $EventArgs.OldFullPath }
            $leaf = $null
            if ($EventArgs.PSObject.Properties['Name']) { $leaf = [string]$EventArgs.Name }
            if (-not $leaf -and $path) { $leaf = [IO.Path]::GetFileName($path) }
            $interesting = $false
            $root = $env:DIAG_TEMP_ROOT
            if ($root -and $path -and $path.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
                if ($path.IndexOf('\control\', [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
                    $path.IndexOf('/control/', [StringComparison]::OrdinalIgnoreCase) -ge 0) { $interesting = $true }
                if ($leaf -match '\.(bend|json|log|txt|err|out|stderr|messages)$') { $interesting = $true }
            }
            if ($leaf -match '^(bend lint fixture.*\.exe|bend2-lsp.*\.exe|bend_lint_protocol-[0-9a-fA-F]+\.exe|_collector-probe.*\.txt)$') { $interesting = $true }
            $parent = $null
            if ($path) { $parent = [IO.Path]::GetDirectoryName($path) }
            if ($parent -and $root) {
                if ([string]::Equals($parent.TrimEnd([char]92, [char]47), $root.TrimEnd([char]92, [char]47), [StringComparison]::OrdinalIgnoreCase)) { $interesting = $true }
            }
            if (-not $interesting) { return }
            $record = [ordered]@{
                schema = 1
                source = 'fsw'
                event  = $Event.SourceIdentifier
                qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
                freq   = [Diagnostics.Stopwatch]::Frequency
                ts_utc = [DateTime]::UtcNow.ToString('o')
                pid    = $PID
                path   = $path
                old_path = $old
                name   = $leaf
                error  = $null
            }
            $line = ($record | ConvertTo-Json -Compress -Depth 4) + "`n"
            $payloadBytes = [Text.Encoding]::UTF8.GetByteCount($line)
            $budget = [AppDomain]::CurrentDomain.GetData('diag.callbackBudget')
            if ($budget) {
                [Threading.Monitor]::Enter($budget)
                try {
                    if ($budget['records'] -ge [long]$env:DIAG_MAX_CALLBACK_RECORDS -or ($budget['bytes'] + $payloadBytes) -gt [long]$env:DIAG_MAX_CALLBACK_BYTES) {
                        $budget['droppedRecords']++
                        $budget['droppedBytes'] += $payloadBytes
                        $budget['overflow'] = [long]1
                        return
                    }
                    $budget['records']++
                    $budget['bytes'] += $payloadBytes
                } finally {
                    [Threading.Monitor]::Exit($budget)
                }
            }
            for ($attempt = 0; $attempt -lt 40; $attempt++) {
                try {
                    [IO.File]::AppendAllText($env:DIAG_EVENTS_FILE, $line, [Text.UTF8Encoding]::new($false))
                    return
                } catch {
                    Start-Sleep -Milliseconds 5
                }
            }
            throw 'append retries exhausted'
        } catch {
            try {
                $failure = [ordered]@{
                    ts_utc = [DateTime]::UtcNow.ToString('o')
                    pid    = $PID
                    source = 'fsw-action'
                    error  = $_.Exception.Message
                }
                [IO.File]::AppendAllText($env:DIAG_APPEND_FAILURES, (($failure | ConvertTo-Json -Compress) + "`n"), [Text.UTF8Encoding]::new($false))
            } catch {
            }
        }
    }
    $errorAction = {
        try {
            $record = [ordered]@{
                schema = 1
                source = 'fsw-error'
                event  = 'Error'
                qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
                freq   = [Diagnostics.Stopwatch]::Frequency
                ts_utc = [DateTime]::UtcNow.ToString('o')
                pid    = $PID
                path   = $null
                old_path = $null
                name   = $null
                error  = $EventArgs.GetException().Message
            }
            $line = ($record | ConvertTo-Json -Compress -Depth 4) + "`n"
            $payloadBytes = [Text.Encoding]::UTF8.GetByteCount($line)
            $budget = [AppDomain]::CurrentDomain.GetData('diag.callbackBudget')
            if ($budget) {
                [Threading.Monitor]::Enter($budget)
                try {
                    if ($budget['records'] -ge [long]$env:DIAG_MAX_CALLBACK_RECORDS -or ($budget['bytes'] + $payloadBytes) -gt [long]$env:DIAG_MAX_CALLBACK_BYTES) {
                        $budget['droppedRecords']++
                        $budget['droppedBytes'] += $payloadBytes
                        $budget['overflow'] = [long]1
                        return
                    }
                    $budget['records']++
                    $budget['bytes'] += $payloadBytes
                } finally {
                    [Threading.Monitor]::Exit($budget)
                }
            }
            for ($attempt = 0; $attempt -lt 40; $attempt++) {
                try {
                    [IO.File]::AppendAllText($env:DIAG_EVENTS_FILE, $line, [Text.UTF8Encoding]::new($false))
                    return
                } catch {
                    Start-Sleep -Milliseconds 5
                }
            }
        } catch {
        }
    }

    try {
        $watcher = [IO.FileSystemWatcher]::new($paths.temp)
        $watcher.IncludeSubdirectories = $true
        $watcher.NotifyFilter = [IO.NotifyFilters]'FileName, DirectoryName, LastWrite, CreationTime, Size'
        $watcher.InternalBufferSize = 65536
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName Created -SourceIdentifier 'diag.fsw.created' -Action $fileAction -ErrorAction Stop
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName Changed -SourceIdentifier 'diag.fsw.changed' -Action $fileAction -ErrorAction Stop
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName Deleted -SourceIdentifier 'diag.fsw.deleted' -Action $fileAction -ErrorAction Stop
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName Renamed -SourceIdentifier 'diag.fsw.renamed' -Action $fileAction -ErrorAction Stop
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName Error -SourceIdentifier 'diag.fsw.error' -Action $errorAction -ErrorAction Stop
        $watcher.EnableRaisingEvents = $true
    } catch {
        $cimError = $_.Exception.Message
        Write-JsonFile -Path $paths.ready -Object ([ordered]@{
            schema       = 1
            ready        = $false
            ready_reason = 'filesystem-watcher-unavailable'
            error        = $_.Exception.Message
            collector_pid = $PID
            ready_utc    = [DateTime]::UtcNow.ToString('o')
        })
        return 4
    }

    try {
        $subscriptions += Register-CimIndicationEvent -Query 'SELECT * FROM Win32_ProcessStartTrace' -SourceIdentifier 'diag.cim.start' -Namespace 'root/CIMV2' -ErrorAction Stop
        $cimStartSubscribed = $true
    } catch {
        $cimError = $_.Exception.Message
    }
    try {
        $subscriptions += Register-CimIndicationEvent -Query 'SELECT * FROM Win32_ProcessStopTrace' -SourceIdentifier 'diag.cim.stop' -Namespace 'root/CIMV2' -ErrorAction Stop
        $cimStopSubscribed = $true
    } catch {
        if (-not $cimError) { $cimError = $_.Exception.Message }
    }

    Write-DiagRecord -State $state -Record @{
        source    = 'lifecycle'
        note      = 'collector-start'
        detail    = 'observer armed'
        temp_root = $paths.temp
        events    = $paths.events
        limits    = $script:Limits
    }

    # Readiness: a probe file must be observed through the watcher, and the snapshot
    # diff must see a probe control directory. Both are removed before the real test.
    $probeFile = Join-Path $paths.temp $probeName
    [IO.File]::WriteAllText($probeFile, 'probe', [Text.UTF8Encoding]::new($false))
    $probeDir = Join-Path $paths.temp $script:ProbeDirName
    [void][IO.Directory]::CreateDirectory((Join-Path $probeDir 'control'))
    [IO.File]::WriteAllText((Join-Path $probeDir 'control/address'), '127.0.0.1:1', [Text.UTF8Encoding]::new($false))
    $probeDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while ([DateTime]::UtcNow -lt $probeDeadline) {
        try {
            if ([IO.File]::Exists($paths.events)) {
                $text = [IO.File]::ReadAllText($paths.events)
                if ($text.Contains($probeName)) { $probeObserved = $true; break }
            }
        } catch {
        }
        Start-Sleep -Milliseconds 100
    }
    Invoke-ControlSnapshot -State $state -Force
    try {
        if ([IO.File]::Exists($paths.events)) {
            $text = [IO.File]::ReadAllText($paths.events)
            if ($text.Contains($script:ProbeDirName)) { $probeSnapshotObserved = $true }
        }
    } catch {
    }
    if ($probeObserved) {
        [IO.File]::WriteAllText($paths.probe_observed, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))
    }
    try {
        if ([IO.Directory]::Exists($probeDir)) { [IO.Directory]::Delete($probeDir, $true) }
        if ([IO.File]::Exists($probeFile)) { [IO.File]::Delete($probeFile) }
    } catch {
    }

    $ready = ($probeObserved -and $probeSnapshotObserved)
    $readyReason = if ($ready) { 'probe-observed' } else { 'probe-not-observed' }
    Write-JsonFile -Path $paths.ready -Object ([ordered]@{
        schema        = 1
        ready         = $ready
        ready_reason  = $readyReason
        ready_utc     = [DateTime]::UtcNow.ToString('o')
        armed_utc     = $startedUtc.ToString('o')
        collector_pid = $PID
        powershell    = $PSVersionTable.PSVersion.ToString()
        os            = [Environment]::OSVersion.VersionString
        temp_root     = $paths.temp
        events_path   = $paths.events
        watcher       = [ordered]@{
            include_subdirectories = $true
            internal_buffer_size   = 65536
            notify_filter          = 'FileName, DirectoryName, LastWrite, CreationTime, Size'
            probe_file_observed    = $probeObserved
            probe_snapshot_observed = $probeSnapshotObserved
        }
        cim           = [ordered]@{
            namespace         = 'root/CIMV2'
            start_subscribed  = $cimStartSubscribed
            stop_subscribed   = $cimStopSubscribed
            error             = $cimError
        }
        process_poll  = [ordered]@{
            enabled      = $true
            interval_ms  = 400
            name_filters = @('bend2-lsp*', 'bend lint fixture*', 'bend_lint_protocol*')
        }
        limits        = $script:Limits
    })
    Write-Host ("collector-ready {0} pid={1}" -f $readyReason, $PID)
    if (-not $ready) {
        Write-DiagRecord -State $state -Record @{ source = 'lifecycle'; note = 'readiness-failed'; detail = $readyReason }
        Write-JsonFile -Path $paths.summary -Object ([ordered]@{
            schema        = 1
            pid           = $PID
            started_utc   = $startedUtc.ToString('o')
            stopped_utc   = [DateTime]::UtcNow.ToString('o')
            stop_reason   = 'readiness-failed'
            ready         = $false
            ready_reason  = $readyReason
            main_records_written = $state.written
            main_records_dropped = $state.dropped
            counter_note  = 'Main-loop records only; FileSystemWatcher action lines are appended independently and are counted by the analysis.'
            callback      = [ordered]@{
                records         = $callbackBudget['records']
                bytes           = $callbackBudget['bytes']
                dropped_records = $callbackBudget['droppedRecords']
                dropped_bytes   = $callbackBudget['droppedBytes']
                overflow        = ($callbackBudget['overflow'] -ne 0)
                record_limit    = $MaxCallbackRecords
                byte_limit      = $MaxCallbackBytes
            }
        })
        return 4
    }

    $stopFile = $paths.stop_file
    $leaseFile = $paths.lease_file
    $deadline = [DateTime]::UtcNow.AddSeconds($CollectorDurationSeconds)
    $iteration = 0
    $lastCallbackRecords = [long]-1
    $lastCallbackDrops = [long]-1
    while ($true) {
        $iteration++
        try {
            Invoke-CimDrain -State $state
            if ($iteration % 2 -eq 0) { Invoke-ControlSnapshot -State $state }
            if ($iteration % 4 -eq 0) { Invoke-ProcessPoll -State $state }
            if ($iteration % 100 -eq 0) {
                $snapshot = $null
                [Threading.Monitor]::Enter($callbackBudget)
                try {
                    $snapshot = [ordered]@{
                        records        = $callbackBudget['records']
                        bytes          = $callbackBudget['bytes']
                        dropped_records = $callbackBudget['droppedRecords']
                        dropped_bytes  = $callbackBudget['droppedBytes']
                        overflow       = ($callbackBudget['overflow'] -ne 0)
                    }
                } finally {
                    [Threading.Monitor]::Exit($callbackBudget)
                }
                if ($snapshot.records -ne $lastCallbackRecords -or $snapshot.dropped_records -ne $lastCallbackDrops) {
                    $lastCallbackRecords = $snapshot.records
                    $lastCallbackDrops = $snapshot.dropped_records
                    Write-DiagRecord -State $state -Record @{
                        source = 'callback-budget'
                        detail = 'watcher callback budget'
                        records = $snapshot.records
                        bytes = $snapshot.bytes
                        dropped_records = $snapshot.dropped_records
                        dropped_bytes = $snapshot.dropped_bytes
                        overflow = $snapshot.overflow
                        record_limit = $MaxCallbackRecords
                        byte_limit = $MaxCallbackBytes
                    }
                }
            }
            if ($iteration % 600 -eq 0) {
                Write-DiagRecord -State $state -Record @{
                    source = 'lifecycle'
                    note   = 'heartbeat'
                    detail = ("iteration {0}" -f $iteration)
                }
            }
            if ($iteration % 10 -eq 0) {
                if ([IO.File]::Exists($leaseFile)) {
                    $age = ([DateTime]::UtcNow - [IO.File]::GetLastWriteTimeUtc($leaseFile)).TotalSeconds
                    if ($age -gt $LeaseSeconds) { $stopReason = 'lease-expired'; break }
                } elseif ($iteration -gt 20) {
                    $stopReason = 'lease-missing'
                    break
                }
            }
            if ([IO.File]::Exists($stopFile)) { $stopReason = 'stop-requested'; break }
            if ([DateTime]::UtcNow -gt $deadline) { $stopReason = 'duration-limit'; break }
        } catch {
            if ($state.error_records -lt 50) {
                $state.error_records++
                Write-DiagRecord -State $state -Record @{ source = 'error'; note = 'collection-loop'; detail = $_.Exception.Message }
            }
        }
        Start-Sleep -Milliseconds 100
    }

    # Final state read: catches anything the watcher and snapshots missed.
    try {
        foreach ($baseDir in ([IO.Directory]::GetDirectories($paths.temp) | Sort-Object)) {
            $baseName = [IO.Path]::GetFileName($baseDir)
            $entries = Get-SessionScanEntries -BaseDir $baseDir
            if ($entries.Count -eq 0) { continue }
            $files = New-Object 'System.Collections.Generic.List[object]'
            foreach ($scanEntry in $entries) {
                $entry = [ordered]@{
                    scope          = $scanEntry.scope
                    name           = $scanEntry.name
                    bytes          = $null
                    last_write_utc = $null
                    content        = $null
                }
                try {
                    $info = [IO.FileInfo]::new($scanEntry.path)
                    if ($info.Exists) {
                        $entry.bytes = $info.Length
                        $entry.last_write_utc = $info.LastWriteTimeUtc.ToString('o')
                    }
                } catch {
                }
                if ($scanEntry.scope -ne 'control-image') {
                    $entry['content'] = Read-DiagContent -Path $scanEntry.path -MaxBytes $MaxContentBytes
                }
                $files.Add($entry)
            }
            Write-DiagRecord -State $state -Record @{
                source           = 'final-read'
                control_relative = "$baseName/control"
                session_relative = $baseName
                focus            = (Test-FocusControl -Files $files)
                files            = $files.ToArray()
            }
        }
        $inventory = @()
        foreach ($child in ([IO.Directory]::GetDirectories($paths.temp) | Sort-Object)) {
            $entries = @()
            try { $entries = @([IO.Directory]::GetFileSystemEntries($child) | ForEach-Object { [IO.Path]::GetFileName($_) } | Sort-Object) } catch { }
            $inventory += [ordered]@{
                name    = [IO.Path]::GetFileName($child)
                entries = $entries
            }
        }
        Write-DiagRecord -State $state -Record @{ source = 'inventory'; directories = $inventory }
    } catch {
        if ($state.error_records -lt 50) {
            $state.error_records++
            Write-DiagRecord -State $state -Record @{ source = 'error'; note = 'final-read'; detail = $_.Exception.Message }
        }
    }

    Write-DiagRecord -State $state -Record @{ source = 'lifecycle'; note = 'collector-stop'; detail = $stopReason; exit_code = 0 }
    try {
        if ($watcher) {
            $watcher.EnableRaisingEvents = $false
            $watcher.Dispose()
        }
    } catch {
    }
    foreach ($subscription in $subscriptions) {
        if ($subscription -and $subscription.Name) {
            try { Unregister-Event -SourceIdentifier $subscription.Name -ErrorAction SilentlyContinue } catch { }
        }
    }
    foreach ($sourceIdentifier in @('diag.cim.start', 'diag.cim.stop')) {
        try { Unregister-Event -SourceIdentifier $sourceIdentifier -ErrorAction SilentlyContinue } catch { }
    }

    $stoppedUtc = [DateTime]::UtcNow
    Write-JsonFile -Path $paths.summary -Object ([ordered]@{
        schema             = 1
        pid                = $PID
        started_utc        = $startedUtc.ToString('o')
        stopped_utc        = $stoppedUtc.ToString('o')
        duration_seconds   = [Math]::Round(($stoppedUtc - $startedUtc).TotalSeconds, 3)
        stop_reason        = $stopReason
        iterations         = $iteration
        ready              = $true
        main_records_written = $state.written
        main_records_dropped = $state.dropped
        counter_note       = 'Main-loop records only; FileSystemWatcher action lines are appended independently and are counted by the analysis.'
        snapshot_records   = $state.snapshot_records
        snapshot_limit_reached = $state.snapshot_limit_reached
        event_byte_limit_reached = $state.byte_limit_reached
        callback           = [ordered]@{
            records         = $callbackBudget['records']
            bytes           = $callbackBudget['bytes']
            dropped_records = $callbackBudget['droppedRecords']
            dropped_bytes   = $callbackBudget['droppedBytes']
            overflow        = ($callbackBudget['overflow'] -ne 0)
            record_limit    = $MaxCallbackRecords
            byte_limit      = $MaxCallbackBytes
        }
        cim                = [ordered]@{
            start_subscribed = $cimStartSubscribed
            stop_subscribed  = $cimStopSubscribed
            error            = $cimError
            start_events     = $state.cim_start_events
            stop_events      = $state.cim_stop_events
            filtered_events  = $state.cim_filtered
        }
        poll               = [ordered]@{
            samples = $state.poll_samples
            starts  = $state.poll_starts
            stops   = $state.poll_stops
        }
        command_line       = [ordered]@{
            queries  = $state.cmdline_queries
            failures = $state.cmdline_failures
        }
        events_bytes       = if ([IO.File]::Exists($paths.events)) { ([IO.FileInfo]::new($paths.events)).Length } else { 0 }
        limits             = $script:Limits
    })
    return 0
}

# ---------------------------------------------------------------------------------------
# start / run / stop
# ---------------------------------------------------------------------------------------

function Invoke-Start {
    if (-not $Root) { throw '-Root is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    if (-not [IO.Directory]::Exists($paths.temp)) { throw "diagnostic root is not prepared: $($paths.root)" }
    $existingPid = $null
    if ([IO.File]::Exists($paths.pid_file)) {
        try { $existingPid = [int]([IO.File]::ReadAllText($paths.pid_file).Trim()) } catch { $existingPid = $null }
    }
    if ($existingPid -and (Get-Process -Id $existingPid -ErrorAction SilentlyContinue)) {
        throw "a collector is already running with pid $existingPid"
    }
    foreach ($stale in @($paths.stop_file, $paths.ready, $paths.summary, $paths.probe_observed)) {
        try { if ([IO.File]::Exists($stale)) { [IO.File]::Delete($stale) } } catch { }
    }
    [IO.File]::WriteAllText($paths.lease_file, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($paths.events, '', [Text.UTF8Encoding]::new($false))

    $pwshPath = Join-Path $PSHOME 'pwsh.exe'
    if (-not [IO.File]::Exists($pwshPath)) {
        $current = Get-Process -Id $PID -ErrorAction Stop
        $pwshPath = $current.Path
    }
    $arguments = @(
        '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
        '-File', $PSCommandPath,
        '-Phase', 'collect',
        '-Root', $paths.root,
        '-CollectorDurationSeconds', [string]$CollectorDurationSeconds,
        '-LeaseSeconds', [string]$LeaseSeconds,
        '-MaxRecords', [string]$MaxRecords,
        '-MaxSnapshotRecords', [string]$MaxSnapshotRecords,
        '-MaxContentBytes', [string]$MaxContentBytes
    )
    $quoted = ($arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $launch = @{
        FilePath               = $pwshPath
        ArgumentList           = $quoted
        PassThru               = $true
        RedirectStandardOutput = $paths.stdout_log
        RedirectStandardError  = $paths.stderr_log
    }
    if ($IsWindows) { $launch['WindowStyle'] = 'Hidden' }
    $process = Start-Process @launch
    $process.Refresh()
    $creationUtc = $null
    try { $creationUtc = $process.StartTime.ToUniversalTime().ToString('o') } catch { $creationUtc = $null }
    $identity = [ordered]@{
        schema        = 1
        pid           = $process.Id
        creation_utc  = $creationUtc
        image         = $pwshPath
        script        = $PSCommandPath
        root          = $paths.root
        launched_utc  = [DateTime]::UtcNow.ToString('o')
        argv          = $arguments
        identity_bound = [bool]($creationUtc -and $process.Id)
    }
    Write-JsonFile -Path $paths.collector_identity -Object $identity

    $launchedUtc = [DateTime]::UtcNow
    $deadline = $launchedUtc.AddSeconds($ReadyTimeoutSeconds)
    $ready = $null
    $exitEarly = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        $process.Refresh()
        if ($process.HasExited) { $exitEarly = $true; break }
        if ([IO.File]::Exists($paths.ready)) {
            $ready = Read-JsonFile -Path $paths.ready
            if ($ready) { break }
        }
        Start-Sleep -Milliseconds 200
    }
    $lifecycle = [ordered]@{
        schema              = 1
        launched_utc        = $launchedUtc.ToString('o')
        collector_pid       = $process.Id
        pwsh                = $pwshPath
        argv                = $arguments
        exit_early          = $exitEarly
        ready               = if ($ready) { [bool]$ready.ready } else { $false }
        ready_reason        = if ($ready) { $ready.ready_reason } else { 'timeout' }
        ready_file          = $paths.ready
        stdout_log          = $paths.stdout_log
        stderr_log          = $paths.stderr_log
        stderr_tail         = Get-DiagStderrTail -Path $paths.stderr_log
    }
    if ($exitEarly) {
        try { $process.WaitForExit(5000) | Out-Null } catch { }
        $lifecycle['exit_code'] = $process.ExitCode
    }
    Write-JsonFile -Path $paths.readiness -Object $lifecycle
    if (-not $ready -or -not $ready.ready -or $exitEarly) {
        Write-Warning ("collector is not ready: {0}" -f $lifecycle.ready_reason)
        if (-not $exitEarly) {
            [IO.File]::WriteAllText($paths.stop_file, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))
            try { $process.WaitForExit($StopTimeoutSeconds * 1000) | Out-Null } catch { }
            $process.Refresh()
            if (-not $process.HasExited) {
                # This Process handle is the exact child launched above, not a PID lookup.
                try { Stop-Process -Id $process.Id -Force -ErrorAction Stop } catch { }
            }
        }
        throw ("observer readiness failed: {0}" -f $lifecycle.ready_reason)
    }
    Write-Host ("collector {0} ready: {1}" -f $process.Id, $ready.ready_reason)
}

function Get-RunLogEvidence {
    param([string]$LogPath, [string]$TempRoot, [int]$MaxDiagnostics = 4, [int]$MaxHighlights = 40, [int]$MaxText = 1024)
    $evidence = [ordered]@{
        log_present                  = $false
        log_bytes                    = $null
        observed_tests               = $null
        result_line                  = $null
        failed_tests                 = @()
        running_binaries             = @()
        wait_file_diagnostics        = @()
        wait_file_diagnostics_omitted = 0
        highlights                   = @()
    }
    if (-not [IO.File]::Exists($LogPath)) { return $evidence }
    $evidence.log_present = $true
    $info = [IO.FileInfo]::new($LogPath)
    $evidence.log_bytes = $info.Length
    if ($info.Length -gt 8388608) {
        $lines = Get-Content -LiteralPath $LogPath -Tail 4000 -ErrorAction SilentlyContinue
    } else {
        $lines = Get-Content -LiteralPath $LogPath -ErrorAction SilentlyContinue
    }
    $highlights = New-Object 'System.Collections.Generic.List[string]'
    $keyword = '(?i)(bend-lint|logMessage|timed out|timeout|wait_file|C7_WAIT_FILE|panicked|fixture did not|server\.stderr|address)'
    foreach ($text in @($lines)) {
        if ($null -eq $text) { continue }
        if ($text -match '^running (\d+) tests?$') { $evidence.observed_tests = [int]$Matches[1] }
        if ($text -match '^test result: (ok|FAILED)\.') { $evidence.result_line = $text.Trim() }
        if ($text -match '^test (.+) \.\.\. FAILED$') { $evidence.failed_tests += $Matches[1] }
        if ($text -match 'Running (tests\\[^ ]+\.exe)') { $evidence.running_binaries += $Matches[1] }
        if ($text -match 'C7_WAIT_FILE_DIAGNOSTIC\s+(\{.*\})\s*$') {
            $raw = $Matches[1]
            if ($evidence.wait_file_diagnostics.Count -ge $MaxDiagnostics) {
                $evidence.wait_file_diagnostics_omitted++
                continue
            }
            try {
                $parsed = $raw | ConvertFrom-Json -AsHashtable -ErrorAction Stop
                $bounded = [ordered]@{
                    missing            = $parsed['missing']
                    control_relative   = Get-TempRelative -Path ([string]$parsed['control']) -TempRoot $TempRoot
                    server_exit_status = $parsed['server_exit_status']
                    raw_line_sha256    = (Get-StringSha256 -Text $raw)
                    raw_line_bytes     = $raw.Length
                    messages           = @()
                    files              = [ordered]@{}
                }
                foreach ($message in (@($parsed['messages']) | Select-Object -First 32)) {
                    $rendered = ($message['message'] | ConvertTo-Json -Compress -Depth 8)
                    if ($null -eq $rendered) { $rendered = '' }
                    $truncated = ($rendered.Length -gt $MaxText)
                    if ($truncated) { $rendered = $rendered.Substring(0, $MaxText) }
                    $bounded.messages += [ordered]@{
                        received_ago_ms = $message['received_ago_ms']
                        truncated       = $truncated
                        text            = $rendered
                    }
                }
                if ($parsed['files']) {
                    foreach ($name in @($parsed['files'].Keys)) {
                        $observation = $parsed['files'][$name]
                        $entry = [ordered]@{
                            open_error = $observation['open_error']
                            read_error = $observation['read_error']
                            truncated  = $false
                            text       = ''
                        }
                        $contents = $observation['contents_utf8_lossy']
                        if ($null -ne $contents) {
                            $entry.text = [string]$contents
                            if ($entry.text.Length -gt $MaxText) {
                                $entry.text = $entry.text.Substring(0, $MaxText)
                                $entry.truncated = $true
                            }
                        }
                        $bounded.files[$name] = $entry
                    }
                }
                $evidence.wait_file_diagnostics += $bounded
            } catch {
                $evidence.wait_file_diagnostics += [ordered]@{
                    parse_error     = $_.Exception.Message
                    raw_line_sha256 = (Get-StringSha256 -Text $raw)
                    raw_line_bytes  = $raw.Length
                    text            = if ($raw.Length -gt $MaxText) { $raw.Substring(0, $MaxText) } else { $raw }
                }
            }
        } elseif ($text -match $keyword -and $highlights.Count -lt $MaxHighlights) {
            $snippet = $text.Trim()
            if ($snippet.Length -gt 400) { $snippet = $snippet.Substring(0, 400) + '...' }
            $highlights.Add($snippet)
        }
    }
    $evidence.highlights = $highlights.ToArray()
    return $evidence
}

function Invoke-Build {
    if (-not $Root) { throw '-Root is required' }
    if (-not $Sut) { throw '-Sut is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    $sutPath = Get-NormalizedPath -Path $Sut
    $log = Join-Path $paths.logs 'cargo-build.log'
    $startedUtc = [DateTime]::UtcNow
    $code = 1
    Push-Location $sutPath
    try {
        & cargo test --locked --test bend_lint_protocol --no-run 2>&1 | Tee-Object -FilePath $log
        $code = $LASTEXITCODE
    } catch {
        Write-Host ("build invocation failed: {0}" -f $_.Exception.Message)
        $code = 127
        Add-Content -LiteralPath $log -Value ("build invocation failed: {0}" -f $_.Exception.Message)
    } finally {
        Pop-Location
    }
    $stoppedUtc = [DateTime]::UtcNow
    $binaries = @()
    $deps = Join-Path $sutPath 'target/debug/deps'
    if ([IO.Directory]::Exists($deps)) {
        foreach ($file in ([IO.Directory]::GetFiles($deps, 'bend_lint_protocol-*.exe') | Sort-Object)) {
            $binaries += [ordered]@{
                path   = [IO.Path]::GetRelativePath($sutPath, $file) -replace '\\', '/'
                bytes  = ([IO.FileInfo]::new($file)).Length
                sha256 = (Get-DiagSha256 -Path $file)
            }
        }
    }
    $server = Join-Path $sutPath 'target/debug/bend2-lsp.exe'
    Write-JsonFile -Path $paths.build_json -Object ([ordered]@{
        schema          = 1
        command         = $script:BuildCommand
        working_directory = $sutPath
        exit_code       = $code
        started_utc     = $startedUtc.ToString('o')
        stopped_utc     = $stoppedUtc.ToString('o')
        duration_seconds = [Math]::Round(($stoppedUtc - $startedUtc).TotalSeconds, 3)
        log             = $log
        test_binaries   = $binaries
        test_source_sha256 = if ([IO.File]::Exists((Join-Path $sutPath $script:SubstitutedTestRelative))) { Get-DiagSha256 -Path (Join-Path $sutPath $script:SubstitutedTestRelative) } else { $null }
        server_binary   = if ([IO.File]::Exists($server)) {
            [ordered]@{ path = 'target/debug/bend2-lsp.exe'; bytes = ([IO.FileInfo]::new($server)).Length; sha256 = (Get-DiagSha256 -Path $server) }
        } else { $null }
        note            = 'Prebuild only. The run phase executes the identical cargo command once; nothing here asserts test success.'
    })
    exit $code
}

function Invoke-Run {
    if (-not $Root) { throw '-Root is required' }
    if (-not $Sut) { throw '-Sut is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    $sutPath = Get-NormalizedPath -Path $Sut
    $ready = Read-JsonFile -Path $paths.ready
    $collectorPid = $null
    if ([IO.File]::Exists($paths.pid_file)) {
        try { $collectorPid = [int]([IO.File]::ReadAllText($paths.pid_file).Trim()) } catch { $collectorPid = $null }
    }
    $collectorAlive = $false
    if ($collectorPid) { $collectorAlive = [bool](Get-Process -Id $collectorPid -ErrorAction SilentlyContinue) }
    $build = Read-JsonFile -Path $paths.build_json
    $buildExit = $null
    if ($build) {
        try { $buildExit = [int]$build['exit_code'] } catch { $buildExit = $null }
    }
    $buildOk = ($null -ne $buildExit -and $buildExit -eq 0)
    $testFile = Join-Path $sutPath $script:SubstitutedTestRelative
    $actualTestHash = if ([IO.File]::Exists($testFile)) { Get-DiagSha256 -Path $testFile } else { $null }
    $testHashOk = (-not $ExpectedTestSha256) -or ([string]$actualTestHash -eq $ExpectedTestSha256.ToLowerInvariant())
    if (-not $ready -or -not $ready.ready -or -not $collectorAlive -or -not $buildOk -or -not $testHashOk) {
        $reason = if (-not $ready -or -not $ready.ready) { 'observer not ready; the original test was not run' }
            elseif (-not $collectorAlive) { 'observer process is gone; the original test was not run' }
            elseif (-not $buildOk) { 'build is not recorded as successful; the original test was not run' }
            else { 'the substituted test digest does not match the pinned value; the original test was not run' }
        Write-JsonFile -Path $paths.run_exit -Object ([ordered]@{
            schema        = 1
            aborted       = $true
            test_started  = $false
            exit_code     = 90
            reason        = $reason
            ready         = $ready
            collector_pid = $collectorPid
            collector_alive = $collectorAlive
            build_exit_code = $buildExit
            expected_test_sha256 = $ExpectedTestSha256
            actual_test_sha256 = $actualTestHash
            generated_utc = [DateTime]::UtcNow.ToString('o')
        })
        [Console]::Error.WriteLine($reason)
        exit 90
    }
    [IO.File]::WriteAllText($paths.lease_file, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))
    $preRunEntries = @()
    try {
        $preRunEntries = @([IO.Directory]::GetFileSystemEntries($paths.temp) |
            ForEach-Object { [IO.Path]::GetFileName($_) } | Sort-Object | Select-Object -First 40)
    } catch {
    }
    $startRecord = [ordered]@{
        schema            = 1
        command           = $script:RunCommand
        working_directory = $sutPath
        tmp               = $paths.temp
        temp              = $paths.temp
        bend_no_telemetry = $env:BEND_NO_TELEMETRY
        expected_tests    = $script:ExpectedWindowsTests
        started_utc       = [DateTime]::UtcNow.ToString('o')
        collector_pid     = $collectorPid
        test_source_sha256 = $actualTestHash
        expected_test_sha256 = $ExpectedTestSha256
        temp_pre_run_entries = $preRunEntries
        retries           = 0
    }
    Write-JsonFile -Path $paths.run_start -Object $startRecord
    $line = [ordered]@{
        schema = 1
        source = 'lifecycle'
        note   = 'test-run-start'
        qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
        freq   = [Diagnostics.Stopwatch]::Frequency
        ts_utc = [DateTime]::UtcNow.ToString('o')
        pid    = $PID
        detail = $script:RunCommand
    }
    Add-DiagLine -Path $paths.events -Line ($line | ConvertTo-Json -Compress -Depth 4) | Out-Null

    $env:TMP = $paths.temp
    $env:TEMP = $paths.temp
    $log = Join-Path $paths.logs 'cargo-test.log'
    $code = 127
    $startedUtc = [DateTime]::UtcNow
    Push-Location $sutPath
    try {
        & cargo test --locked --test bend_lint_protocol 2>&1 | Tee-Object -FilePath $log
        $code = $LASTEXITCODE
    } catch {
        Write-Host ("test invocation failed: {0}" -f $_.Exception.Message)
        $code = 127
        Add-Content -LiteralPath $log -Value ("test invocation failed: {0}" -f $_.Exception.Message)
    } finally {
        Pop-Location
    }
    $stoppedUtc = [DateTime]::UtcNow

    $evidence = Get-RunLogEvidence -LogPath $log -TempRoot $paths.temp
    $endLine = [ordered]@{
        schema = 1
        source = 'lifecycle'
        note   = 'test-run-end'
        qpc    = [Diagnostics.Stopwatch]::GetTimestamp()
        freq   = [Diagnostics.Stopwatch]::Frequency
        ts_utc = [DateTime]::UtcNow.ToString('o')
        pid    = $PID
        detail = ('exit {0}' -f $code)
        exit_code = $code
    }
    Add-DiagLine -Path $paths.events -Line ($endLine | ConvertTo-Json -Compress -Depth 4) | Out-Null

    Write-JsonFile -Path $paths.run_exit -Object ([ordered]@{
        schema            = 1
        aborted           = $false
        test_started      = $true
        command           = $script:RunCommand
        working_directory = $sutPath
        exit_code         = $code
        started_utc       = $startedUtc.ToString('o')
        stopped_utc       = $stoppedUtc.ToString('o')
        duration_seconds  = [Math]::Round(($stoppedUtc - $startedUtc).TotalSeconds, 3)
        tmp               = $paths.temp
        temp              = $paths.temp
        bend_no_telemetry = $env:BEND_NO_TELEMETRY
        expected_tests    = $script:ExpectedWindowsTests
        observed_tests    = $evidence.observed_tests
        expected_tests_observed = ($evidence.observed_tests -eq $script:ExpectedWindowsTests)
        result_line       = $evidence.result_line
        failed_tests      = @($evidence.failed_tests)
        running_binaries  = @($evidence.running_binaries)
        test_source_sha256 = if ([IO.File]::Exists((Join-Path $sutPath $script:SubstitutedTestRelative))) { Get-DiagSha256 -Path (Join-Path $sutPath $script:SubstitutedTestRelative) } else { $null }
        wait_file_diagnostics = @($evidence.wait_file_diagnostics)
        wait_file_diagnostics_omitted = $evidence.wait_file_diagnostics_omitted
        failure_highlights = @($evidence.highlights)
        log_present       = $evidence.log_present
        log_bytes         = $evidence.log_bytes
        log               = $log
        retries           = 0
        note              = 'Original exit code preserved. This file records evidence only; it does not reinterpret the quality workflow result.'
    })
    exit $code
}

function Invoke-Stop {
    if (-not $Root) { throw '-Root is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    $identity = Read-JsonFile -Path $paths.collector_identity
    $pidFromFile = $null
    if ([IO.File]::Exists($paths.pid_file)) {
        try { $pidFromFile = [int]([IO.File]::ReadAllText($paths.pid_file).Trim()) } catch { $pidFromFile = $null }
    }
    $record = [ordered]@{
        schema              = 1
        collector_pid       = if ($identity) { $identity.pid } else { $pidFromFile }
        pid_file_value      = $pidFromFile
        stop_requested_utc  = [DateTime]::UtcNow.ToString('o')
        collector_present   = $false
        stop_mode           = 'absent'
        identity_verification = 'not-applicable'
        owned               = $false
        exit_code           = $null
        lingering_children  = @()
        summary             = $null
        stderr_tail         = $null
    }
    $finish = {
        param([int]$Code)
        $record['stopped_utc'] = [DateTime]::UtcNow.ToString('o')
        $record.summary = Read-JsonFile -Path $paths.summary
        $record.stderr_tail = Get-DiagStderrTail -Path $paths.stderr_log
        Write-JsonFile -Path $paths.lifecycle -Object $record
        return $Code
    }

    if (-not $identity) {
        if ($pidFromFile -and (Get-Process -Id $pidFromFile -ErrorAction SilentlyContinue)) {
            # A PID exists but no launch identity does: never signal or kill it.
            $record.stop_mode = 'identity-uncertain'
            $record.identity_verification = 'missing'
            $record['collector_gone'] = $false
            Write-Warning 'observer identity is unavailable; refusing to signal or kill a PID-only process'
            return (& $finish 1)
        }
        return (& $finish 0)
    }

    $expectedPid = $null
    try { $expectedPid = [int]$identity.pid } catch { $expectedPid = $null }
    if (-not $expectedPid) {
        $record.stop_mode = 'identity-uncertain'
        $record.identity_verification = 'missing'
        return (& $finish 1)
    }
    if ($pidFromFile -and $pidFromFile -ne $expectedPid) {
        $record.stop_mode = 'identity-uncertain'
        $record.identity_verification = 'mismatch'
        $record['pid_files_agree'] = $false
        Write-Warning ("observer identity disagrees with the pid file: {0} vs {1}" -f $expectedPid, $pidFromFile)
        return (& $finish 1)
    }

    $process = Get-Process -Id $expectedPid -ErrorAction SilentlyContinue
    if (-not $process) {
        $record.stop_mode = 'already-exited'
        $record.identity_verification = 'verified-pid-absent'
        $record['collector_gone'] = $true
        $record['exit_code_source'] = 'unavailable'
        return (& $finish 0)
    }
    $record.collector_present = $true
    $record['pid_files_agree'] = ($null -eq $pidFromFile -or $pidFromFile -eq $expectedPid)

    # Bind the live process to the launch record: PID plus creation time plus image plus
    # the exact script path. Only a fully verified process may be signalled or killed.
    $expectedCreation = $null
    if ($identity.creation_utc -is [DateTime]) {
        $expectedCreation = $identity.creation_utc.ToUniversalTime()
    } elseif ($identity.creation_utc) {
        $expectedCreation = ConvertTo-UtcDateTime -Text ([string]$identity.creation_utc)
    }
    $actualCreation = $null
    try { $actualCreation = $process.StartTime.ToUniversalTime() } catch { $actualCreation = $null }
    $creationDelta = $null
    if ($expectedCreation -and $actualCreation) {
        $creationDelta = [Math]::Round([Math]::Abs(($actualCreation - $expectedCreation).TotalSeconds), 3)
    }
    $creationMatch = ($null -ne $creationDelta -and $creationDelta -le 2)
    $actualImage = $null
    try { $actualImage = $process.Path } catch { $actualImage = $null }
    $imageMatch = $null
    if ($actualImage -and $identity.image) {
        try {
            $imageMatch = [string]::Equals(
                [IO.Path]::GetFullPath($actualImage),
                [IO.Path]::GetFullPath([string]$identity.image),
                [StringComparison]::OrdinalIgnoreCase
            )
        } catch {
            $imageMatch = $null
        }
    }
    $scriptMatch = $null
    if ($identity.script) {
        $scriptMatch = [string]::Equals([string]$identity.script, $PSCommandPath, [StringComparison]::OrdinalIgnoreCase)
    }
    $verified = ($creationMatch -and ($imageMatch -eq $true) -and ($scriptMatch -eq $true))
    $record['identity'] = [ordered]@{
        expected_creation_utc = if ($expectedCreation) { $expectedCreation.ToString('o') } else { $null }
        actual_creation_utc   = if ($actualCreation) { $actualCreation.ToString('o') } else { $null }
        creation_delta_seconds = $creationDelta
        creation_match        = $creationMatch
        expected_image        = $identity.image
        actual_image          = $actualImage
        image_match           = $imageMatch
        script_match          = $scriptMatch
    }
    if (-not $verified) {
        $record.stop_mode = 'identity-uncertain'
        $record.identity_verification = 'mismatch'
        $record['collector_gone'] = $false
        Write-Warning 'observer identity does not match the launch record; refusing to signal or kill it'
        return (& $finish 1)
    }
    $record.identity_verification = 'verified'
    $record.owned = $true

    if (-not [IO.File]::Exists($paths.stop_file)) {
        [IO.File]::WriteAllText($paths.stop_file, [DateTime]::UtcNow.ToString('o'), [Text.UTF8Encoding]::new($false))
    }
    $deadline = [DateTime]::UtcNow.AddSeconds($StopTimeoutSeconds)
    $exited = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        $process.Refresh()
        if ($process.HasExited) { $exited = $true; break }
        Start-Sleep -Milliseconds 200
    }
    if ($exited) {
        $record.stop_mode = 'graceful'
        try { $process.WaitForExit(1000) | Out-Null } catch { }
        try {
            $record.exit_code = $process.ExitCode
            $record['exit_code_source'] = 'process'
        } catch {
            $record['exit_code_source'] = 'unavailable'
        }
        $record['collector_gone'] = $process.HasExited
    } else {
        # Re-verify immediately before the only forceful action.
        $reverified = $false
        try {
            $process.Refresh()
            $againCreation = $process.StartTime.ToUniversalTime()
            $againImpl = $process.Path
            $reverified = ((-not $process.HasExited) -and
                ([Math]::Abs(($againCreation - $expectedCreation).TotalSeconds) -le 2) -and
                ([string]::Equals([IO.Path]::GetFullPath($againImpl), [IO.Path]::GetFullPath([string]$identity.image), [StringComparison]::OrdinalIgnoreCase)))
        } catch {
            $reverified = $false
        }
        if (-not $reverified) {
            $record.stop_mode = 'identity-lost-before-force'
            $record['collector_gone'] = $false
            return (& $finish 1)
        }
        $record.stop_mode = 'forced'
        $children = @()
        try {
            $children = @(Get-CimInstance -ClassName Win32_Process -Filter ("ParentProcessId = {0}" -f $expectedPid) -ErrorAction Stop |
                ForEach-Object { [ordered]@{ pid = [int]$_.ProcessId; name = [string]$_.Name } })
        } catch {
        }
        # Recorded only: this diagnostic never kills processes it did not launch itself.
        $record.lingering_children = $children
        try { Stop-Process -Id $expectedPid -Force -ErrorAction Stop } catch { }
        Start-Sleep -Milliseconds 500
        try { $process.Refresh() } catch { }
        $record['collector_gone'] = $process.HasExited
        $record['still_alive'] = (-not $process.HasExited)
    }

    if ($record['collector_gone']) { return (& $finish 0) }
    Write-Warning 'owned observer process is still present after stop'
    return (& $finish 1)
}

# ---------------------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------------------

function Add-FileObservation {
    param($Entry, [string]$Scope, [string]$Name, [string]$Utc, [hashtable]$Extra)
    if (-not $Scope) { $Scope = 'control' }
    $key = "$Scope/$Name"
    if (-not $Entry.files.Contains($key)) {
        $Entry.files[$key] = [ordered]@{
            scope           = $Scope
            name            = $Name
            first_seen_utc  = $Utc
            last_seen_utc   = $Utc
            bytes           = $null
            last_write_utc  = $null
            content         = $null
            content_utc     = $null
            truncated       = $null
            read_error      = $null
            deleted_utc     = $null
        }
    }
    $file = $Entry.files[$key]
    if (-not $file.first_seen_utc -or ($Utc -and $Utc -lt $file.first_seen_utc)) { $file.first_seen_utc = $Utc }
    if (-not $file.last_seen_utc -or ($Utc -and $Utc -gt $file.last_seen_utc)) { $file.last_seen_utc = $Utc }
    if ($Extra) {
        if ($Extra.Contains('bytes') -and $null -ne $Extra['bytes']) { $file.bytes = $Extra['bytes'] }
        if ($Extra.Contains('last_write_utc') -and $Extra['last_write_utc']) { $file.last_write_utc = $Extra['last_write_utc'] }
        if ($Extra.Contains('content') -and $Extra['content']) {
            $content = $Extra['content']
            if ($content.Contains('text') -and $null -ne $content['text']) {
                $file.content = $content['text']
                $file.content_utc = $Utc
                $file.truncated = [bool]$content['truncated']
            }
            if ($content.Contains('error') -and $content['error']) { $file.read_error = $content['error'] }
        }
        if ($Extra.Contains('change') -and $Extra['change'] -eq 'deleted') { $file.deleted_utc = $Utc }
    }
}

function ConvertTo-FileResult {
    param($File)
    return [ordered]@{
        scope          = $File.scope
        name           = $File.name
        first_seen_utc = $File.first_seen_utc
        last_seen_utc  = $File.last_seen_utc
        bytes          = $File.bytes
        last_write_utc = $File.last_write_utc
        content        = $File.content
        content_utc    = $File.content_utc
        truncated      = $File.truncated
        read_error     = $File.read_error
        deleted_utc    = $File.deleted_utc
    }
}

function Get-ControlEntry {
    param($Directories, [string]$Relative, [string]$Utc)
    if (-not $Directories.Contains($Relative)) {
        $Directories[$Relative] = [ordered]@{
            control_relative = $Relative
            first_event_utc  = $Utc
            last_event_utc   = $Utc
            files            = @{}
            processes        = @()
            focus            = $false
            probe            = ($Relative -like "$($script:ProbeDirName)/*")
        }
    }
    $entry = $Directories[$Relative]
    if ($Utc) {
        if (-not $entry.first_event_utc -or $Utc -lt $entry.first_event_utc) { $entry.first_event_utc = $Utc }
        if (-not $entry.last_event_utc -or $Utc -gt $entry.last_event_utc) { $entry.last_event_utc = $Utc }
    }
    return $entry
}

function Get-DiagAnalysis {
    param([string]$DiagnosticRoot, [string]$TempRoot, [string]$EventsPath, [string]$RunExitPath)
    $analysis = [ordered]@{
        schema             = 1
        generated_utc      = [DateTime]::UtcNow.ToString('o')
        events_path        = $EventsPath
        events_lines       = 0
        events_truncated   = $false
        malformed_records  = 0
        record_counts      = [ordered]@{}
        control_dirs       = @()
        processes          = @()
        test_run           = $null
        reproduction       = 'unknown'
        observation_limits = @()
    }
    $records = New-Object 'System.Collections.Generic.List[object]'
    if ([IO.File]::Exists($EventsPath)) {
        $reader = [IO.StreamReader]::new($EventsPath, [Text.UTF8Encoding]::new($false), $false, 65536)
        try {
            while ($null -ne ($line = $reader.ReadLine())) {
                $analysis.events_lines++
                if ($analysis.events_lines -gt 200000) { $analysis.events_truncated = $true; break }
                if ([string]::IsNullOrWhiteSpace($line)) { continue }
                try {
                    $records.Add(($line | ConvertFrom-Json -AsHashtable -ErrorAction Stop))
                } catch {
                    $analysis.malformed_records++
                }
            }
        } finally {
            $reader.Dispose()
        }
    }
    $counts = @{}
    foreach ($record in $records) {
        $source = [string]$record['source']
        if (-not $counts.Contains($source)) { $counts[$source] = 0 }
        $counts[$source]++
    }
    foreach ($key in ($counts.Keys | Sort-Object)) { $analysis.record_counts[$key] = $counts[$key] }

    $directories = @{}
    foreach ($record in $records) {
        $source = [string]$record['source']
        if ($source -eq 'fsw') {
            $path = [string]$record['path']
            $relative = Get-TempRelative -Path $path -TempRoot $TempRoot
            if (-not $relative) { continue }
            $scope = $null
            $baseName = $null
            $fileName = $null
            if ($relative -match '^(?<base>[^/]+)/control/(?<file>[^/]+)$') {
                $scope = 'control'
                $baseName = $Matches['base']
                $fileName = $Matches['file']
            } elseif ($relative -match '^(?<base>[^/]+)/(?<file>[^/]+)$') {
                if ($Matches['file'] -eq 'control') { continue }
                $scope = 'session'
                $baseName = $Matches['base']
                $fileName = $Matches['file']
            } elseif ($relative -match '^(?<base>[^/]+)/(?<project>[^/]+)/(?<file>[^/]+)$') {
                $scope = 'project'
                $baseName = $Matches['base']
                $fileName = $Matches['file']
            } else {
                continue
            }
            $entry = Get-ControlEntry -Directories $directories -Relative "$baseName/control" -Utc (ConvertTo-IsoUtc $record['ts_utc'])
            $change = ([string]$record['event']) -replace '^diag\.fsw\.', ''
            $extra = @{ change = $change }
            if (Test-ContentEligibleName -Name $fileName) {
                $extra['content'] = Read-DiagContent -Path $path -MaxBytes $MaxContentBytes
            }
            Add-FileObservation -Entry $entry -Scope $scope -Name $fileName -Utc (ConvertTo-IsoUtc $record['ts_utc']) -Extra $extra
        } elseif ($source -eq 'snapshot' -or $source -eq 'final-read') {
            $controlRelative = [string]$record['control_relative']
            if (-not $controlRelative) { continue }
            $entry = Get-ControlEntry -Directories $directories -Relative $controlRelative -Utc (ConvertTo-IsoUtc $record['ts_utc'])
            $entry.focus = ($entry.focus -or [bool]$record['focus'])
            foreach ($file in @($record['files'])) {
                if (-not $file) { continue }
                $name = [string]$file['name']
                $scope = [string]$file['scope']
                if (-not $scope) { $scope = 'control' }
                $extra = @{ bytes = $file['bytes']; last_write_utc = $file['last_write_utc'] }
                if ($file.Contains('content') -and $file['content']) { $extra['content'] = $file['content'] }
                Add-FileObservation -Entry $entry -Scope $scope -Name $name -Utc (ConvertTo-IsoUtc $record['ts_utc']) -Extra $extra
            }
        } elseif ($source -eq 'cim-start' -or $source -eq 'poll-start') {
            $pidValue = [int]$record['process_pid']
            if ($pidValue -le 0) { continue }
            $key = [string]$pidValue
            if (-not $analysis['_process_map']) { $analysis['_process_map'] = @{} }
            $map = $analysis['_process_map']
            if (-not $map.Contains($key)) {
                $map[$key] = [ordered]@{
                    pid              = $pidValue
                    name             = [string]$record['name']
                    ppid             = $record['ppid']
                    start_utc        = (ConvertTo-IsoUtc $record['process_start_utc'])
                    exit_utc         = $null
                    exit_status      = $null
                    command_line     = $record['command_line']
                    role             = $record['role']
                    control_relative = $record['control_relative']
                    sources          = @($source)
                }
            } else {
                $map[$key].sources = @($map[$key].sources + $source)
                if (-not $map[$key].command_line -and $record['command_line']) { $map[$key].command_line = $record['command_line'] }
            }
        } elseif ($source -eq 'cim-stop' -or $source -eq 'poll-stop') {
            $pidValue = [int]$record['process_pid']
            $key = [string]$pidValue
            if (-not $analysis['_process_map']) { $analysis['_process_map'] = @{} }
            $map = $analysis['_process_map']
            if ($map.Contains($key)) {
                $processEntry = $map[$key]
                if ($record['process_exit_utc']) { $processEntry.exit_utc = (ConvertTo-IsoUtc $record['process_exit_utc']) }
                if ($null -ne $record['exit_status']) { $processEntry.exit_status = $record['exit_status'] }
                $processEntry.sources = @($processEntry.sources + $source)
            }
        }
    }

    # End-state scan, independent of the observer's lifetime: re-read whatever survived.
    if ([IO.Directory]::Exists($TempRoot)) {
        $scanUtc = [DateTime]::UtcNow.ToString('o')
        foreach ($baseDir in ([IO.Directory]::GetDirectories($TempRoot) | Sort-Object)) {
            $baseName = [IO.Path]::GetFileName($baseDir)
            $scanEntries = @(Get-SessionScanEntries -BaseDir $baseDir)
            if ($scanEntries.Count -eq 0) { continue }
            $entry = Get-ControlEntry -Directories $directories -Relative "$baseName/control" -Utc $scanUtc
            foreach ($scanEntry in $scanEntries) {
                if ($scanEntry.scope -eq 'control-image') { continue }
                $extra = @{ content = (Read-DiagContent -Path $scanEntry.path -MaxBytes $MaxContentBytes) }
                try {
                    $info = [IO.FileInfo]::new($scanEntry.path)
                    if ($info.Exists) {
                        $extra['bytes'] = $info.Length
                        $extra['last_write_utc'] = $info.LastWriteTimeUtc.ToString('o')
                    }
                } catch {
                }
                Add-FileObservation -Entry $entry -Scope $scanEntry.scope -Name $scanEntry.name -Utc $scanUtc -Extra $extra
            }
            $entry.focus = ($entry.focus -or (Test-FocusControl -Files $scanEntries))
        }
    }

    $processMap = $analysis['_process_map']
    if (-not $processMap) { $processMap = @{} }
    $analysis.Remove('_process_map')

    # Attribute fixture processes to a session's control directory using their own args.
    foreach ($key in @($processMap.Keys)) {
        $processEntry = $processMap[$key]
        if (-not $processEntry.name -or $processEntry.name -notmatch '^bend lint') { continue }
        if (-not $processEntry.control_relative -and $processEntry.command_line) {
            $invocation = Get-FixtureInvocation -CommandLine ([string]$processEntry.command_line) -TempRoot $TempRoot
            $processEntry.role = $invocation.role
            $processEntry.control_relative = $invocation.control_relative
        }
        if (-not $processEntry.role -and $processEntry.command_line) {
            $invocation = Get-FixtureInvocation -CommandLine ([string]$processEntry.command_line) -TempRoot $TempRoot
            $processEntry.role = $invocation.role
        }
        if ($processEntry.control_relative -and $directories.Contains($processEntry.control_relative)) {
            $directories[$processEntry.control_relative].processes = @($directories[$processEntry.control_relative].processes + $processEntry)
        } elseif ($processEntry.control_relative) {
            $entry = Get-ControlEntry -Directories $directories -Relative $processEntry.control_relative -Utc $null
            $entry.processes = @($entry.processes + $processEntry)
        }
    }

    $controlResults = @()
    $orderedDirs = @($directories.Values | Sort-Object @{ Expression = { if ($_.focus) { 0 } else { 1 } } }, @{ Expression = { $_.first_event_utc } })
    $dirCount = 0
    foreach ($entry in $orderedDirs) {
        if ($entry.probe) { continue }
        if ($dirCount -ge 64) { break }
        $dirCount++
        $files = @()
        $controlNames = @('calls', 'cwd', 'arguments', 'address', 'address.pending', 'descendant-address.pending', 'descendant-address', 'started', 'exit-code', 'block', 'release', 'spawn-descendant', 'compiler.stderr', 'server.stderr')
        $emitted = @{}
        foreach ($name in $controlNames) {
            $key = "control/$name"
            if (-not $entry.files.Contains($key)) { continue }
            $emitted[$key] = $true
            $files += (ConvertTo-FileResult -File $entry.files[$key])
        }
        foreach ($key in @($entry.files.Keys | Sort-Object)) {
            if ($files.Count -ge 48) { break }
            if ($emitted.Contains($key)) { continue }
            $file = $entry.files[$key]
            if ($file.scope -eq 'control-image') { continue }
            $emitted[$key] = $true
            $files += (ConvertTo-FileResult -File $file)
        }
        $omittedFiles = [Math]::Max(0, $entry.files.Count - $files.Count)
        $lintProcesses = @($entry.processes | Where-Object { $_.role -eq 'lint' })
        $descendantProcesses = @($entry.processes | Where-Object { $_.role -eq 'descendant' })
        $hasCalls = $entry.files.Contains('control/calls')
        $hasAddress = $entry.files.Contains('control/address')
        $hasPending = $entry.files.Contains('control/address.pending')
        $hasDescendantAddress = $entry.files.Contains('control/descendant-address')
        $firstWriteUtc = $null
        foreach ($name in @('control/calls', 'control/cwd', 'control/arguments', 'control/address')) {
            if ($entry.files.Contains($name)) {
                $candidate = $entry.files[$name].first_seen_utc
                if ($candidate -and (-not $firstWriteUtc -or $candidate -lt $firstWriteUtc)) { $firstWriteUtc = $candidate }
            }
        }
        $lintExitUtc = $null
        foreach ($processEntry in $lintProcesses) {
            if ($processEntry.exit_utc -and (-not $lintExitUtc -or $processEntry.exit_utc -lt $lintExitUtc)) { $lintExitUtc = $processEntry.exit_utc }
        }
        $controlResults += [ordered]@{
            control_relative        = $entry.control_relative
            probe                   = $entry.probe
            focus                   = $entry.focus
            first_event_utc         = $entry.first_event_utc
            last_event_utc          = $entry.last_event_utc
            files                   = $files
            omitted_files           = $omittedFiles
            address_published       = $hasAddress
            invocation_recorded     = $hasCalls
            pending_without_address = ($hasPending -and -not $hasAddress)
            descendant_address_published = $hasDescendantAddress
            lint_processes          = @($lintProcesses | Select-Object -First 8)
            descendant_processes    = @($descendantProcesses | Select-Object -First 4)
            lint_process_count      = $lintProcesses.Count
            descendant_process_count = $descendantProcesses.Count
            first_control_write_utc = $firstWriteUtc
            earliest_lint_exit_utc  = $lintExitUtc
            lint_exit_before_first_control_write = (($lintExitUtc -and $firstWriteUtc) -and ($lintExitUtc -lt $firstWriteUtc))
        }
    }
    $analysis.control_dirs = $controlResults

    $processResults = @()
    foreach ($key in @($processMap.Keys)) {
        if ($processResults.Count -ge 400) { break }
        $processEntry = $processMap[$key]
        $processResults += [ordered]@{
            pid              = $processEntry.pid
            name             = $processEntry.name
            ppid             = $processEntry.ppid
            role             = $processEntry.role
            control_relative = $processEntry.control_relative
            start_utc        = $processEntry.start_utc
            exit_utc         = $processEntry.exit_utc
            exit_status      = $processEntry.exit_status
            command_line     = $processEntry.command_line
            sources          = $processEntry.sources
        }
    }
    $analysis.processes = $processResults

    $runExit = Read-JsonFile -Path $RunExitPath
    $analysis.test_run = $runExit
    if ($runExit) {
        if ($runExit['aborted'] -or -not $runExit['test_started']) {
            $analysis.reproduction = 'not_run'
        } elseif ([int]$runExit['exit_code'] -eq 0) {
            $analysis.reproduction = 'not_reproduced'
        } elseif (@($runExit['failed_tests']) -contains $script:TestName) {
            $analysis.reproduction = 'reproduced'
        } else {
            $analysis.reproduction = 'failed_differently'
        }
        $testBinaryStarts = @($processMap.Values | Where-Object { $_.name -match '^bend_lint_proto' })
        $analysis['test_binary_starts_observed'] = $testBinaryStarts.Count
    }
    if ($analysis.events_truncated) {
        $analysis.observation_limits += 'The event log exceeded 200000 lines and analysis stopped early; records after that point are in events.jsonl but not summarized.'
    }
    if ($analysis.malformed_records -gt 0) {
        $analysis.observation_limits += ("{0} event lines could not be parsed; the raw file is preserved." -f $analysis.malformed_records)
    }
    $analysis.observation_limits += $script:Limits
    return $analysis
}

function Format-StepSummary {
    param($Analysis, $Substitution = $null)
    $lines = New-Object 'System.Collections.Generic.List[string]'
    $lines.Add('## Windows x64 saved-lint startup diagnostic (investigation only)')
    $lines.Add('')
    $lines.Add('This job observes the `bend_lint_protocol` tests. It does **not** replace, supersede, or reinterpret the quality workflow result.')
    if ($Substitution) {
        $mode = if ($Substitution['changed']) { 'instrumented producer copy' } else { 'original C7 bytes' }
        $lines.Add(('- Test source: {0}; C7 `{1}` -> diagnostic `{2}`; diff +{3}/-{4}' -f $mode, ([string]$Substitution['original']['sha256']).Substring(0, 12), ([string]$Substitution['instrumented']['sha256']).Substring(0, 12), $Substitution['diff']['added_lines'], $Substitution['diff']['removed_lines']))
        $lines.Add('- Not byte-identical-test qualification: cases, assertions, default concurrency and the 500 ms timeout are unchanged, but the test file bytes are not.')
    }
    $testRun = $Analysis.test_run
    if ($testRun) {
        $lines.Add('')
        $lines.Add(('- Original command: `{0}`' -f $testRun['command']))
        $lines.Add(('- Original exit code (preserved): `{0}`' -f $testRun['exit_code']))
        $lines.Add(('- Tests observed: `{0}` (expected {1})' -f $testRun['observed_tests'], $testRun['expected_tests']))
        if ($testRun['result_line']) { $lines.Add(('- Result line: `{0}`' -f $testRun['result_line'])) }
        if (@($testRun['failed_tests']).Count -gt 0) { $lines.Add(('- Failed: `{0}`' -f (@($testRun['failed_tests']) -join '`, `'))) }
    } else {
        $lines.Add('')
        $lines.Add('- No original test run was recorded (build failure or aborted diagnostic).')
    }
    $lines.Add(('- Reproduction: `{0}`' -f $Analysis.reproduction))
    if ($Analysis.record_counts) {
        $counts = ($Analysis.record_counts.Keys | Sort-Object | ForEach-Object { '{0}={1}' -f $_, $Analysis.record_counts[$_] }) -join ', '
        $lines.Add(('- Observer records: `{0}`' -f $counts))
    }
    $focus = @($Analysis.control_dirs | Where-Object { $_.focus })
    if ($focus.Count -gt 0) {
        $lines.Add('')
        $lines.Add('### 500 ms descendant sessions')
        foreach ($entry in $focus) {
            $address = if ($entry.address_published) { 'published' } else { 'not published' }
            $lines.Add(('- `{0}`: address {1}; invocation record {2}; lint processes {3}; descendant processes {4}; descendant address {5}' -f $entry.control_relative, $address, $entry.invocation_recorded, $entry.lint_process_count, $entry.descendant_process_count, $entry.descendant_address_published))
            foreach ($processEntry in @($entry.lint_processes)) {
                $lines.Add(('  - lint pid {0}: start `{1}`, exit `{2}`, status `{3}`' -f $processEntry.pid, $processEntry.start_utc, $processEntry.exit_utc, $processEntry.exit_status))
            }
        }
    }
    if ($testRun -and @($testRun['wait_file_diagnostics']).Count -gt 0) {
        $lines.Add('')
        $lines.Add('### wait_file diagnostics (instrumented test)')
        foreach ($diagnostic in @($testRun['wait_file_diagnostics'])) {
            $lines.Add(('- missing `{0}` in `{1}`; server exit `{2}`; messages {3}; files {4}' -f $diagnostic['missing'], $diagnostic['control_relative'], $diagnostic['server_exit_status'], @($diagnostic['messages']).Count, @($diagnostic['files'].Keys).Count))
            foreach ($message in (@($diagnostic['messages']) | Select-Object -First 6)) {
                $lines.Add(('  - message (+{0} ms): `{1}`' -f $message.received_ago_ms, $message.text))
            }
            foreach ($name in @($diagnostic['files'].Keys)) {
                $observation = $diagnostic['files'][$name]
                $summaryText = if ($observation['open_error']) { 'open_error: ' + $observation['open_error'] } else { ([string]$observation['text']).Trim() }
                if ($summaryText.Length -gt 200) { $summaryText = $summaryText.Substring(0, 200) + '...' }
                $lines.Add(('  - file `{0}`: {1}' -f $name, $summaryText))
            }
        }
    }
    $lines.Add('')
    $lines.Add('Full record: `diagnostic-summary.json`, `control-timelines.json`, `observe/events.jsonl`, `run-exit.json`, `collector-lifecycle.json`, `instrumented-test.diff`.')
    $lines.Add('The original session pipes analyzer stderr to `Stdio::null`; only the instrumented copy may capture it, and nothing is invented.')
    return ($lines -join "`n")
}

# ---------------------------------------------------------------------------------------
# finalize
# ---------------------------------------------------------------------------------------

function Invoke-Finalize {
    if (-not $Root) { throw '-Root is required' }
    $paths = Get-DiagPaths -DiagnosticRoot $Root
    $errors = New-Object 'System.Collections.Generic.List[string]'
    $before = Read-JsonFile -Path $paths.before
    $substitution = Read-JsonFile -Path $paths.substitution
    $after = $null
    $comparison = $null
    try {
        if ($Sut) {
            $sutPath = Get-NormalizedPath -Path $Sut
            $producerPath = if ($Producer) { Get-NormalizedPath -Path $Producer } else { $null }
            $after = Get-SourceIdentity -Repository $sutPath -ScriptPath $PSCommandPath -ProducerRepository $producerPath
            Write-JsonFile -Path $paths.after -Object $after
            $expectedInstrumented = $null
            $originalTest = $null
            if ($substitution) {
                $expectedInstrumented = [string]$substitution.instrumented.sha256
                $originalTest = [string]$substitution.original.sha256
            }
            $comparison = Compare-Identity -Before $before -After $after -ExpectedInstrumentedSha256 $expectedInstrumented -OriginalTestSha256 $originalTest
        }
    } catch {
        $errors.Add(('identity-after: {0}' -f $_.Exception.Message))
    }
    $analysis = $null
    try {
        $analysis = Get-DiagAnalysis -DiagnosticRoot $paths.root -TempRoot $paths.temp -EventsPath $paths.events -RunExitPath $paths.run_exit
    } catch {
        $errors.Add(('analysis: {0}' -f $_.Exception.Message))
        Write-TextFile -Path $paths.analyze_fails -Text (($_ | Out-String) + "`n")
    }
    $context = Read-JsonFile -Path $paths.context
    $readiness = Read-JsonFile -Path $paths.readiness
    $lifecycle = Read-JsonFile -Path $paths.lifecycle
    $collectorSummary = Read-JsonFile -Path $paths.summary
    $plan = Read-JsonFile -Path $paths.run_plan
    $build = Read-JsonFile -Path $paths.build_json
    $testRun = if ($analysis) { $analysis.test_run } else { Read-JsonFile -Path $paths.run_exit }
    $summary = [ordered]@{
        schema                   = 1
        kind                     = 'windows-lint-startup-diagnostic'
        diagnostic_only          = $true
        original_quality_verdict = 'unchanged'
        byte_identical_test_qualification = $false
        statement                = 'Evidence only. The original quality workflow result, PR verdict, and test policy are unchanged by this run; no retry or rerun-to-pass was performed. The test file was the diagnostic producer copy, so this run is not byte-identical-test qualification.'
        generated_utc            = [DateTime]::UtcNow.ToString('o')
        run_context              = $context
        run_plan                 = $plan
        test_source_substitution = $substitution
        identities               = [ordered]@{ before = $before; after = $after; comparison = $comparison }
        observer = [ordered]@{
            readiness = $readiness
            lifecycle = $lifecycle
            summary   = $collectorSummary
        }
        build                    = $build
        test_run                 = $testRun
        failure_highlights       = if ($testRun) { @($testRun['failure_highlights']) } else { @() }
        reproduction             = if ($analysis) { $analysis.reproduction } else { 'unknown' }
        control_dirs             = if ($analysis) { $analysis.control_dirs } else { @() }
        processes                = if ($analysis) { $analysis.processes } else { @() }
        observation_limits       = if ($analysis) { $analysis.observation_limits } else { $script:Limits }
        analysis_record_counts   = if ($analysis) { $analysis.record_counts } else { $null }
        malformed_event_records  = if ($analysis) { $analysis.malformed_records } else { $null }
        missed_observations      = [ordered]@{
            cim_start_subscribed     = if ($collectorSummary) { $collectorSummary.cim.start_subscribed } else { $null }
            cim_stop_subscribed      = if ($collectorSummary) { $collectorSummary.cim.stop_subscribed } else { $null }
            cim_error                = if ($collectorSummary) { $collectorSummary.cim.error } elseif ($readiness) { $readiness.cim.error } else { $null }
            cim_filtered_events      = if ($collectorSummary) { $collectorSummary.cim.filtered_events } else { $null }
            poll_interval_ms         = 400
            poll_samples             = if ($collectorSummary) { $collectorSummary.poll.samples } else { $null }
            main_records_dropped     = if ($collectorSummary) { $collectorSummary.main_records_dropped } else { $null }
            snapshot_limit_reached   = if ($collectorSummary) { $collectorSummary.snapshot_limit_reached } else { $null }
            event_byte_limit_reached = if ($collectorSummary) { $collectorSummary.event_byte_limit_reached } else { $null }
            file_watcher_error_records = if ($analysis -and $analysis.record_counts['fsw-error']) { $analysis.record_counts['fsw-error'] } else { 0 }
            malformed_event_records  = if ($analysis) { $analysis.malformed_records } else { $null }
            events_truncated         = if ($analysis) { $analysis.events_truncated } else { $null }
            callback_records         = if ($collectorSummary) { $collectorSummary.callback.records } else { $null }
            callback_dropped_records = if ($collectorSummary) { $collectorSummary.callback.dropped_records } else { $null }
            callback_dropped_bytes   = if ($collectorSummary) { $collectorSummary.callback.dropped_bytes } else { $null }
            callback_overflow        = if ($collectorSummary) { $collectorSummary.callback.overflow } else { $null }
            explanation = 'When the CIM subscriptions are unavailable, process history comes only from 400 ms Get-Process sampling and can miss a sub-500 ms fixture process. File publication and the instrumented wait_file diagnostics do not depend on process sampling. Watcher callbacks stop at a hard record/byte budget; the dropped counts above are authoritative for that bound.'
        }
        test_binary_starts_observed = if ($analysis) { $analysis['test_binary_starts_observed'] } else { $null }
        interpretation           = [ordered]@{
            decisive_evidence = 'wait_file_diagnostics emitted by the instrumented test (timed LSP messages and bounded control/server.stderr snapshots) plus the raw cargo exit code. Per-session booleans (invocation_recorded, address_published, pending_without_address, descendant_address_published) are raw observations, not cause assignments.'
            cannot_conclude = @(
                'A fixture process that both CIM and 400 ms sampling missed may be absent from the process list; control files and instrumented messages remain authoritative.',
                'This diagnostic does not attribute the original Windows failure to a cause and does not reproduce it by construction.',
                'This run used the instrumented producer test file, so it is not byte-identical-test qualification and proves nothing about the original C7 test bytes beyond their recorded hash.'
            )
        }
        errors                   = $errors
    }
    try {
        Write-JsonFile -Path $paths.diagnostics -Object $summary
    } catch {
        $errors.Add(('summary: {0}' -f $_.Exception.Message))
        Write-TextFile -Path $paths.diagnostics -Text ((@{ schema = 1; diagnostic_only = $true; errors = @($errors); statement = 'summary serialization failed; raw evidence is preserved' } | ConvertTo-Json -Depth 4) + "`n")
    }
    try {
        $timelines = [ordered]@{
            schema       = 1
            generated_utc = [DateTime]::UtcNow.ToString('o')
            test_run     = if ($analysis) { $analysis.test_run } else { $null }
            reproduction = if ($analysis) { $analysis.reproduction } else { 'unknown' }
            control_dirs = if ($analysis) { $analysis.control_dirs } else { @() }
        }
        Write-JsonFile -Path $paths.timelines -Object $timelines
    } catch {
        $errors.Add(('timelines: {0}' -f $_.Exception.Message))
    }
    if ($env:GITHUB_STEP_SUMMARY) {
        $markdown = if ($analysis) { Format-StepSummary -Analysis $analysis -Substitution $substitution } else { '## Windows x64 saved-lint startup diagnostic`n`nAnalysis failed; raw evidence is in the uploaded artifact.' }
        Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $markdown
    }
    foreach ($message in $errors) { Write-Warning $message }

    # Enforce provenance only after every artifact is on disk. The original cargo exit code
    # is already recorded in run-exit.json and the job's run step; this final gate fails the
    # diagnostic when the tracked source/overlay identity is missing, empty, or changed.
    $identityOk = $true
    $reasons = New-Object 'System.Collections.Generic.List[string]'
    if (-not $before) { $identityOk = $false; $reasons.Add('source identity before is missing') }
    if (-not $after) { $identityOk = $false; $reasons.Add('source identity after is missing or failed to record') }
    if (-not $comparison -or -not $comparison.unchanged) { $identityOk = $false; $reasons.Add('tracked source/overlay identity changed or is unverifiable') }
    if ($comparison -and -not $comparison.instrumented_test_intact) { $identityOk = $false; $reasons.Add('instrumented test digest does not match the pinned value') }
    if ($before -and ($before.digest.combined -notmatch '^[0-9a-f]{64}$' -or $before.digest.covered_count -le 0)) { $identityOk = $false; $reasons.Add('before identity digest is empty or covers no tracked file') }
    if ($after -and ($after.digest.combined -notmatch '^[0-9a-f]{64}$' -or $after.digest.covered_count -le 0)) { $identityOk = $false; $reasons.Add('after identity digest is empty or covers no tracked file') }
    if ($ExpectedTestSha256 -and $substitution -and ([string]$substitution['expected_test_sha256']) -ne $ExpectedTestSha256.ToLowerInvariant()) {
        $identityOk = $false
        $reasons.Add('pinned test digest does not match the substitution record')
    }
    $enforcement = [ordered]@{
        schema        = 1
        generated_utc = [DateTime]::UtcNow.ToString('o')
        enforced      = $identityOk
        reasons       = @($reasons)
        compared      = [ordered]@{
            head_before                 = if ($before) { $before.head } else { $null }
            head_after                  = if ($after) { $after.head } else { $null }
            digest_before               = if ($before) { $before.digest.combined } else { $null }
            digest_after                = if ($after) { $after.digest.combined } else { $null }
            covered_count_before        = if ($before) { $before.digest.covered_count } else { $null }
            covered_count_after         = if ($after) { $after.digest.covered_count } else { $null }
            inventory_sha256_before     = if ($before) { $before.digest.inventory_sha256 } else { $null }
            inventory_sha256_after      = if ($after) { $after.digest.inventory_sha256 } else { $null }
            instrumented_test_sha256    = if ($comparison) { $comparison.instrumented_test_sha256 } else { $null }
            instrumented_test_expected  = if ($comparison) { $comparison.instrumented_test_expected } else { $null }
            other_tracked_modifications = if ($comparison) { @($comparison.other_tracked_modifications) } else { @() }
        }
        artifacts     = @('diagnostic-summary.json', 'control-timelines.json', 'source-identity-after.json', 'run-exit.json', 'collector-lifecycle.json')
    }
    Write-JsonFile -Path $paths.enforcement -Object $enforcement
    if (-not $identityOk) {
        foreach ($reason in $reasons) { Write-Warning ("identity enforcement: {0}" -f $reason) }
        return 1
    }
    return 0
}

# ---------------------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------------------

switch ($Phase) {
    'prepare-root' { Invoke-PrepareRoot; exit 0 }
    'prepare' { Invoke-Prepare; exit 0 }
    'start' { Invoke-Start; exit 0 }
    'build' { Invoke-Build }
    'run' { Invoke-Run }
    'stop' { $code = Invoke-Stop; exit $code }
    'finalize' { $code = Invoke-Finalize; exit $code }
    'collect' { $code = Invoke-Collect; exit $code }
}
