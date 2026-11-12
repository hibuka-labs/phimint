# install.ps1 — phimint installer (Windows)
#
#   irm https://github.com/hibuka-labs/phimint/releases/latest/download/install.ps1 | iex
#   $env:PHIMINT_MIRROR='gitee'; irm https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install.ps1 | iex   # mainland China
#
# Env:
#   $env:PHIMINT_MIRROR = "gitee"|"github"   force a download mirror
#   $env:PHIMINT_INDEX_URL = "<url>"         fetch the flat index from here
#   $env:PHIMINT_INSTALL_DIR = "<dir>"       install location (default: ~\.local\bin)
#   $env:PHIMINT_NO_PATH_EDIT = "1"          do not touch the user PATH
#   $env:PHIMINT_NO_SHORTCUT = "1"           do not create the Start Menu shortcut
#
# The flat index (install-index.txt) is generated from the same source as
# manifest.json, so this script and install.sh read the same three fields.

$ErrorActionPreference = "Stop"

$GithubIndex = "https://github.com/hibuka-labs/phimint/releases/latest/download/install-index.txt"
# Same asset name on both mirrors: the release run (deploy/release.sh step
# 7.2b) uploads the Gitee-URL variant (install-index-gitee.txt from
# scripts/gen-manifest.py) renamed to install-index.txt on the Gitee release.
$GiteeIndex = "https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install-index.txt"

function Fail([string]$Msg) { Write-Error "❌ $Msg"; exit 1 }

# ── 1. Platform detection (keys match the update manifest) ──────────────────
# Windows on ARM runs the x64 binary via emulation.
$Arch = "x86_64"
$Key = "windows-$Arch"

# ── 2. Index fetch: GitHub first, Gitee fallback ────────────────────────────
$Endpoints = @($GithubIndex, $GiteeIndex)
if ($env:PHIMINT_MIRROR -eq "gitee") { $Endpoints = @($GiteeIndex, $GithubIndex) }
if ($env:PHIMINT_INDEX_URL) { $Endpoints = @($env:PHIMINT_INDEX_URL) }

$Index = $null
foreach ($Endpoint in $Endpoints) {
    Write-Host "  fetching index: $Endpoint"
    try {
        $Index = (Invoke-WebRequest -Uri $Endpoint -UseBasicParsing -TimeoutSec 20).Content
    } catch {
        $Index = $null
    }
    if ($Index) { break }
}
if (-not $Index) { Fail "could not fetch the release index from any mirror." }

# ── 3. Resolve version / URL / sha256 for this platform ─────────────────────
# The index is flat text: a `version` line and one line per platform.
$Version = $null
$Url = $null
$Sha = $null
foreach ($Line in ($Index -split "`r?`n")) {
    $Trimmed = $Line.Trim()
    if (-not $Trimmed) { continue }
    $Parts = $Trimmed -split '\s+'
    if ($Parts[0] -eq "version") {
        if ($Parts.Count -ge 2) { $Version = $Parts[1] }
        continue
    }
    if ($Parts[0] -eq $Key -and $Parts.Count -ge 3) {
        $Url = $Parts[1]
        $Sha = $Parts[2]
    }
}
if (-not $Url) { Fail "no prebuilt binary for platform $Key in the release index" }
Write-Host "  phimint $Version for $Key"

# ── 4. Download + checksum ──────────────────────────────────────────────────
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("phimint-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null
$Archive = Join-Path $Tmp "phimint.zip"
try {
    Invoke-WebRequest -Uri $Url -OutFile $Archive -UseBasicParsing -TimeoutSec 300
} catch {
    Fail "download failed: $Url"
}

if ($Sha) {
    $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLower()
    if ($Actual -ne $Sha.ToLower()) {
        Fail "checksum mismatch (expected $Sha, got $Actual)"
    }
    Write-Host "  checksum ok"
} else {
    Write-Host "  checksum: the index has none - installing unverified"
}

# ── 5. Install binary: temp file first, then rename into place ─────────────
# Writing phimint.exe.tmp and renaming it means a half-written binary is
# never visible as phimint.exe.
$InstallDir = if ($env:PHIMINT_INSTALL_DIR) { $env:PHIMINT_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Expand-Archive -Path $Archive -DestinationPath $Tmp -Force
$ExePath = Join-Path $InstallDir "phimint.exe"
$TmpExe = Join-Path $InstallDir "phimint.exe.tmp"
Copy-Item -Force (Join-Path $Tmp "phimint.exe") $TmpExe
Move-Item -Force $TmpExe $ExePath
Write-Host "  installed -> $ExePath"

# ── 6. User PATH (Windows keeps it in the registry) ─────────────────────────
if ($env:PHIMINT_NO_PATH_EDIT -ne "1") {
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $Want = $InstallDir.TrimEnd('\').ToLower()
    $Present = $false
    if ($UserPath) {
        foreach ($Entry in ($UserPath -split ';')) {
            if ($Entry.Trim().TrimEnd('\').ToLower() -eq $Want) { $Present = $true }
        }
    }
    if (-not $Present) {
        $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "  added $InstallDir to user PATH (restart your shell)"
    }
}

# ── 7. Start Menu shortcut ──────────────────────────────────────────────────
# One entry, named phimint, pointing at the installed binary and wearing the
# binary's own icon. A shortcut is a convenience, never a requirement: if the
# COM object is unavailable the install still succeeds.
if ($env:PHIMINT_NO_SHORTCUT -ne "1") {
    try {
        $Programs = [Environment]::GetFolderPath("Programs")
        if ($Programs) {
            $ShortcutPath = Join-Path $Programs "phimint.lnk"
            $WshShell = New-Object -ComObject WScript.Shell
            $Shortcut = $WshShell.CreateShortcut($ShortcutPath)
            $Shortcut.TargetPath = $ExePath
            $Shortcut.WorkingDirectory = $InstallDir
            $Shortcut.IconLocation = "$ExePath,0"
            $Shortcut.Description = "phimint - a terminal AI coding agent"
            $Shortcut.Save()
            Write-Host "  Start Menu shortcut: $ShortcutPath"
        }
    } catch {
        Write-Host "  could not create the Start Menu shortcut (skipping)"
    }
}

Remove-Item -Recurse -Force $Tmp

# ── 8. Record the install source + let the App write the config ────────────
$PhimintDir = Join-Path $HOME ".phimint"
New-Item -ItemType Directory -Force -Path $PhimintDir | Out-Null
$StatePath = Join-Path $PhimintDir "state.json"
if (-not (Test-Path $StatePath)) {
    Set-Content -Path $StatePath -Value "{`n  `"install_source`": `"standalone`"`n}"
} else {
    # The App owns this file and keeps every other field, so only the one
    # value is replaced.
    $Raw = Get-Content $StatePath -Raw
    if ($Raw -match '"install_source"') {
        Set-Content -Path $StatePath -Value ($Raw -replace '"install_source"\s*:\s*"[^"]*"', '"install_source": "standalone"')
    }
}
Write-Host "  install source recorded (upgrade: phimint update)"

# The App writes the config skeleton - one source of truth for its shape.
# Name the file the App will actually read - its resolve_path rule (an
# existing LLM_CONFIG, else the first existing default, else the skeleton
# fallback), not a parallel rule of our own.
if ($env:LLM_CONFIG -and (Test-Path $env:LLM_CONFIG)) {
    $ConfigPath = $env:LLM_CONFIG
} elseif (Test-Path (Join-Path $PhimintDir "config.json")) {
    $ConfigPath = Join-Path $PhimintDir "config.json"
} elseif (Test-Path (Join-Path $HOME ".config/phimint/config.json")) {
    $ConfigPath = Join-Path $HOME ".config/phimint/config.json"
} elseif ($env:LLM_CONFIG) {
    $ConfigPath = $env:LLM_CONFIG
} else {
    $ConfigPath = Join-Path $PhimintDir "config.json"
}
$Written = $false
try {
    & $ExePath init --if-missing | Out-Null
    $Written = ($LASTEXITCODE -eq 0)
} catch {
    $Written = $false
}
if ($Written) {
    Write-Host "  config file: $ConfigPath"
} else {
    Write-Host "  could not write $ConfigPath - create it and fill in api_key"
}

Write-Host ""
Write-Host "✅ phimint $Version ready."
Write-Host ""
Write-Host "Next: open $ConfigPath, fill in the api_key line, then run:  phimint"
