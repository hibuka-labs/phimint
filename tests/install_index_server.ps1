# The loopback index server the Windows installer contract test runs against.
#
# It is a separate process on purpose. The test starts it with
# Start-Process and stops it with Stop-Process, which ends it at once.
#
# The earlier version lived in a Start-Job and blocked in
# AcceptTcpClient(); Stop-Job could not interrupt that, so every case
# cost two minutes and the job ran for half an hour (2026-11-13).
#
#   pwsh -NoProfile -File tests/install_index_server.ps1 <port-file> <sha> <zip>
#
# It binds an ephemeral port, writes that port to the port file, and then
# serves two paths: the flat index and the archive.

param(
    [Parameter(Mandatory = $true)][string]$PortFile,
    [Parameter(Mandatory = $true)][string]$Sha,
    [Parameter(Mandatory = $true)][string]$ZipPath
)

$ErrorActionPreference = "Stop"

$Listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
$Listener.Start()
$Port = $Listener.LocalEndpoint.Port
$Listener.Stop()

$Listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, $Port)
$Listener.Start()

$IndexText = "version 9.9.9`nwindows-x86_64 http://127.0.0.1:$Port/phimint.zip $Sha`n"
$Zip = [System.IO.File]::ReadAllBytes($ZipPath)

# Tell the caller which port we got, then serve until we are killed.
Set-Content -Path $PortFile -Value "$Port" -NoNewline

while ($true) {
    try {
        $Client = $Listener.AcceptTcpClient()
    } catch {
        break
    }
    try {
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
    } catch {
    } finally {
        $Client.Close()
    }
}
