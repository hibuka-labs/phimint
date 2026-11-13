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
# Note: the shortcut case writes into the real Start Menu of whoever runs
# this and removes it again. On CI that is a throwaway runner; on a desktop
# it is one .lnk that comes and goes.

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

# A loopback HTTP server that serves the index and the zip. Raw sockets, so
# no URL reservation is needed on Windows.
#
# The index text is built inside the job: it names the port, and the port is
# only known once the listener is bound.
function Start-IndexServer([string]$Sha, [string]$ZipPath) {
    $Listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    $Port = $Listener.LocalEndpoint.Port
    $Listener.Stop()

    $Job = Start-Job -ArgumentList $Port, $Sha, $ZipPath -ScriptBlock {
        param($Port, $Sha, $ZipPath)
        $IndexText = "version 9.9.9`nwindows-x86_64 http://127.0.0.1:$Port/phimint.zip $Sha`n"
        $Listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, $Port)
        $Listener.Start()
        $Zip = [System.IO.File]::ReadAllBytes($ZipPath)
        while ($true) {
            $Client = $Listener.AcceptTcpClient()
            $Stream = $Client.GetStream()
            $Reader = New-Object System.IO.StreamReader($Stream)
            $RequestLine = $Reader.ReadLine()
            while ($true) {
                $Header = $Reader.ReadLine()
                if ($null -eq $Header -or $Header -eq "") { break }
            }
            $Body = $null
            $Type = "application/octet-stream"
            if ($RequestLine -like "*index.txt*") {
                $Body = [System.Text.Encoding]::UTF8.GetBytes($IndexText)
                $Type = "text/plain"
            } elseif ($RequestLine -like "*.zip*") {
                $Body = $Zip
            }
            if ($null -eq $Body) {
                $Head = "HTTP/1.1 404 Not Found`r`nContent-Length: 0`r`nConnection: close`r`n`r`n"
                $Bytes = [System.Text.Encoding]::ASCII.GetBytes($Head)
            } else {
                $Head = "HTTP/1.1 200 OK`r`nContent-Type: $Type`r`nContent-Length: $($Body.Length)`r`nConnection: close`r`n`r`n"
                $Bytes = ([System.Text.Encoding]::ASCII.GetBytes($Head)) + $Body
            }
            $Stream.Write($Bytes, 0, $Bytes.Length)
            $Stream.Flush()
            $Client.Close()
        }
    }
    return @{ Port = $Port; Job = $Job }
}

function Stop-IndexServer($Server) {
    Stop-Job $Server.Job -ErrorAction SilentlyContinue
    Remove-Job $Server.Job -Force -ErrorAction SilentlyContinue
}

# Run the real installer in a child process with a controlled environment.
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
$Programs = [Environment]::GetFolderPath("Programs")
$ShortcutPath = Join-Path $Programs "phimint.lnk"
if (Test-Path $ShortcutPath) { Remove-Item -Force $ShortcutPath }

$Sandbox = New-Sandbox
$Server = Start-IndexServer $Sandbox.Sha $Sandbox.Zip
# No PHIMINT_NO_SHORTCUT here: this case is about the shortcut.
$Environment = Sandbox-Environment $Sandbox $Server.Port $null
$Environment.Remove("PHIMINT_NO_SHORTCUT")
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
