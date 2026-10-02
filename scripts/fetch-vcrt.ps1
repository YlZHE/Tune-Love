# Copy the x64 Visual C++ runtime DLLs from the local Visual Studio installation into
# src-tauri/vcrt/ (git-ignored). The installer ships them app-locally next to
# tune-love.exe and devocal-engine.exe (see bundle.resources in
# src-tauri/tauri.release.conf.json), so users need no separate VC++ Redistributable.
#
#   pwsh -File scripts/fetch-vcrt.ps1 [-Destination <dir>]
#
# Both exes import MSVCP140.dll; devocal-engine.exe (statically linked ONNX Runtime) also
# imports MSVCP140_1.dll, VCRUNTIME140.dll and VCRUNTIME140_1.dll.
#
# Source: the newest VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT of the latest Visual
# Studio instance reported by vswhere. Taking them from the same Visual Studio that builds
# the exes keeps the runtime at least as new as the toolset that linked against it.
# Each DLL must carry a valid Authenticode signature. Exit code 0 on success, non-zero
# (with a message naming what is missing) otherwise.
[CmdletBinding()]
param(
    [string]$Destination
)

$ErrorActionPreference = 'Stop'

$Dlls = @('msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll')

if (-not $Destination) {
    $Destination = Join-Path (Join-Path $PSScriptRoot '..') 'src-tauri/vcrt'
}
$Destination = [System.IO.Path]::GetFullPath($Destination)

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) {
    Write-Error "vswhere.exe not found at $vswhere; install Visual Studio 2022 (or Build Tools) with the C++ workload."
    exit 1
}

$found = @(& $vswhere -latest -products * -find 'VC\Redist\MSVC\*\x64\Microsoft.VC143.CRT')
if ($LASTEXITCODE -ne 0) {
    Write-Error "vswhere failed (exit $LASTEXITCODE)"
    exit 1
}
# <...>\VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT -> pick the highest <version>.
$candidates = foreach ($dir in $found) {
    if (-not $dir) { continue }
    $versionName = Split-Path -Leaf (Split-Path -Parent (Split-Path -Parent $dir))
    $version = $null
    if ([version]::TryParse($versionName, [ref]$version)) {
        [pscustomobject]@{ Path = $dir; Version = $version }
    }
}
$source = $candidates | Sort-Object Version -Descending | Select-Object -First 1
if (-not $source) {
    Write-Error 'No VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT found; install the "MSVC v143 - VS 2022 C++ x64/x86 build tools" component of Visual Studio.'
    exit 1
}
Write-Host "VC++ runtime source: $($source.Path)"

$missing = @($Dlls | Where-Object { -not (Test-Path -LiteralPath (Join-Path $source.Path $_)) })
if ($missing.Count -gt 0) {
    Write-Error "Missing in $($source.Path): $($missing -join ', ')"
    exit 1
}
foreach ($dll in $Dlls) {
    $path = Join-Path $source.Path $dll
    $sig = Get-AuthenticodeSignature -LiteralPath $path
    if ($sig.Status -ne 'Valid') {
        Write-Error "$path has no valid Authenticode signature (status: $($sig.Status))"
        exit 1
    }
}

New-Item -ItemType Directory -Force $Destination | Out-Null
foreach ($dll in $Dlls) {
    $from = Join-Path $source.Path $dll
    $to = Join-Path $Destination $dll
    Copy-Item -LiteralPath $from -Destination $to -Force
    $info = (Get-Item -LiteralPath $to).VersionInfo
    Write-Host ("  {0,-20} {1}" -f $dll, $info.FileVersion)
}
$stillMissing = @($Dlls | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Destination $_)) })
if ($stillMissing.Count -gt 0) {
    Write-Error "Copy failed for: $($stillMissing -join ', ')"
    exit 1
}
Write-Host "VC++ runtime ready in $Destination"
exit 0
