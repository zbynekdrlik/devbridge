# Install the Portos eKasa API (NineDigit) on a FRESH GitHub-hosted
# windows-latest runner and switch it to InMemory storage + Playground
# (integration) environment, for the `portos-live` CI job (issue #100).
#
# NEVER run this on a store PC: it reconfigures Portos' storage. It refuses to
# run outside GitHub Actions (GITHUB_ACTIONS=true) unless -Force.
#
# Steps (each fails the job loudly):
#   1. download the pinned MSI and verify its SHA256;
#   2. msiexec /qn (headless), service NineDigit.eKasa.API running, the API
#      answering (settings - product/info needs the storage, see below);
#   3. PUT /api/v1/settings storage.storageModel=InMemory,
#      eKasaClient.environment=Playground, restart the service, product/info
#      answers 200, read the settings back;
#   4. pick the InMemory identity whose certificate is valid, print its expiry,
#      export DEVBRIDGE_PORTOS_URL + DEVBRIDGE_PORTOS_CASH_REGISTER_CODE to
#      GITHUB_ENV for the test step.
#
# Facts (vendor docs / INTEGRATOR.md of ninedigit/ekasa-clientphp):
# InMemory storage is initialized with integration (test) data and is wiped
# on every service restart; Playground = the eKasa integration environment.

param(
    [Parameter(Mandatory)][string]$MsiUrl,
    [Parameter(Mandatory)][string]$MsiSha256,
    [string]$BaseUrl = "http://localhost:3010",
    [string]$ServiceName = "NineDigit.eKasa.API",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

if ($env:GITHUB_ACTIONS -ne "true" -and -not $Force) {
    throw "ci-portos-inmemory.ps1 reconfigures Portos storage - CI runners only (GITHUB_ACTIONS=true)"
}

# GET $Path until it answers 200 (the API is up). Right after the install the
# storage is the default CHDU on COM3, which a runner does not have:
# product/info answers 500 then, settings already 200.
function Wait-PortosApi {
    param(
        [string]$Path = "/api/v1/product/info",
        [int]$TimeoutSeconds = 180
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastError = ""
    while ((Get-Date) -lt $deadline) {
        try {
            return Invoke-RestMethod -Uri "$BaseUrl$Path" -TimeoutSec 10
        } catch {
            $lastError = $_.Exception.Message
            Start-Sleep -Seconds 3
        }
    }
    throw "Portos API at $BaseUrl did not answer $Path within $TimeoutSeconds s (last error: $lastError)"
}

function Wait-PortosService {
    param([int]$TimeoutSeconds = 120)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
        if ($svc) {
            if ($svc.Status -eq "Running") { return $svc }
            if ($svc.Status -eq "Stopped") { Start-Service -Name $ServiceName }
        }
        Start-Sleep -Seconds 2
    }
    throw "service $ServiceName is not running after $TimeoutSeconds s"
}

# -- 1. download + verify ------------------------------------------------------
$msi = Join-Path $env:RUNNER_TEMP "portos-ekasa-api.msi"
for ($i = 1; $i -le 5; $i++) {
    try {
        Invoke-WebRequest -Uri $MsiUrl -OutFile $msi -UseBasicParsing
        break
    } catch {
        if ($i -eq 5) { throw }
        Start-Sleep -Seconds (3 * $i)
    }
}
$hash = (Get-FileHash -Path $msi -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "MSI: $MsiUrl ($((Get-Item $msi).Length) bytes, sha256 $hash)"
if ($hash -ne $MsiSha256.ToLowerInvariant()) {
    throw "MSI sha256 $hash does not match the pinned $MsiSha256 - refusing to install"
}

# -- 2. headless install -------------------------------------------------------
$log = Join-Path $env:RUNNER_TEMP "portos-msiexec.log"
$p = Start-Process -FilePath "msiexec.exe" -ArgumentList "/i `"$msi`" /qn /norestart /l*v `"$log`"" -Wait -PassThru
Write-Host "msiexec exit code: $($p.ExitCode)"
if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {
    if (Test-Path $log) { Get-Content $log -Tail 80 | Write-Host }
    throw "msiexec failed with exit code $($p.ExitCode) - Portos cannot be installed headless on this runner"
}
$svc = Wait-PortosService
Write-Host "service $ServiceName : $($svc.Status) (start type $($svc.StartType))"

# -- 3. InMemory + Playground --------------------------------------------------
$before = Wait-PortosApi -Path "/api/v1/settings"
Write-Host "settings before: storage=$($before.storage | ConvertTo-Json -Compress) eKasaClient=$($before.eKasaClient | ConvertTo-Json -Compress)"
$body = @{
    storage     = @{ storageModel = "InMemory" }
    eKasaClient = @{ environment = "Playground" }
} | ConvertTo-Json -Depth 5
Invoke-RestMethod -Uri "$BaseUrl/api/v1/settings" -Method Put -Body $body -ContentType "application/json" -TimeoutSec 30 | Out-Null
Restart-Service -Name $ServiceName
Wait-PortosService | Out-Null
$info = Wait-PortosApi -Path "/api/v1/product/info"
Write-Host "product/info (InMemory): $($info | ConvertTo-Json -Depth 5 -Compress)"
$after = Invoke-RestMethod -Uri "$BaseUrl/api/v1/settings" -TimeoutSec 30
Write-Host "settings after: storage=$($after.storage | ConvertTo-Json -Compress) eKasaClient=$($after.eKasaClient | ConvertTo-Json -Compress)"
if ("$($after.storage.storageModel)" -ne "InMemory" -or "$($after.eKasaClient.environment)" -ne "Playground") {
    throw "Portos did not switch to InMemory + Playground (storage=$($after.storage.storageModel), environment=$($after.eKasaClient.environment))"
}

# -- 4. the InMemory identity and its certificate -------------------------------
$identities = @(Invoke-RestMethod -Uri "$BaseUrl/api/v1/identities" -TimeoutSec 30)
Write-Host "identities: $($identities | ConvertTo-Json -Depth 6 -Compress)"
$code = $null
foreach ($identity in $identities) {
    $candidate = "$($identity.organizationUnit.cashRegisterCode)"
    if (-not $candidate) { continue }
    try {
        $cert = Invoke-RestMethod -Uri "$BaseUrl/api/v1/certificates/valid/latest?cashRegisterCode=$candidate" -TimeoutSec 30
    } catch {
        Write-Host "cash register $candidate : no valid certificate ($($_.Exception.Message))"
        continue
    }
    Write-Host "cash register $candidate : certificate valid until $($cert.expirationDate) (isExpired=$($cert.isExpired))"
    if (-not $cert.isExpired) {
        $code = $candidate
        $expires = [DateTimeOffset]::Parse("$($cert.expirationDate)")
        $days = [int]($expires - [DateTimeOffset]::Now).TotalDays
        if ($days -lt 30) {
            Write-Host "::warning::the Portos InMemory test certificate of $candidate expires in $days days ($($cert.expirationDate)) - every vendor MSI (7.0.4-9.0.6) bundles this same identity: get a renewed test identity from NineDigit (issue #101)"
        }
        break
    }
}
if (-not $code) {
    throw "no InMemory identity with a valid certificate - the pinned Portos MSI's test identity has expired"
}
"DEVBRIDGE_PORTOS_URL=$BaseUrl" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
"DEVBRIDGE_PORTOS_CASH_REGISTER_CODE=$code" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
$printer = Invoke-RestMethod -Uri "$BaseUrl/api/v1/printers/status" -TimeoutSec 30
Write-Host "printers/status: $($printer | ConvertTo-Json -Compress)"
Write-Host "Portos ready: InMemory + Playground, cash register $code"
