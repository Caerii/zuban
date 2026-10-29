param(
    [Parameter(Mandatory = $true)]
    [string]$BlazePath,

    [Parameter(Mandatory = $true)]
    [string]$NativeProcMacroServer,

    [string]$ClangPath
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).ProviderPath
$blaze = (Resolve-Path -LiteralPath $BlazePath).ProviderPath
$nativeServer = (Resolve-Path -LiteralPath $NativeProcMacroServer).ProviderPath
$clangArgs = if ($ClangPath) {
    @{ ClangPath = (Resolve-Path -LiteralPath $ClangPath -ErrorAction Stop).ProviderPath }
} else {
    @{}
}
$wrapper = Join-Path $repo 'scripts\blaze-dev.ps1'
$source = Join-Path $repo 'crates\zuban_python\src\lib.rs'
$cargoExe = Join-Path $repo 'target\debug\zuban.exe'
$blazeExe = Join-Path $repo 'target\blaze-dev\debug\zuban.exe'
$cargoArgs = @('--locked', '-p', 'zuban', '--bin', 'zuban')

if (-not (Test-Path -LiteralPath $wrapper -PathType Leaf)) { throw "Missing wrapper: $wrapper" }
if (-not (Test-Path -LiteralPath $blazeExe -PathType Leaf)) {
    throw "Blaze baseline binary is missing; verify a successful Blaze build before comparing: $blazeExe"
}
$sourceStatus = & git -C $repo status --porcelain -- 'crates/zuban_python/src/lib.rs'
if ($LASTEXITCODE -ne 0) { throw 'Could not verify source status; comparison aborted' }
if ($sourceStatus) {
    throw "Source file has uncommitted changes: $source"
}

$runDir = Join-Path $repo ('target\blaze-evaluation\' + (Get-Date).ToString('yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($runDir) | Out-Null
$sample = Join-Path $runDir 'sample.py'
[IO.File]::WriteAllText($sample, "value: int = 1`n", [Text.UTF8Encoding]::new($false))
Write-Output "Evaluation artifacts: $runDir"

# Latin-1 is byte-preserving for an ASCII-only replacement, including CRLF and BOM bytes.
$encoding = [Text.Encoding]::GetEncoding(28591)
$original = [IO.File]::ReadAllBytes($source)
$old = 'Success: no issues found in {checked} source file{checked_s}'
$new = 'SUCCESS: no issues found in {checked} source file{checked_s}'
$sourceText = $encoding.GetString($original)
if ($sourceText.Split(@($old), [StringSplitOptions]::None).Count -ne 2) {
    throw 'Expected exactly one success-format literal in zuban_python/src/lib.rs'
}
$alternate = $encoding.GetBytes($sourceText.Replace($old, $new))
$script:lastWritten = $original
$backup = Join-Path $runDir 'source-original.bin'
$backupStream = [IO.File]::Open($backup, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $backupStream.Write($original, 0, $original.Length)
    $backupStream.Flush($true)
}
finally {
    $backupStream.Dispose()
}
Write-Output "Original source byte backup: $backup"

function Assert-OwnedSource {
    $current = [IO.File]::ReadAllBytes($source)
    if (-not [Linq.Enumerable]::SequenceEqual([byte[]]$current, [byte[]]$script:lastWritten)) {
        throw "Source changed outside this benchmark; leaving it untouched: $source"
    }
}

function Set-Variant([string]$variant) {
    Assert-OwnedSource
    $wanted = if ($variant -ceq 'Success') { $original } else { $alternate }
    if (-not [Linq.Enumerable]::SequenceEqual([byte[]]$wanted, [byte[]]$script:lastWritten)) {
        [IO.File]::WriteAllBytes($source, $wanted)
        $script:lastWritten = $wanted
    }
}

function Get-StackReserve([string]$exe) {
    $stream = [IO.File]::OpenRead($exe)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        $stream.Position = 0x3c
        $peOffset = $reader.ReadInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "Invalid PE signature: $exe" }
        $stream.Position = $peOffset + 24
        if ($reader.ReadUInt16() -ne 0x20b) { throw "Expected PE32+ executable: $exe" }
        $stream.Position = $peOffset + 24 + 72
        return $reader.ReadUInt64()
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

function Invoke-Build([string]$tool, [string]$variant, [string]$phase) {
    $variantName = if ($variant -ceq 'SUCCESS') { 'upper' } else { 'normal' }
    $direction = if ($phase -eq 'warm') { 'baseline' } elseif ($variant -ceq 'SUCCESS') { 'edit' } else { 'revert' }
    $name = "$phase-$tool-$variantName".ToLowerInvariant()
    $log = Join-Path $runDir "$name.log"
    $watch = [Diagnostics.Stopwatch]::StartNew()
    Set-Variant $variant
    if ($tool -eq 'Cargo') {
        & cargo +1.95.0 build @cargoArgs *> $log
    } else {
        & $wrapper -BlazePath $blaze -NativeProcMacroServer $nativeServer @clangArgs -Command Build -CargoArgs $cargoArgs *> $log
    }
    $code = $LASTEXITCODE
    $watch.Stop()
    if ($code -ne 0) {
        throw "$tool build failed with exit $code; see $log"
    }
    Assert-OwnedSource

    $exe = if ($tool -eq 'Cargo') { $cargoExe } else { $blazeExe }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Missing built binary: $exe" }
    $stackReserve = Get-StackReserve $exe
    if ($stackReserve -ne 16000000) {
        throw "$tool binary has stack reserve $stackReserve; expected 16000000: $exe"
    }
    $output = (& $exe check $sample 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0 -or -not $output.Contains("${variant}: no issues found")) {
        throw "$tool binary did not execute expected $variant code; output: $output"
    }
    [pscustomobject]@{ Phase = $phase; Tool = $tool; Direction = $direction; Variant = $variant; Seconds = [Math]::Round($watch.Elapsed.TotalSeconds, 3); StackReserve = $stackReserve; Log = $log }
}

function Record-Build([string]$tool, [string]$variant, [string]$phase) {
    $script:results += Invoke-Build $tool $variant $phase
    $script:results | Export-Csv -LiteralPath (Join-Path $runDir 'compare-results.csv') -NoTypeInformation
}

$process = [Diagnostics.Process]::GetCurrentProcess()
$previousAffinity = $process.ProcessorAffinity
$mask = $previousAffinity.ToInt64()
$oneCpu = 1L
while (($mask -band $oneCpu) -eq 0) { $oneCpu = $oneCpu -shl 1 }
$savedEnvironment = @{}
foreach ($name in @('CARGO_BUILD_JOBS', 'RAYON_NUM_THREADS', 'CARGO_TARGET_DIR', 'ZUBAN_TYPESHED')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$changedAffinity = $false
$pushedLocation = $false
$results = @()
try {
    $process.ProcessorAffinity = [IntPtr]::new($oneCpu)
    $changedAffinity = $true
    Push-Location -LiteralPath $repo
    $pushedLocation = $true
    [Environment]::SetEnvironmentVariable('CARGO_BUILD_JOBS', '1', 'Process')
    [Environment]::SetEnvironmentVariable('RAYON_NUM_THREADS', '1', 'Process')
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', (Join-Path $repo 'target'), 'Process')
    [Environment]::SetEnvironmentVariable('ZUBAN_TYPESHED', (Join-Path $repo 'third_party\typeshed'), 'Process')

    & $wrapper -BlazePath $blaze -Command Stop *> (Join-Path $runDir 'before-cargo-blaze-stop.log')
    if ($LASTEXITCODE -ne 0) { throw 'Could not stop Blaze before Cargo phase' }
    Record-Build Cargo Success warm
    Record-Build Cargo SUCCESS measured
    Record-Build Cargo Success measured
    # On Windows Client::nobody() is None, so own-base does not start in the
    # background; the Blaze warm build returns after its synchronous work.
    Record-Build Blaze Success warm
    Record-Build Blaze SUCCESS measured
    Record-Build Blaze Success measured
    $results | Format-Table -AutoSize
}
finally {
    $current = [IO.File]::ReadAllBytes($source)
    if ([Linq.Enumerable]::SequenceEqual([byte[]]$current, [byte[]]$script:lastWritten)) {
        if (-not [Linq.Enumerable]::SequenceEqual([byte[]]$current, [byte[]]$original)) {
            [IO.File]::WriteAllBytes($source, $original)
        }
    } else {
        Write-Warning "Concurrent source change detected; did not overwrite $source"
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    if ($pushedLocation) { Pop-Location }
    if ($changedAffinity) { $process.ProcessorAffinity = $previousAffinity }
    $process.Dispose()
}
