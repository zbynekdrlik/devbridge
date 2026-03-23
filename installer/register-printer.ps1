# Register DevBridge IPP virtual printer
# Run as Administrator on the server

$PrinterName = "DevBridge"
$IppPort = 631
$IppUrl = "http://127.0.0.1:${IppPort}/ipp/print"

# Remove existing printers and ports
Get-Printer | Where-Object {
    $_.Name -eq $PrinterName -or $_.PortName -like "*$IppPort/ipp*"
} | ForEach-Object {
    Write-Host "Removing printer '$($_.Name)'..."
    Remove-Printer -Name $_.Name -ErrorAction SilentlyContinue
}
Get-PrinterPort | Where-Object { $_.Name -like "*$IppPort/ipp*" } | ForEach-Object {
    Write-Host "Removing port '$($_.Name)'..."
    Remove-PrinterPort -Name $_.Name -ErrorAction SilentlyContinue
}

# Wait for IPP server HTTP readiness
Write-Host "Waiting for IPP server on port $IppPort..."
for ($i = 0; $i -lt 15; $i++) {
    try {
        $response = Invoke-WebRequest -Uri $IppUrl -Method POST `
            -ContentType "application/ipp" -Body ([byte[]](1,1,0,0x0b,0,0,0,1,3)) `
            -UseBasicParsing -TimeoutSec 3 -ErrorAction Stop
        if ($response.StatusCode -eq 200) {
            Write-Host "IPP server ready (attempt $($i+1))"
            break
        }
    } catch {
        Start-Sleep -Seconds 1
    }
}

# Register printer using rundll32 printui.dll which creates a proper
# Internet Port (inetpp.dll) when given an HTTP URL as the port name.
$printUiArgs = "/if /b `"$PrinterName`" /r `"$IppUrl`" /m `"Microsoft IPP Class Driver`" /q"
Write-Host "Running: rundll32 printui.dll,PrintUIEntry $printUiArgs"
$proc = Start-Process -FilePath "rundll32.exe" `
    -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
    -Wait -PassThru -NoNewWindow -ErrorAction SilentlyContinue
if ($proc -and $proc.ExitCode -eq 0) {
    Write-Host "Registered printer via printui.dll" -ForegroundColor Green
} else {
    Write-Host "printui.dll failed (exit code: $($proc.ExitCode))" -ForegroundColor Yellow
}

# Verify
$verifyPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($verifyPrinter) {
    Write-Host "Verified: name='$PrinterName' port='$($verifyPrinter.PortName)' driver='$($verifyPrinter.DriverName)' type='$($verifyPrinter.Type)'" -ForegroundColor Green
    $port = Get-PrinterPort -Name $verifyPrinter.PortName -ErrorAction SilentlyContinue
    if ($port) {
        Write-Host "Port: host='$($port.PrinterHostAddress)' desc='$($port.Description)' monitor='$($port.PortMonitor)'"
    }
} else {
    Write-Host "WARNING: Printer registration could not be verified" -ForegroundColor Yellow
}

Write-Host "DevBridge printer setup complete."
