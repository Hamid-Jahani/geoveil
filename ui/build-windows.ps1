[CmdletBinding()]
param([string]$GeoveilBinary)

$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Build this Windows installer on Windows.'
}

$uiRoot = $PSScriptRoot
$repoRoot = Split-Path -Parent $uiRoot
$manifest = Get-Content -LiteralPath (Join-Path $uiRoot 'package.json') -Raw | ConvertFrom-Json
$version = $manifest.version
if ($version -notmatch '^\d+\.\d+\.\d+$') {
    throw 'Expected a Geoveil release version such as 0.1.1 in ui/package.json.'
}

$binDir = Join-Path $uiRoot 'bin'
$stagedBinary = Join-Path $binDir 'geoveil.exe'
New-Item -ItemType Directory -Path $binDir -Force | Out-Null

if (!$GeoveilBinary) {
    Push-Location -LiteralPath $repoRoot
    try {
        cargo build --release --locked -p geoveil-cli
        if ($LASTEXITCODE -ne 0) { throw "Geoveil CLI build failed (exit $LASTEXITCODE)." }
    } finally {
        Pop-Location
    }
    $GeoveilBinary = Join-Path $repoRoot 'target\release\geoveil.exe'
}

$source = (Resolve-Path -LiteralPath $GeoveilBinary).Path
if ($source -ne $stagedBinary) {
    Copy-Item -LiteralPath $source -Destination $stagedBinary -Force
}

Push-Location -LiteralPath $uiRoot
try {
    npm ci
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed (exit $LASTEXITCODE)." }
    npm run dist:win
    if ($LASTEXITCODE -ne 0) { throw "Windows installer build failed (exit $LASTEXITCODE)." }
} finally {
    Pop-Location
}

$installer = Join-Path $repoRoot "target\desktop\Geoveil-Setup-$version-x64.exe"
if (!(Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "The installer was not created at $installer."
}
Get-FileHash -LiteralPath $installer -Algorithm SHA256
