# Register DevBridge IPP virtual printer
# Run as Administrator on the server

$PrinterName = "DevBridge"
$IppPort = 631
$IppUrl = "http://localhost:${IppPort}/ipp/print"

# Remove existing printer for clean re-registration
$existingPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($existingPrinter) {
    Write-Host "Removing existing printer '$PrinterName'..."
    Remove-Printer -Name $PrinterName -ErrorAction SilentlyContinue
}
Get-Printer | Where-Object { $_.PortName -like "*localhost*$IppPort*" -and $_.Name -ne $PrinterName } | ForEach-Object {
    Write-Host "Removing leftover printer '$($_.Name)'..."
    Remove-Printer -Name $_.Name -ErrorAction SilentlyContinue
}
Get-PrinterPort | Where-Object { $_.Name -like "*localhost*$IppPort*" } | ForEach-Object {
    Write-Host "Removing leftover port '$($_.Name)'..."
    Remove-PrinterPort -Name $_.Name -ErrorAction SilentlyContinue
}

# Wait for IPP server
Write-Host "Waiting for IPP server on port $IppPort..."
$ippReady = $false
for ($i = 0; $i -lt 10; $i++) {
    try {
        $tcp = New-Object System.Net.Sockets.TcpClient
        $tcp.Connect("localhost", $IppPort)
        $tcp.Close()
        $ippReady = $true
        break
    } catch {
        Start-Sleep -Seconds 1
    }
}
if (-not $ippReady) {
    Write-Host "WARNING: IPP server not responding on port $IppPort" -ForegroundColor Yellow
}

# Use Add-Printer -ConnectionName to create a proper IPP printer connection.
$registered = $false
try {
    Write-Host "Creating IPP connection to $IppUrl..."
    Add-Printer -ConnectionName $IppUrl -ErrorAction Stop
    Start-Sleep -Seconds 2
    $connPrinter = Get-Printer | Where-Object {
        $_.PortName -like "*localhost*$IppPort*" -or $_.PortName -eq $IppUrl
    } | Select-Object -First 1
    if ($connPrinter) {
        if ($connPrinter.Name -ne $PrinterName) {
            Rename-Printer -Name $connPrinter.Name -NewName $PrinterName -ErrorAction SilentlyContinue
            Write-Host "Renamed '$($connPrinter.Name)' -> '$PrinterName'" -ForegroundColor Green
        }
        $registered = $true
    }
} catch {
    Write-Host "Add-Printer -ConnectionName failed: $_" -ForegroundColor Yellow
}

# Fallback: rundll32 printui.dll
if (-not $registered) {
    Write-Host "Trying rundll32 printui.dll fallback..."
    $printUiArgs = "/if /b `"$PrinterName`" /r `"$IppUrl`" /m `"Microsoft IPP Class Driver`" /q"
    $proc = Start-Process -FilePath "rundll32.exe" `
        -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
        -Wait -PassThru -NoNewWindow -ErrorAction SilentlyContinue
    if ($proc -and $proc.ExitCode -eq 0) {
        $registered = $true
    }
}

# Verify
$verifyPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($verifyPrinter) {
    Write-Host "Verified: '$PrinterName' port='$($verifyPrinter.PortName)' driver='$($verifyPrinter.DriverName)'" -ForegroundColor Green
} else {
    Write-Host "WARNING: Printer registration could not be verified" -ForegroundColor Yellow
}

Write-Host "DevBridge printer setup complete."
