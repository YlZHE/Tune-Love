# Fetch the pinned CMake (Windows x64 zip) that src-tauri/build.rs needs to build FFTW.
#
#   pwsh -File scripts/fetch-cmake.ps1 [-Destination <dir>]
#
# Default destination: src-tauri/vendor/tools  (git-ignored)
# Result:              <Destination>/cmake-3.30.5-windows-x86_64/bin/cmake.exe
#
# SHA-256 provenance: value taken from Kitware's published
# cmake-3.30.5-SHA-256.txt for cmake-3.30.5-windows-x86_64.zip (the same file that
# is kept next to the already-extracted copy in src-tauri/vendor/tools) and equal to
# the hash of the zip that copy was extracted from.
#
# Flow: download to <dest>/.download-<pid>.zip -> verify SHA-256 -> extract to
# <dest>/.extract-<pid> -> rename into place. Temp files are always removed.
# Exit code 0 on success (including "already present"), non-zero otherwise.
#
# -ExpectedSha256 exists only so the self-test can force a mismatch.
# -SelfTest runs three cases against a fresh directory under $env:TEMP and never
# touches src-tauri/vendor/tools.
[CmdletBinding()]
param(
    [string]$Destination,
    [string]$ExpectedSha256,
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$CmakeVersion = '3.30.5'
$DirName = "cmake-$CmakeVersion-windows-x86_64"
$Url = "https://github.com/Kitware/CMake/releases/download/v$CmakeVersion/$DirName.zip"
$PinnedSha256 = '5ab6e1faf20256ee4f04886597e8b6c3b1bd1297b58a68a58511af013710004b'

if (-not $Destination) {
    $Destination = Join-Path (Join-Path $PSScriptRoot '..') 'src-tauri/vendor/tools'
}
if (-not $ExpectedSha256) { $ExpectedSha256 = $PinnedSha256 }

function Invoke-SelfTest {
    $root = Join-Path $env:TEMP "fetch-cmake-selftest-$PID"
    $script:failures = @()
    function Check([bool]$ok, [string]$what) {
        if ($ok) { Write-Host "  ok   $what" } else { Write-Host "  FAIL $what"; $script:failures += $what }
    }
    function Run-Child([string]$dest, [string[]]$extra, [hashtable]$env2 = @{}) {
        $saved = @{}
        foreach ($k in $env2.Keys) { $saved[$k] = [Environment]::GetEnvironmentVariable($k); [Environment]::SetEnvironmentVariable($k, $env2[$k]) }
        try {
            $out = & pwsh -NoProfile -File $PSCommandPath -Destination $dest @extra 2>&1 | Out-String
            return [pscustomobject]@{ Code = $LASTEXITCODE; Out = $out }
        } finally {
            foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
        }
    }
    try {
        New-Item -ItemType Directory -Force $root | Out-Null

        Write-Host 'case 1: fresh download into an empty directory'
        $d1 = Join-Path $root 'ok'
        $r = Run-Child $d1 @()
        $exe = Join-Path $d1 "$DirName/bin/cmake.exe"
        Check ($r.Code -eq 0) "exit code 0 (got $($r.Code))"
        Check (Test-Path $exe) 'cmake.exe exists'
        Check (-not (Get-ChildItem $d1 -Force -Filter '.download-*') -and -not (Get-ChildItem $d1 -Force -Filter '.extract-*')) 'no temp leftovers'

        Write-Host 'case 2: second run is a no-op without network'
        $t0 = (Get-Item $exe).LastWriteTimeUtc
        # A dead proxy makes any download attempt fail, so success proves no request was made.
        $r = Run-Child $d1 @() @{ HTTPS_PROXY = 'http://127.0.0.1:9'; HTTP_PROXY = 'http://127.0.0.1:9' }
        Check ($r.Code -eq 0) "exit code 0 (got $($r.Code))"
        Check ($r.Out -match 'already present') 'prints "already present"'
        Check ((Get-Item $exe).LastWriteTimeUtc -eq $t0) 'cmake.exe modification time unchanged'

        Write-Host 'case 3: wrong SHA-256 fails and leaves nothing behind'
        $d3 = Join-Path $root 'bad'
        $r = Run-Child $d3 @('-ExpectedSha256', ('0' * 64))
        Check ($r.Code -ne 0) "non-zero exit code (got $($r.Code))"
        Check (-not (Test-Path (Join-Path $d3 $DirName))) 'target directory does not exist'
        $left = @()
        if (Test-Path $d3) { $left = @(Get-ChildItem $d3 -Force | Where-Object { $_.Name -like '.download-*' -or $_.Name -like '.extract-*' }) }
        Check ($left.Count -eq 0) 'no .download-* / .extract-* leftovers'
    } finally {
        if (Test-Path $root) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
    }
    if ($script:failures.Count -eq 0) { Write-Host 'SELFTEST PASS'; exit 0 }
    Write-Host "SELFTEST FAIL ($($script:failures.Count))"; exit 1
}

if ($SelfTest) { Invoke-SelfTest }

$final = Join-Path $Destination $DirName
$exePath = Join-Path $final 'bin/cmake.exe'

if (Test-Path $exePath) {
    Write-Host "CMake $CmakeVersion already present: $exePath"
    exit 0
}
if (Test-Path $final) {
    Write-Error "$final exists but has no bin/cmake.exe; remove it and run again."
    exit 1
}

$destCreated = -not (Test-Path $Destination)
New-Item -ItemType Directory -Force $Destination | Out-Null
$zip = Join-Path $Destination ".download-$PID.zip"
$tmp = Join-Path $Destination ".extract-$PID"
$ok = $false
try {
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $ExpectedSha256.ToLowerInvariant()) {
        throw "SHA-256 mismatch for $DirName.zip: expected $ExpectedSha256, got $actual"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $tmp)
    $inner = Join-Path $tmp $DirName
    if (-not (Test-Path (Join-Path $inner 'bin/cmake.exe'))) {
        throw "archive does not contain $DirName/bin/cmake.exe"
    }
    Move-Item -LiteralPath $inner -Destination $final
    $ok = $true
    Write-Host "CMake $CmakeVersion ready: $exePath"
} catch {
    Write-Host "fetch-cmake failed: $($_.Exception.Message)"
} finally {
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    if (-not $ok -and $destCreated -and -not (Get-ChildItem $Destination -Force -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $Destination -Force -ErrorAction SilentlyContinue
    }
}
exit ($ok ? 0 : 1)
