# The installer's contract, exercised in a sandbox (Windows).
#
#   pwsh -NoProfile -File tests/install_script_test.ps1
#
# The Windows twin of tests/install_script_test.sh: the real install.ps1
# against a local index and a local zip, served over loopback — no network,
# never the user's own profile.
#
# The rules it pins (notes/install-onboarding-design.md, decisions 3/6/7/8):
#
#   1. the binary lands in the install directory, no temp file is left
#      behind (decision 7) and the App is asked to write the config skeleton
#      (decision 3);
#   2. a checksum mismatch installs nothing;
#   3. an existing config is never touched (decision 6);
#   4. the install source is recorded, so `phimint update` knows the channel;
#   5. a Start Menu shortcut is created and points at the installed binary.
#
# Note: the shortcut case writes into the sandbox, never into the real
# Start Menu of whoever runs this (2026-11-13).

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$Installer = Join-Path $Root "install.ps1"

$script:Passed = 0
$script:Failed = 0

function Ok([string]$Name) { Write-Host "  ok   $Name"; $script:Passed++ }
function Bad([string]$Name) { Write-Host "  FAIL $Name"; $script:Failed++ }
function Check([string]$Name, [bool]$Condition) {
    if ($Condition) { Ok $Name } else { Bad $Name }
}

# ── helpers ────────────────────────────────────────────────────────────────

# A console app that records how it was called, standing in for the real
# binary. The real phimint.exe is a Rust binary; this only has to run and
# write its arguments down.
function New-StubExe([string]$Path) {
    $Source = Join-Path ([System.IO.Path]::GetTempPath()) ("phimint-stub-" + [Guid]::NewGuid() + ".cs")
    @'
using System;
using System.IO;
public class PhimintStub {
    public static int Main(string[] args) {
        string log = Environment.GetEnvironmentVariable("PHIMINT_TEST_LOG");
        if (!string.IsNullOrEmpty(log)) {
            File.AppendAllText(log, string.Join(" ", args) + Environment.NewLine);
        }
        return 0;
    }
}
'@ | Set-Content -Path $Source -Encoding UTF8

    if (Test-Path $Path) { Remove-Item -Force $Path }
    $Csc = Join-Path $env:WINDIR "Microsoft.NET\Framework64\v4.0.30319\csc.exe"
    if (Test-Path $Csc) {
        & $Csc "/nologo" "/target:exe" "/out:$Path" $Source | Out-Null
        if ($LASTEXITCODE -eq 0) { Remove-Item -Force $Source; return }
    }
    # No .NET Framework compiler: fall back to the in-session one.
    Add-Type -TypeDefinition @'
using System;
using System.IO;
public class PhimintStub2 {
    public static int Main(string[] args) {
        string log = Environment.GetEnvironmentVariable("PHIMINT_TEST_LOG");
        if (!string.IsNullOrEmpty(log)) {
            File.AppendAllText(log, string.Join(" ", args) + Environment.NewLine);
        }
        return 0;
    }
}
'@ -OutputAssembly $Path -OutputType ConsoleApplication
    Remove-Item -Force $Source
}

function Start-IndexServer([string]$Sha, [string]$ZipPath) {
    # A separate process, so Stop-IndexServer can end it at once. See
    # tests/install_index_server.ps1 for why it is not a job.
    $PortFile = Join-Path ([System.IO.Path]::GetTempPath()) ("phimint-port-" + [Guid]::NewGuid() + ".txt")
    $Shell = (Get-Command pwsh -ErrorAction SilentlyContinue).Source
    if (-not $Shell) { $Shell = (Get-Command powershell).Source }
    $ServerScript = Join-Path $Root "tests/install_index_server.ps1"
    $Process = Start-Process -FilePath $Shell `
        -ArgumentList @("-NoProfile", "-File", $ServerScript, $PortFile, $Sha, $ZipPath) `
        -PassThru -WindowStyle Hidden

    # Wait for the server to publish its port.
    $Port = 0
    for ($Try = 0; $Try -lt 100; $Try++) {
        if (Test-Path $PortFile) {
            $Text = (Get-Content $PortFile -Raw).Trim()
            if ($Text) { $Port = [int]$Text; break }
        }
        Start-Sleep -Milliseconds 100
    }
    if ($Port -eq 0) {
        try { Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue } catch {}
        throw "the index server never published a port"
    }
    return @{ Port = $Port; Process = $Process; PortFile = $PortFile }
}

function Stop-IndexServer($Server) {
    if ($Server -and $Server.Process) {
        try { Stop-Process -Id $Server.Process.Id -Force -ErrorAction SilentlyContinue } catch {}
    }
    if ($Server -and $Server.PortFile) {
        Remove-Item -Force $Server.PortFile -ErrorAction SilentlyContinue
    }
}

function Invoke-Installer([hashtable]$Environment) {
    $Shell = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($Shell) { $Exe = $Shell.Source } else { $Exe = (Get-Command powershell).Source }

    $Prelude = ($Environment.GetEnumerator() | ForEach-Object {
        $Value = ([string]$_.Value) -replace "'", "''"
        "`$env:$($_.Key)='$Value'"
    }) -join "; "
    $Script = $Installer -replace "'", "''"
    $Command = "$Prelude; & '$Script'"

    $Output = & $Exe -NoProfile -ExecutionPolicy Bypass -Command $Command 2>&1 | Out-String
    return @{ Status = $LASTEXITCODE; Output = $Output }
}

# A sandbox: a fake home, the stub binary, the zip and the log.
function New-Sandbox {
    $Sandbox = Join-Path ([System.IO.Path]::GetTempPath()) ("phimint-win-" + [Guid]::NewGuid())
    # $Home is a read-only automatic variable in PowerShell — assigning to it
    # throws "Cannot overwrite variable HOME because it is read-only or
    # constant" (2026-11-13 CI). Use our own name.
    $SandboxHome = Join-Path $Sandbox "home"
    New-Item -ItemType Directory -Force -Path $SandboxHome | Out-Null
    $Work = Join-Path $Sandbox "work"
    New-Item -ItemType Directory -Force -Path $Work | Out-Null

    $Stub = Join-Path $Work "phimint.exe"
    New-StubExe $Stub

    $Zip = Join-Path $Sandbox "phimint.zip"
    Compress-Archive -Path $Stub -DestinationPath $Zip -Force
    $Sha = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()

    $Log = Join-Path $Sandbox "log"
    Set-Content -Path $Log -Value "" -NoNewline

    return @{
        Root = $Sandbox
        Home = $SandboxHome
        InstallDir = (Join-Path $SandboxHome ".local\bin")
        Zip = $Zip
        Sha = $Sha
        Log = $Log
    }
}

# The environment every case runs the installer in.
function Sandbox-Environment($Sandbox, [int]$Port, [hashtable]$Extra) {
    $Environment = @{
        "HOME" = $Sandbox.Home
        "USERPROFILE" = $Sandbox.Home
        "PHIMINT_INDEX_URL" = "http://127.0.0.1:$Port/install-index.txt"
        "PHIMINT_INSTALL_DIR" = $Sandbox.InstallDir
        "PHIMINT_NO_PATH_EDIT" = "1"
        "PHIMINT_NO_SHORTCUT" = "1"
        "PHIMINT_TEST_LOG" = $Sandbox.Log
    }
    if ($Extra) {
        foreach ($Key in $Extra.Keys) { $Environment[$Key] = $Extra[$Key] }
    }
    return $Environment
}

# ── 1. A normal install ────────────────────────────────────────────────────
$Sandbox = New-Sandbox
$Server = Start-IndexServer $Sandbox.Sha $Sandbox.Zip
$Run = Invoke-Installer (Sandbox-Environment $Sandbox $Server.Port $null)

Check "install succeeds" ($Run.Status -eq 0)
Check "the binary lands in the install directory" (Test-Path (Join-Path $Sandbox.InstallDir "phimint.exe"))
Check "no temp file is left behind" (-not (Test-Path (Join-Path $Sandbox.InstallDir "phimint.exe.tmp")))
Check "the App is asked to write the config skeleton" ((Get-Content $Sandbox.Log -Raw) -match "init --if-missing")
Check "the install source is recorded" (Test-Path (Join-Path $Sandbox.Home ".phimint\state.json"))
Check "the version is reported" ($Run.Output -match "9\.9\.9")

Stop-IndexServer $Server

# ── 2. A checksum mismatch installs nothing ────────────────────────────────
$Sandbox = New-Sandbox
$Server = Start-IndexServer ("0" * 64) $Sandbox.Zip
$Run = Invoke-Installer (Sandbox-Environment $Sandbox $Server.Port $null)
Check "a checksum mismatch fails the install" ($Run.Status -ne 0)
Check "nothing is installed on a mismatch" (-not (Test-Path (Join-Path $Sandbox.InstallDir "phimint.exe")))

Stop-IndexServer $Server

# ── 3. An existing config is never touched ─────────────────────────────────
$Sandbox = New-Sandbox
$Server = Start-IndexServer $Sandbox.Sha $Sandbox.Zip
$PhimintDir = Join-Path $Sandbox.Home ".phimint"
New-Item -ItemType Directory -Force -Path $PhimintDir | Out-Null
Set-Content -Path (Join-Path $PhimintDir "config.json") -Value '{ "api_key": "sk-mine" }'
$Run = Invoke-Installer (Sandbox-Environment $Sandbox $Server.Port $null)
Check "the existing config survives the install" ((Get-Content (Join-Path $PhimintDir "config.json") -Raw) -match "sk-mine")

Stop-IndexServer $Server

# ── 4. The Start Menu shortcut ─────────────────────────────────────────────
$Sandbox = New-Sandbox
$Server = Start-IndexServer $Sandbox.Sha $Sandbox.Zip
# No PHIMINT_NO_SHORTCUT here: this case is about the shortcut. The
# shortcut's folder is the sandbox's, not the runner's real Start Menu:
# a CI runner has none, and writing into a real user's menu to test a
# shortcut is not a thing a test should do (2026-11-13).
$ShortcutDir = Join-Path $Sandbox.Root "programs"
New-Item -ItemType Directory -Force -Path $ShortcutDir | Out-Null
$ShortcutPath = Join-Path $ShortcutDir "phimint.lnk"
$Environment = Sandbox-Environment $Sandbox $Server.Port $null
$Environment.Remove("PHIMINT_NO_SHORTCUT")
$Environment["PHIMINT_SHORTCUT_DIR"] = $ShortcutDir
$Run = Invoke-Installer $Environment
Check "install succeeds with the shortcut" ($Run.Status -eq 0)
Check "the Start Menu shortcut is created" (Test-Path $ShortcutPath)
if (Test-Path $ShortcutPath) {
    $Shell = New-Object -ComObject WScript.Shell
    $Shortcut = $Shell.CreateShortcut($ShortcutPath)
    Check "the shortcut points at the installed binary" ($Shortcut.TargetPath -eq (Join-Path $Sandbox.InstallDir "phimint.exe"))
    Remove-Item -Force $ShortcutPath
}

Stop-IndexServer $Server

Write-Host ""
Write-Host "$script:Passed passed, $script:Failed failed"
if ($script:Failed -gt 0) { exit 1 }
