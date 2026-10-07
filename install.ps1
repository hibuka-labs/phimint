# install.ps1 — phimint installer (Windows)
#
#   irm https://github.com/hibuka-labs/phimint/releases/latest/download/install.ps1 | iex
#   irm https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/install.ps1 | iex   # mainland China
#
# Env:
#   $env:PHIMINT_MIRROR = "gitee"|"github"   force a download mirror
#   $env:PHIMINT_INSTALL_DIR = "<dir>"       install location (default: ~\.local\bin)

$ErrorActionPreference = "Stop"

$GithubManifest = "https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json"
$GiteeManifest = "https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/manifest.json"

function Fail([string]$Msg) { Write-Error "❌ $Msg"; exit 1 }

# ── 1. Platform detection (keys match the update manifest) ──────────────────
# Windows on ARM runs the x64 binary via emulation.
$Arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "x86_64" } else { "x86_64" }
$Key = "windows-$Arch"

# ── 2. Manifest fetch: GitHub first, Gitee fallback ─────────────────────────
$Endpoints = @($GithubManifest, $GiteeManifest)
if ($env:PHIMINT_MIRROR -eq "gitee") { $Endpoints = @($GiteeManifest, $GithubManifest) }

$Manifest = $null
foreach ($Endpoint in $Endpoints) {
    Write-Host "  fetching manifest: $Endpoint"
    try {
        $Manifest = Invoke-RestMethod -Uri $Endpoint -TimeoutSec 20
        break
    } catch { }
}
if (-not $Manifest) { Fail "could not fetch the release manifest from any mirror." }

# ── 3. Resolve version / URL / sha256 for this platform ─────────────────────
$Entry = $Manifest.platforms.$Key
if (-not $Entry) { Fail "no prebuilt binary for platform $Key in manifest $($Manifest.version)" }
$Version = $Manifest.version
Write-Host "  phimint $Version for $Key"

# ── 4. Download + checksum ──────────────────────────────────────────────────
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("phimint-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null
$Archive = Join-Path $Tmp "phimint.zip"
try {
    Invoke-WebRequest -Uri $Entry.url -OutFile $Archive -TimeoutSec 300
} catch {
    Fail "download failed: $($Entry.url)"
}

if ($Entry.sha256) {
    $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLower()
    if ($Actual -ne $Entry.sha256.ToLower()) {
        Fail "checksum mismatch (expected $($Entry.sha256), got $Actual)"
    }
    Write-Host "  checksum ok"
} else {
    Write-Host "  checksum: manifest has none - installing unverified"
}

# ── 5. Install binary ───────────────────────────────────────────────────────
$InstallDir = if ($env:PHIMINT_INSTALL_DIR) { $env:PHIMINT_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Expand-Archive -Path $Archive -DestinationPath $Tmp -Force
Move-Item -Force (Join-Path $Tmp "phimint.exe") (Join-Path $InstallDir "phimint.exe")
Write-Host "  installed -> $InstallDir\phimint.exe"

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "  added $InstallDir to user PATH (restart your shell)"
}

Remove-Item -Recurse -Force $Tmp

# ── 6. Record install source (upgrade routing) + config bootstrap ───────────
$PhimintDir = Join-Path $HOME ".phimint"
New-Item -ItemType Directory -Force -Path $PhimintDir | Out-Null
$StatePath = Join-Path $PhimintDir "state.json"
$State = @{}
if (Test-Path $StatePath) {
    try { $State = Get-Content $StatePath -Raw | ConvertFrom-Json -AsHashtable } catch { $State = @{} }
}
$State["install_source"] = "standalone"
$State | ConvertTo-Json | Set-Content $StatePath
Write-Host "  install source recorded (upgrade: phimint update)"

$ConfigPath = Join-Path $PhimintDir "config.json"
if (-not (Test-Path $ConfigPath)) {
    Write-Host "  next: create ~\.phimint\config.json with your API key (see README Configure)"
}

Write-Host ""
Write-Host "✅ phimint $Version ready. Run:  phimint"
