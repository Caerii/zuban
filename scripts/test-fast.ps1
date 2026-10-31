# Run the complete suite with faster incremental compilation. Keep the normal
# optimized `cargo test` gate as well; this command trades test speed for build speed.
$ErrorActionPreference = 'Stop'
$taskNames = @('CARGO_PROFILE_TEST_OPT_LEVEL', 'CARGO_BUILD_JOBS', 'RAYON_NUM_THREADS')
$taskPrevious = @{}
foreach ($taskName in $taskNames) {
    $taskPrevious[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process')
}
$taskExit = 1
Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    $env:CARGO_PROFILE_TEST_OPT_LEVEL = '0'
    $env:CARGO_BUILD_JOBS = '1'
    $env:RAYON_NUM_THREADS = '1'
    & cargo test -- --test-threads=1
    $taskExit = $LASTEXITCODE
} finally {
    foreach ($taskName in $taskNames) {
        [Environment]::SetEnvironmentVariable($taskName, $taskPrevious[$taskName], 'Process')
    }
    Pop-Location
}
exit $taskExit
