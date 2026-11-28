[CmdletBinding()]
param(
    [ValidateSet('Release', 'FastRelease', 'Debug')]
    [string]$Configuration = 'Release',

    [switch]$Run,

    [string]$ConfigPath
)

$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot
$targetDir = Join-Path $workspace 'target'
$profile = switch ($Configuration) {
    'Release' { 'release' }
    'FastRelease' { 'fast-release' }
    'Debug' { 'dev' }
}
$outputName = if ($profile -eq 'dev') { 'debug' } else { $profile }
$outputDir = Join-Path $targetDir $outputName

if (-not $Run) {
    $cargoArgs = @(
        'build', '--locked', '--target-dir', $targetDir, '--profile', $profile,
        '--package', 'pi-web-backend', '--bin', 'pi-web-backend',
        '--package', 'pi-coding-agent', '--bin', 'pi'
    )
    Push-Location -LiteralPath $workspace
    try {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            exit $LASTEXITCODE
        }
    } finally {
        Pop-Location
    }

    Write-Host "Binaries: $outputDir"
    return
}

$exeSuffix = ''
if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    $exeSuffix = '.exe'
}
$backend = Join-Path $outputDir "pi-web-backend$exeSuffix"
$agent = Join-Path $outputDir "pi$exeSuffix"
foreach ($binary in @($backend, $agent)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Missing $binary. Run this script without -Run to build both binaries."
    }
}

if (-not $ConfigPath) {
    $ConfigPath = Join-Path $PSScriptRoot 'pi-web-backend.psd1'
}
$settings = Import-PowerShellDataFile -LiteralPath $ConfigPath
$previous = @{}
try {
    foreach ($name in $settings.Keys) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $settings[$name], 'Process')
    }
    & $backend
    $backendExitCode = $LASTEXITCODE
} finally {
    foreach ($name in $previous.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
}
if ($backendExitCode -ne 0) {
    exit $backendExitCode
}
