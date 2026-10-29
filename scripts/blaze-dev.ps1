# Opt-in Blaze entry point. Use a Blaze build that honors CARGO_BUILD_JOBS;
# the current upstream artifact does not cap its own workers with that setting.
# Single-worker behavior must be verified for the supplied executable.
# Example: .\scripts\blaze-dev.ps1 -BlazePath C:\path\blaze.exe -NativeProcMacroServer C:\path\server.exe -Command Build -CargoArgs @('--locked', '-p', 'zuban')
param(
    [Parameter(Mandatory = $true)]
    [string]$BlazePath,

    [string]$NativeProcMacroServer,

    [string]$ClangPath,

    [ValidateSet('Build', 'Run', 'Status', 'Stop')]
    [string]$Command = 'Build',

    [string[]]$CargoArgs = @()
)

$ErrorActionPreference = 'Stop'

$blaze = (Resolve-Path -LiteralPath $BlazePath -ErrorAction Stop).ProviderPath
if (-not (Test-Path -LiteralPath $blaze -PathType Leaf)) {
    throw "Blaze executable is not a file: $BlazePath"
}
if ($Command -in @('Status', 'Stop') -and $CargoArgs.Count -ne 0) {
    throw "$Command does not accept Cargo arguments"
}
if ($Command -in @('Build', 'Run')) {
    if (-not $NativeProcMacroServer) {
        throw "$Command requires -NativeProcMacroServer"
    }
    $server = (Resolve-Path -LiteralPath $NativeProcMacroServer -ErrorAction Stop).ProviderPath
    if (-not (Test-Path -LiteralPath $server -PathType Leaf)) {
        throw "Native proc-macro server is not a file: $NativeProcMacroServer"
    }
    if ($ClangPath) {
        $clangCandidate = $ClangPath
    } else {
        $foundClang = Get-Command clang.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($foundClang) {
            $clangCandidate = $foundClang.Source
        } elseif ($env:ProgramFiles) {
            $clangCandidate = Join-Path $env:ProgramFiles 'LLVM\bin\clang.exe'
        }
    }
    if (-not $clangCandidate -or -not (Test-Path -LiteralPath $clangCandidate -PathType Leaf)) {
        throw 'Build/Run requires clang.exe; pass -ClangPath or install it under Program Files\LLVM\bin'
    }
    $clang = (Resolve-Path -LiteralPath $clangCandidate -ErrorAction Stop).ProviderPath
    if ([IO.Path]::GetFileName($clang) -ine 'clang.exe') {
        throw '-ClangPath must name clang.exe; Blaze discovers that filename on PATH'
    }
} elseif ($NativeProcMacroServer) {
    throw "$Command does not use -NativeProcMacroServer"
} elseif ($ClangPath) {
    throw "$Command does not use -ClangPath"
}

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).ProviderPath
$process = [Diagnostics.Process]::GetCurrentProcess()
$previousAffinity = $process.ProcessorAffinity
$allowedMask = $previousAffinity.ToInt64()
$singleCpuMask = 1L
while (($allowedMask -band $singleCpuMask) -eq 0) {
    $singleCpuMask = $singleCpuMask -shl 1
}
$previous = @{}
foreach ($name in @('CARGO_BUILD_JOBS', 'RAYON_NUM_THREADS', 'RUST_MIN_STACK', 'BLAZE_SANDBOX', 'BLAZE_PROC_MACRO_SERVER', 'BLAZE_NATIVE_PROC_MACRO_SERVER', 'BLAZE_HOME', 'CARGO_TARGET_DIR', 'ZUBAN_TYPESHED', 'PATH')) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

$pushedLocation = $false
$affinityChanged = $false
try {
    # Child builds inherit one allowed logical CPU; the patched Blaze worker cap is separate.
    $process.ProcessorAffinity = [IntPtr]::new($singleCpuMask)
    $affinityChanged = $true
    Push-Location -LiteralPath $repoRoot
    $pushedLocation = $true
    [Environment]::SetEnvironmentVariable('CARGO_BUILD_JOBS', '1', 'Process')
    [Environment]::SetEnvironmentVariable('RAYON_NUM_THREADS', '1', 'Process')
    [Environment]::SetEnvironmentVariable('RUST_MIN_STACK', '16777216', 'Process')
    [Environment]::SetEnvironmentVariable('BLAZE_SANDBOX', 'off', 'Process')
    # This Windows trial uses explicit native proc-macro server path and keeps
    # Blaze's own compile-time scheduler off while building the main program.
    [Environment]::SetEnvironmentVariable('BLAZE_PROC_MACRO_SERVER', '0', 'Process')
    if ($Command -in @('Build', 'Run')) {
        [Environment]::SetEnvironmentVariable('BLAZE_NATIVE_PROC_MACRO_SERVER', $server, 'Process')
        $clangDir = Split-Path -Path $clang -Parent
        $scopedPath = if ($previous['PATH']) { "$clangDir;$($previous['PATH'])" } else { $clangDir }
        [Environment]::SetEnvironmentVariable('PATH', $scopedPath, 'Process')
    }
    [Environment]::SetEnvironmentVariable('BLAZE_HOME', (Join-Path $repoRoot 'target\blaze-home'), 'Process')
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', (Join-Path $repoRoot 'target\blaze-dev'), 'Process')
    if ($Command -eq 'Run' -and -not $previous['ZUBAN_TYPESHED']) {
        [Environment]::SetEnvironmentVariable('ZUBAN_TYPESHED', (Join-Path $repoRoot 'third_party\typeshed'), 'Process')
    }

    & $blaze $Command.ToLowerInvariant() @CargoArgs
    $result = $LASTEXITCODE
}
finally {
    if ($affinityChanged) {
        $process.ProcessorAffinity = $previousAffinity
    }
    $process.Dispose()
    foreach ($name in $previous.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
    if ($pushedLocation) {
        Pop-Location
    }
}

exit $result
