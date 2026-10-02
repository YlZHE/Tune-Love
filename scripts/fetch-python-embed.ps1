# Fetch the pinned official Windows embeddable Python and patch it so the bridge
# script reference/app_bridge.py can `import client` from its own directory.
#
#   pwsh -File scripts/fetch-python-embed.ps1 [-Destination <dir>]
#
# Default destination: src-tauri/python  (git-ignored). Result: <Destination>/python.exe
#
# Pinned version: 3.13.16 (latest 3.13.x on python.org at the time of pinning,
# 2026-10-02; its directory has the embed-amd64 zip).
# SHA-256 of python-3.13.16-embed-amd64.zip, 11439094 bytes:
#   97dae5274cc54867065e8d5a3226e48c35017ed332a0fdb0e27d5b5821961297
# Cross-check (done 2026-10-02): the zip was downloaded once and hashed locally, and
# the hash was compared with two independent python.org publications:
#   1. the "Checksum" column of the Windows embeddable package (64-bit) row on
#      https://www.python.org/downloads/release/python-31316/ (64 hex chars, SHA-256);
#   2. the CPython package checksum in python-3.13.16-embed-amd64.zip.spdx.json
#      next to the zip on www.python.org/ftp.
# Both are identical to the local hash. (A .sigstore bundle is also published; this
# script does not verify it.) SHA-256 of the extracted python.exe, used for the
# "already present" check:
#   31fe10b4ed82a2f960af0ef9931f46a4b84ff286f8b6ffd9dee098cf4f10f17f
#
# Flow: download to <parent of dest>/.download-<pid>.zip -> verify SHA-256 -> extract
# to <parent>/.extract-<pid> -> append `..\reference` to python313._pth (once;
# stock `#import site` is left untouched) -> run an import self-check -> rename the
# extracted directory to <Destination>. The temp files sit in the destination's parent
# because the destination is itself the directory being created. Temp files are always
# removed. Exit code 0 on success (including "already present"), non-zero otherwise.
#
# -ExpectedSha256 exists only so the self-test can force a mismatch.
# -SelfTest runs three cases under $env:TEMP and never touches src-tauri/python.
[CmdletBinding()]
param(
    [string]$Destination,
    [string]$ExpectedSha256,
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$PyVersion = '3.13.16'
$Url = "https://www.python.org/ftp/python/$PyVersion/python-$PyVersion-embed-amd64.zip"
$PinnedSha256 = '97dae5274cc54867065e8d5a3226e48c35017ed332a0fdb0e27d5b5821961297'
$PinnedExeSha256 = '31fe10b4ed82a2f960af0ef9931f46a4b84ff286f8b6ffd9dee098cf4f10f17f'
$PthName = 'python313._pth'
$PthLine = '..\reference'

if (-not $Destination) {
    $Destination = Join-Path (Join-Path $PSScriptRoot '..') 'src-tauri/python'
}
if (-not $ExpectedSha256) { $ExpectedSha256 = $PinnedSha256 }

function Test-PthPatched([string]$dir) {
    $pth = Join-Path $dir $PthName
    if (-not (Test-Path $pth)) { return $false }
    $lines = [System.IO.File]::ReadAllLines($pth) | ForEach-Object { $_.Trim() }
    return ($lines -contains $PthLine)
}

function Add-PthLine([string]$dir) {
    $pth = Join-Path $dir $PthName
    $text = [System.IO.File]::ReadAllText($pth)
    if (Test-PthPatched $dir) { return }
    if ($text.Length -gt 0 -and -not $text.EndsWith("`n")) { $text += "`r`n" }
    $text += "$PthLine`r`n"
    [System.IO.File]::WriteAllText($pth, $text, (New-Object System.Text.UTF8Encoding($false)))
}

function Invoke-SelfTest {
    $root = Join-Path $env:TEMP "fetch-python-selftest-$PID"
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
    function Leftovers([string]$parent) {
        if (-not (Test-Path $parent)) { return @() }
        return @(Get-ChildItem $parent -Force | Where-Object { $_.Name -like '.download-*' -or $_.Name -like '.extract-*' })
    }
    try {
        New-Item -ItemType Directory -Force $root | Out-Null

        Write-Host 'case 1: fresh download into an empty directory'
        $d1 = Join-Path $root 'ok/python'
        $r = Run-Child $d1 @()
        $exe = Join-Path $d1 'python.exe'
        Check ($r.Code -eq 0) "exit code 0 (got $($r.Code))"
        Check (Test-Path $exe) 'python.exe exists'
        $pth = [System.IO.File]::ReadAllLines((Join-Path $d1 $PthName))
        Check ((@($pth | Where-Object { $_.Trim() -eq $PthLine })).Count -eq 1) "$PthName contains `..\reference` exactly once"
        Check ($pth -contains '#import site') 'stock "#import site" line untouched'
        $v = & $exe -c "import ctypes, json, sys; print(sys.version)"
        Check ($LASTEXITCODE -eq 0 -and "$v" -match "^$([regex]::Escape($PyVersion))") "python.exe imports ctypes/json ($v)"
        Check ((Leftovers (Split-Path $d1)).Count -eq 0) 'no temp leftovers'

        Write-Host 'case 2: second run is a no-op without network'
        $t0 = (Get-Item $exe).LastWriteTimeUtc
        $p0 = (Get-Item (Join-Path $d1 $PthName)).LastWriteTimeUtc
        # A dead proxy makes any download attempt fail, so success proves no request was made.
        $r = Run-Child $d1 @() @{ HTTPS_PROXY = 'http://127.0.0.1:9'; HTTP_PROXY = 'http://127.0.0.1:9' }
        Check ($r.Code -eq 0) "exit code 0 (got $($r.Code))"
        Check ($r.Out -match 'already present') 'prints "already present"'
        Check ((Get-Item $exe).LastWriteTimeUtc -eq $t0) 'python.exe modification time unchanged'
        Check ((Get-Item (Join-Path $d1 $PthName)).LastWriteTimeUtc -eq $p0) "$PthName modification time unchanged"
        $pth = [System.IO.File]::ReadAllLines((Join-Path $d1 $PthName))
        Check ((@($pth | Where-Object { $_.Trim() -eq $PthLine })).Count -eq 1) "$PthName line still present exactly once"

        Write-Host 'case 3: wrong SHA-256 fails and leaves nothing behind'
        $d3 = Join-Path $root 'bad/python'
        $r = Run-Child $d3 @('-ExpectedSha256', ('0' * 64))
        Check ($r.Code -ne 0) "non-zero exit code (got $($r.Code))"
        Check (-not (Test-Path $d3)) 'target directory does not exist'
        Check ((Leftovers (Split-Path $d3)).Count -eq 0) 'no .download-* / .extract-* leftovers'
    } finally {
        if (Test-Path $root) { Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue }
    }
    if ($script:failures.Count -eq 0) { Write-Host 'SELFTEST PASS'; exit 0 }
    Write-Host "SELFTEST FAIL ($($script:failures.Count))"; exit 1
}

if ($SelfTest) { Invoke-SelfTest }

$Destination = [System.IO.Path]::GetFullPath($Destination)
$exePath = Join-Path $Destination 'python.exe'

if (Test-Path $Destination) {
    if ((Test-Path $exePath) -and
        ((Get-FileHash $exePath -Algorithm SHA256).Hash.ToLowerInvariant() -eq $PinnedExeSha256) -and
        (Test-PthPatched $Destination)) {
        Write-Host "Embedded Python $PyVersion already present: $exePath"
        exit 0
    }
    Write-Error "$Destination exists but is not a complete Python $PyVersion embed (python.exe hash or $PthName patch line missing); remove it and run again."
    exit 1
}

$parent = Split-Path $Destination
$parentCreated = -not (Test-Path $parent)
New-Item -ItemType Directory -Force $parent | Out-Null
$zip = Join-Path $parent ".download-$PID.zip"
$tmp = Join-Path $parent ".extract-$PID"
$ok = $false
try {
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $ExpectedSha256.ToLowerInvariant()) {
        throw "SHA-256 mismatch for python-$PyVersion-embed-amd64.zip: expected $ExpectedSha256, got $actual"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $tmp)
    if (-not (Test-Path (Join-Path $tmp 'python.exe')) -or -not (Test-Path (Join-Path $tmp $PthName))) {
        throw "archive does not contain python.exe and $PthName"
    }
    Add-PthLine $tmp
    $ver = & (Join-Path $tmp 'python.exe') -c "import ctypes, json, sys; print(sys.version)"
    if ($LASTEXITCODE -ne 0) { throw 'python.exe self-check (import ctypes, json, sys) failed' }
    Write-Host "python.exe self-check: $ver"
    Move-Item -LiteralPath $tmp -Destination $Destination
    $ok = $true
    Write-Host "Embedded Python $PyVersion ready: $exePath"
} catch {
    Write-Host "fetch-python-embed failed: $($_.Exception.Message)"
} finally {
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    if (-not $ok -and $parentCreated -and -not (Get-ChildItem $parent -Force -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $parent -Force -ErrorAction SilentlyContinue
    }
}
exit ($ok ? 0 : 1)
