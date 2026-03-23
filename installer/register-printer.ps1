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

# Try Add-Printer -ConnectionName (creates proper IPP port)
$registered = $false
$urls = @($IppUrl, "http://localhost:${IppPort}/ipp/print")
foreach ($url in $urls) {
    if ($registered) { break }
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Write-Host "Add-Printer -ConnectionName '$url' (attempt $attempt)..."
            Add-Printer -ConnectionName $url -ErrorAction Stop
            Start-Sleep -Seconds 2
            $connPrinter = Get-Printer | Where-Object {
                $_.PortName -like "*$IppPort/ipp*"
            } | Select-Object -First 1
            if ($connPrinter) {
                if ($connPrinter.Name -ne $PrinterName) {
                    Rename-Printer -Name $connPrinter.Name -NewName $PrinterName -ErrorAction SilentlyContinue
                }
                $registered = $true
                break
            }
        } catch {
            Write-Host "Attempt $attempt failed: $($_.Exception.Message)" -ForegroundColor Yellow
            Start-Sleep -Seconds 2
        }
    }
}

# Fallback: rundll32 printui.dll
if (-not $registered) {
    Write-Host "Trying rundll32 printui.dll fallback..."
    $printUiArgs = "/if /b `"$PrinterName`" /r `"$IppUrl`" /m `"Microsoft IPP Class Driver`" /q"
    $proc = Start-Process -FilePath "rundll32.exe" `
        -ArgumentList "printui.dll,PrintUIEntry $printUiArgs" `
        -Wait -PassThru -NoNewWindow -ErrorAction SilentlyContinue
    if ($proc -and $proc.ExitCode -eq 0) {
        Write-Host "WARNING: Used rundll32 fallback - printing may not work via spooler" -ForegroundColor Yellow
    }
}

# Verify
$verifyPrinter = Get-Printer -Name $PrinterName -ErrorAction SilentlyContinue
if ($verifyPrinter) {
    Write-Host "Verified: name='$PrinterName' port='$($verifyPrinter.PortName)' driver='$($verifyPrinter.DriverName)' type='$($verifyPrinter.Type)'" -ForegroundColor Green
} else {
    Write-Host "WARNING: Printer registration could not be verified" -ForegroundColor Yellow
}

Write-Host "DevBridge printer setup complete."
