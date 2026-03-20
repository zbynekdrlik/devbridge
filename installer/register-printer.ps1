# Register DevBridge IPP virtual printer
# Run as Administrator on the server

$PrinterName = "DevBridge"
$PortName = "http://localhost:631/ipp/print"
$DriverName = "Microsoft IPP Class Driver"

# Check if port already exists
$existingPort = Get-PrinterPort -Name $PortName -ErrorAction SilentlyContinue
if (-not $existingPort) {
    Add-PrinterPort -Name $PortName -PrinterHostAddress "localhost"
    Write-Host "Created printer port: $PortName"
}

# Check if printer already exists
$existingPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($existingPrinter) {
    Write-Host "Printer '$PrinterName' already exists."
} else {
    Add-Printer -Name $PrinterName -PortName $PortName -DriverName $DriverName
    Write-Host "Registered printer: $PrinterName"
}

Write-Host "DevBridge printer setup complete."
