# Register DevBridge IPP virtual printer
# Run as Administrator on the server

$PrinterName = "DevBridge"
$IppUrl = "http://localhost:631/ipp/print"

# Remove existing printer for clean re-registration
$existingPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($existingPrinter) {
    Write-Host "Removing existing printer for re-registration..."
    Remove-Printer -Name $PrinterName -ErrorAction SilentlyContinue
}

# Clean up any leftover port
$existingPort = Get-PrinterPort -Name $IppUrl -ErrorAction SilentlyContinue
if ($existingPort) {
    Remove-PrinterPort -Name $IppUrl -ErrorAction SilentlyContinue
}

# Use rundll32 printui.dll to add printer with proper IPP port handling.
# Unlike Add-PrinterPort which creates a Standard TCP/IP port (TCPMON.DLL),
# printui.dll creates a proper IPP port that speaks HTTP POST to the URL.
$printUiArgs = "/if /b `"$PrinterName`" /r `"$IppUrl`" /m `"Microsoft IPP Class Driver`" /q"
Write-Host "Running: rundll32 printui.dll,PrintUIEntry $printUiArgs"
$proc = Start-Process -FilePath "rundll32.exe" `
    -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
    -Wait -PassThru -NoNewWindow -ErrorAction Stop
if ($proc.ExitCode -eq 0) {
    Write-Host "Registered printer via printui.dll: $PrinterName" -ForegroundColor Green
} else {
    Write-Host "printui.dll returned exit code $($proc.ExitCode), trying fallback..." -ForegroundColor Yellow
    Add-Printer -ConnectionName $IppUrl -ErrorAction SilentlyContinue
    $fallbackPrinter = Get-Printer | Where-Object { $_.PortName -eq $IppUrl } | Select-Object -First 1
    if ($fallbackPrinter -and $fallbackPrinter.Name -ne $PrinterName) {
        Rename-Printer -Name $fallbackPrinter.Name -NewName $PrinterName -ErrorAction SilentlyContinue
    }
}

# Verify
$verifyPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($verifyPrinter) {
    Write-Host "Verified: '$PrinterName' registered with port '$($verifyPrinter.PortName)'" -ForegroundColor Green
} else {
    Write-Host "WARNING: Printer registration could not be verified" -ForegroundColor Yellow
}

Write-Host "DevBridge printer setup complete."
