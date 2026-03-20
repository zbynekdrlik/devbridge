# Generate mTLS certificates for DevBridge
# Requires: openssl in PATH
# Run from the project root or installer directory

param(
    [string]$OutputDir = "certs",
    [int]$Days = 3650,
    [string]$CACommonName = "DevBridge CA",
    [string]$ServerCommonName = "devbridge-server",
    [string]$ClientCommonName = "devbridge-client"
)

$ErrorActionPreference = "Stop"

# Ensure output directory exists
if (-not (Test-Path $OutputDir)) {
    New-Item -ItemType Directory -Path $OutputDir | Out-Null
    Write-Host "Created output directory: $OutputDir"
}

# Verify openssl is available
try {
    $null = & openssl version
} catch {
    Write-Error "openssl is not installed or not in PATH. Please install OpenSSL and try again."
    exit 1
}

# --- Certificate Authority ---
Write-Host "`n==> Generating CA key and certificate..."

& openssl genrsa -out "$OutputDir/ca.key" 4096
if ($LASTEXITCODE -ne 0) { throw "Failed to generate CA key" }

& openssl req -new -x509 -key "$OutputDir/ca.key" -out "$OutputDir/ca.crt" `
    -days $Days -subj "/CN=$CACommonName/O=DevBridge"
if ($LASTEXITCODE -ne 0) { throw "Failed to generate CA certificate" }

Write-Host "CA certificate: $OutputDir/ca.crt"

# --- Server Certificate ---
Write-Host "`n==> Generating server key and certificate..."

& openssl genrsa -out "$OutputDir/server.key" 2048
if ($LASTEXITCODE -ne 0) { throw "Failed to generate server key" }

# Create server CSR
& openssl req -new -key "$OutputDir/server.key" -out "$OutputDir/server.csr" `
    -subj "/CN=$ServerCommonName/O=DevBridge"
if ($LASTEXITCODE -ne 0) { throw "Failed to generate server CSR" }

# Create server extensions file for SAN
$serverExtFile = "$OutputDir/server_ext.cnf"
@"
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names

[alt_names]
DNS.1 = localhost
DNS.2 = $ServerCommonName
IP.1 = 127.0.0.1
"@ | Set-Content -Path $serverExtFile

# Sign server certificate with CA
& openssl x509 -req -in "$OutputDir/server.csr" -CA "$OutputDir/ca.crt" -CAkey "$OutputDir/ca.key" `
    -CAcreateserial -out "$OutputDir/server.crt" -days $Days `
    -extfile $serverExtFile -extensions v3_req
if ($LASTEXITCODE -ne 0) { throw "Failed to sign server certificate" }

Write-Host "Server certificate: $OutputDir/server.crt"

# --- Client Certificate ---
Write-Host "`n==> Generating client key and certificate..."

& openssl genrsa -out "$OutputDir/client.key" 2048
if ($LASTEXITCODE -ne 0) { throw "Failed to generate client key" }

# Create client CSR
& openssl req -new -key "$OutputDir/client.key" -out "$OutputDir/client.csr" `
    -subj "/CN=$ClientCommonName/O=DevBridge"
if ($LASTEXITCODE -ne 0) { throw "Failed to generate client CSR" }

# Create client extensions file
$clientExtFile = "$OutputDir/client_ext.cnf"
@"
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
"@ | Set-Content -Path $clientExtFile

# Sign client certificate with CA
& openssl x509 -req -in "$OutputDir/client.csr" -CA "$OutputDir/ca.crt" -CAkey "$OutputDir/ca.key" `
    -CAcreateserial -out "$OutputDir/client.crt" -days $Days `
    -extfile $clientExtFile -extensions v3_req
if ($LASTEXITCODE -ne 0) { throw "Failed to sign client certificate" }

Write-Host "Client certificate: $OutputDir/client.crt"

# --- Cleanup temporary files ---
Remove-Item -Path "$OutputDir/*.csr" -ErrorAction SilentlyContinue
Remove-Item -Path "$OutputDir/*.cnf" -ErrorAction SilentlyContinue
Remove-Item -Path "$OutputDir/*.srl" -ErrorAction SilentlyContinue

Write-Host "`n==> Certificate generation complete."
Write-Host "Files in $OutputDir:"
Get-ChildItem $OutputDir | ForEach-Object { Write-Host "  $_" }
