param(
    [string]$CargoExe,
    [string]$BlazeExe
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).ProviderPath
if (-not $CargoExe) { $CargoExe = Join-Path $repo 'target\debug\zuban.exe' }
if (-not $BlazeExe) { $BlazeExe = Join-Path $repo 'target\blaze-dev\debug\zuban.exe' }
$CargoExe = (Resolve-Path -LiteralPath $CargoExe).ProviderPath
$BlazeExe = (Resolve-Path -LiteralPath $BlazeExe).ProviderPath
if (-not (Test-Path -LiteralPath $CargoExe -PathType Leaf)) { throw "Missing Cargo binary: $CargoExe" }
if (-not (Test-Path -LiteralPath $BlazeExe -PathType Leaf)) { throw "Missing Blaze binary: $BlazeExe" }

$runDir = Join-Path $repo ('target\blaze-evaluation\' + (Get-Date).ToString('yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N'))
$casesDir = Join-Path $runDir 'diagnostic-cases'
[IO.Directory]::CreateDirectory($casesDir) | Out-Null
Write-Output "Diagnostic artifacts: $runDir"
$cases = @(
    @{ Name = 'valid'; Exit = 0; Code = "value: int = 1`n" },
    @{ Name = 'assignment'; Exit = 1; Code = "value: int = 'wrong'`n" },
    @{ Name = 'argument'; Exit = 1; Code = "def expects(value: int) -> int:`n    return value`nresult = expects('wrong')`n" },
    @{ Name = 'typeshed_import'; Exit = 0; Code = "from collections.abc import Iterable`nvalues: Iterable[int] = [1, 2]`n" }
)

function Normalize-Diagnostics([string]$text) {
    $text = [regex]::Replace($text, "`e\[[0-9;]*[A-Za-z]", '')
    return ($text -replace "`r`n?", "`n").Trim()
}

function Check-File([string]$exe, [string]$path) {
    $output = (& $exe check $path 2>&1 | Out-String)
    [pscustomobject]@{ Exit = $LASTEXITCODE; Diagnostics = (Normalize-Diagnostics $output) }
}

$previousTypeshed = [Environment]::GetEnvironmentVariable('ZUBAN_TYPESHED', 'Process')
$previousNoColor = [Environment]::GetEnvironmentVariable('NO_COLOR', 'Process')
$failed = $false
$results = @()
try {
    [Environment]::SetEnvironmentVariable('ZUBAN_TYPESHED', (Join-Path $repo 'third_party\typeshed'), 'Process')
    [Environment]::SetEnvironmentVariable('NO_COLOR', '1', 'Process')
    foreach ($case in $cases) {
        $path = Join-Path $casesDir ($case.Name + '.py')
        [IO.File]::WriteAllText($path, $case.Code, [Text.UTF8Encoding]::new($false))
        $cargo = Check-File $CargoExe $path
        $blaze = Check-File $BlazeExe $path
        $match = $cargo.Exit -eq $blaze.Exit -and $cargo.Diagnostics -ceq $blaze.Diagnostics
        $expected = $cargo.Exit -eq $case.Exit -and $blaze.Exit -eq $case.Exit
        if (-not ($match -and $expected)) { $failed = $true }
        [IO.File]::WriteAllText((Join-Path $runDir ($case.Name + '-cargo.txt')), $cargo.Diagnostics)
        [IO.File]::WriteAllText((Join-Path $runDir ($case.Name + '-blaze.txt')), $blaze.Diagnostics)
        $result = [pscustomobject]@{ Case = $case.Name; CargoExit = $cargo.Exit; BlazeExit = $blaze.Exit; DiagnosticsMatch = $match; ExpectedExit = $expected }
        $results += $result
        $results | Export-Csv -LiteralPath (Join-Path $runDir 'diagnostic-results.csv') -NoTypeInformation
        $result
        if (-not $match) {
            Write-Output "--- Cargo $($case.Name) ---`n$($cargo.Diagnostics)`n--- Blaze $($case.Name) ---`n$($blaze.Diagnostics)"
        }
    }
}
finally {
    [Environment]::SetEnvironmentVariable('ZUBAN_TYPESHED', $previousTypeshed, 'Process')
    [Environment]::SetEnvironmentVariable('NO_COLOR', $previousNoColor, 'Process')
}

if ($failed) { throw 'Diagnostic smoke comparison failed' }
