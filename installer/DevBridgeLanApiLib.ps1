# DevBridge installer function library, part 2: the LAN API of the tills and
# pz-server's [server.acme] (issue #101).
#
# SHIPPING and RULES exactly like DevBridgeInstallerLib.ps1 (its sibling, a
# Tauri resource next to post-install.ps1): functions ONLY, pure ASCII,
# Windows PowerShell 5.1 syntax -- asserted by
# installer/tests/post-install-lib.Tests.ps1. post-install.ps1 dot-sources it
# AFTER DevBridgeInstallerLib.ps1 (it uses ConvertTo-DevBridgeTomlString,
# Merge-DevBridgeTomlTableIntoConfig and the TOML readers from there) and
# exits 1 BEFORE any change when it is missing. A Pester suite or harness
# that AST-extracts a function from here must extract what it calls from the
# other file too.

# ---------------------------------------------------------------------------
# LAN API of the tills (issue #101): the Android pokladna sends its fiscal.*
# jobs (eKasa receipts) to this PC directly, also while the store's internet
# is down. Env:
#   DEVBRIDGE_POKLADNA_KASY -- a JSON array of the tills, written as WHOLE
#     [[client.pokladna.kasy]] blocks (every existing one is replaced):
#       [{"id": 3, "portos_url": "http://localhost:3010", "lan_key": "<key>"}]
#     id = the till's Odoo pos.config id; portos_url optional (default
#     http://localhost:3010); lan_key = the till's LAN API key -- a secret,
#     read from the environment only, never printed; 32-256 printable
#     characters without spaces, quotes or backslashes, one key per till,
#     never the Odoo bridge key; terminal_ip (IPv4, one till per terminal) /
#     terminal_tid (1-16 letters or digits) = the till's Worldline card
#     terminal (issue #102). The parser takes exactly these keys.
#   DEVBRIDGE_LAN_API_ENABLED (true / false, default true),
#   DEVBRIDGE_LAN_API_PORT (default 9443), DEVBRIDGE_LAN_API_HOSTNAME
#     (<client_id>.<zone>, the certificate's name) -> [client.pokladna.lan_api].
#   DEVBRIDGE_POKLADNA_TIM_LIBRARY (the TIM C library file, a licensed
#     Worldline SDK file devbridge never ships), optional
#     DEVBRIDGE_POKLADNA_TIM_INTEGRATOR_ID / _TIM_COMMIT_TIMEOUT_S (50-300)
#     -> [client.pokladna.tim], written WHOLE (issue #102).
# ---------------------------------------------------------------------------

# Parse DEVBRIDGE_POKLADNA_KASY into its entries (PSCustomObjects). Throws on
# anything but a JSON array of objects; the message never shows the value.
# Returns an object[] (unary comma): ASSIGN it before using .Count.
function ConvertFrom-DevBridgePokladnaKasySpec {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Spec)
    if ($Spec.Trim() -notmatch '^\[') {
        throw "DEVBRIDGE_POKLADNA_KASY must be a JSON array of tills, e.g. [{`"id`": 3, `"lan_key`": `"...`"}] (value not shown)"
    }
    # pwsh 7's ConvertFrom-Json (Newtonsoft) accepts a TRUNCATED document
    # ('[{"id": 3' -> one till), which would silently drop the tills after it,
    # so on pwsh 7 the text must first parse with System.Text.Json (strict: no
    # truncation, trailing commas or trailing text). Windows PowerShell 5.1's
    # ConvertFrom-Json (JavaScriptSerializer) is already strict.
    $strictJson = $null
    if ($PSVersionTable.PSVersion.Major -ge 6) {
        Add-Type -AssemblyName System.Text.Json -ErrorAction Stop
        $strictJson = "System.Text.Json.JsonDocument" -as [type]
        if ($null -eq $strictJson) {
            throw "System.Text.Json.JsonDocument is not available to check DEVBRIDGE_POKLADNA_KASY"
        }
    }
    try {
        if ($null -ne $strictJson) {
            $strictJson::Parse($Spec).Dispose()
        }
        $parsed = ConvertFrom-Json -InputObject $Spec -ErrorAction Stop
    } catch {
        throw "DEVBRIDGE_POKLADNA_KASY is not valid JSON (value not shown)"
    }
    # pwsh 7 returns nothing for "[]"; 5.1 returns the array as ONE object.
    if ($null -eq $parsed) { $parsed = @() }
    $entries = @()
    foreach ($item in @($parsed)) {
        if ($item -isnot [System.Management.Automation.PSCustomObject]) {
            throw "DEVBRIDGE_POKLADNA_KASY: every till must be a JSON object (value not shown)"
        }
        $entries += , $item
    }
    return , $entries
}

# Problems with DEVBRIDGE_POKLADNA_KASY (empty list = OK), checked BEFORE any
# change. No key value is ever part of a message. -ApiKey is the Odoo bridge
# key (DEVBRIDGE_POKLADNA_API_KEY), which a till must never share.
function Get-DevBridgePokladnaKasyProblems {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Spec,
        [string]$ApiKey = ""
    )
    $problems = @()
    try {
        $entries = ConvertFrom-DevBridgePokladnaKasySpec -Spec $Spec
    } catch {
        $problems += $_.Exception.Message
        return , $problems
    }
    if ($entries.Count -eq 0) {
        $problems += "DEVBRIDGE_POKLADNA_KASY lists no till"
    }
    $allowed = @("id", "portos_url", "lan_key", "terminal_ip", "terminal_tid")
    $seenIds = @{}
    $seenKeys = [System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)
    $seenTerminals = [System.Collections.Hashtable]::new([System.StringComparer]::Ordinal)
    $octet = '(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])'
    for ($i = 0; $i -lt $entries.Count; $i++) {
        $entry = $entries[$i]
        $where = "DEVBRIDGE_POKLADNA_KASY entry $($i + 1)"
        foreach ($name in @($entry.PSObject.Properties.Name)) {
            if ($allowed -cnotcontains $name) {
                $problems += "${where}: unknown key '$name' (supported: $($allowed -join ', '))"
            }
        }
        $id = $entry.id
        if ((($id -isnot [int]) -and ($id -isnot [long])) -or ($id -le 0)) {
            $problems += "${where}: id must be the till's positive pos.config id (a whole number)"
        } elseif ($seenIds.ContainsKey([long]$id)) {
            $problems += "${where}: id $id is listed twice"
        } else {
            $seenIds[[long]$id] = $true
        }
        if ($null -ne $entry.PSObject.Properties["portos_url"]) {
            $url = $entry.portos_url
            if (($url -isnot [string]) -or ($url -cnotmatch '^https?://[^\s"\\]+\z')) {
                $problems += "${where}: portos_url must be an http(s):// URL (lower-case scheme) without spaces, quotes or backslashes"
            }
        }
        if ($null -ne $entry.PSObject.Properties["lan_key"]) {
            $key = $entry.lan_key
            if (($key -isnot [string]) -or ($key.Length -lt 32) -or ($key.Length -gt 256) -or
                ($key -cnotmatch '^[\x21\x23-\x5B\x5D-\x7E]+\z')) {
                $problems += "${where}: lan_key must be 32-256 printable characters without spaces, quotes or backslashes (value not shown)"
            } elseif ($ApiKey -and ($key -ceq $ApiKey)) {
                $problems += "${where}: lan_key must not be the Odoo bridge key (DEVBRIDGE_POKLADNA_API_KEY)"
            } elseif ($seenKeys.ContainsKey($key)) {
                $problems += "${where}: lan_key is the same as another till's (a key belongs to one till)"
            } else {
                $seenKeys[$key] = $true
            }
        }
        if ($null -ne $entry.PSObject.Properties["terminal_ip"]) {
            $ip = $entry.terminal_ip
            if (($ip -isnot [string]) -or ($ip -cnotmatch "^($octet\.){3}$octet\z")) {
                $problems += "${where}: terminal_ip must be the card terminal's IPv4 address (TIM port 7784 is fixed)"
            } elseif ($seenTerminals.ContainsKey($ip)) {
                $problems += "${where}: terminal_ip $ip is listed for two tills (a terminal belongs to one till)"
            } else {
                $seenTerminals[$ip] = $true
            }
            # the till id is the terminal's POS-ID: at most six digits (EP2, issue #102)
            if ((($id -is [int]) -or ($id -is [long])) -and ($id -gt 999999)) {
                $problems += "${where}: a till with a card terminal needs an id 1-999999 (its POS-ID on the terminal)"
            }
        }
        if ($null -ne $entry.PSObject.Properties["terminal_tid"]) {
            $tid = $entry.terminal_tid
            if (($tid -isnot [string]) -or ($tid -cnotmatch '^[A-Za-z0-9]{1,16}\z')) {
                $problems += "${where}: terminal_tid must be 1-16 letters or digits (a JSON string)"
            }
        }
    }
    return , $problems
}

# The [[client.pokladna.kasy]] blocks of DEVBRIDGE_POKLADNA_KASY (no trailing
# newline). Validated by Get-DevBridgePokladnaKasyProblems first; strings go
# through ConvertTo-DevBridgeTomlString.
function Get-DevBridgePokladnaKasyToml {
    param([Parameter(Mandatory)][string]$Spec)
    $entries = ConvertFrom-DevBridgePokladnaKasySpec -Spec $Spec
    $blocks = @()
    foreach ($entry in $entries) {
        $lines = @("[[client.pokladna.kasy]]", "id = $([long]$entry.id)")
        if ($null -ne $entry.PSObject.Properties["portos_url"]) {
            $lines += "portos_url = $(ConvertTo-DevBridgeTomlString -Value $entry.portos_url)"
        }
        foreach ($name in @("lan_key", "terminal_ip", "terminal_tid")) {
            if ($null -ne $entry.PSObject.Properties[$name]) {
                $lines += "$name = $(ConvertTo-DevBridgeTomlString -Value $entry.$name)"
            }
        }
        $blocks += ($lines -join "`n")
    }
    return ($blocks -join "`n`n")
}

# Put the [[client.pokladna.kasy]] blocks into an EXISTING (preserved)
# config.toml: every existing [[client.pokladna.kasy]] block is removed (the
# list is always given WHOLE -- a new till, a key rotation) and the new blocks
# go before [jobs] (else at the end). Returns 'added' or 'replaced'. Written
# UTF-8 without BOM, keeping the file's line-ending style.
function Merge-DevBridgePokladnaKasyIntoConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Block
    )
    $raw = [System.IO.File]::ReadAllText($Path)
    if ($raw -match "`r`n") { $eol = "`r`n" } elseif ($raw -match "`n") { $eol = "`n" } else { $eol = "`r`n" }
    $blockText = $Block -replace "`r?`n", $eol
    $pattern = '(?ms)^\[\[client\.pokladna\.kasy\]\][^\r\n]*(?:\r?\n|\z).*?(?=^\[|\z)'
    $result = if ([regex]::IsMatch($raw, $pattern)) { "replaced" } else { "added" }
    $rest = [regex]::Replace($raw, $pattern, "")
    $jobs = [regex]::Match($rest, '(?m)^\[jobs\]')
    if ($jobs.Success) {
        $new = $rest.Substring(0, $jobs.Index) + $blockText + $eol + $eol + $rest.Substring($jobs.Index)
    } else {
        $lead = if ($rest.Length -gt 0 -and -not $rest.EndsWith("`n")) { $eol } else { "" }
        $new = $rest + $lead + $eol + $blockText + $eol
    }
    [System.IO.File]::WriteAllText($Path, $new, (New-Object System.Text.UTF8Encoding($false)))
    return $result
}

# Problems with the requested [client.pokladna.tim] (empty list = OK),
# checked BEFORE any change -- the service's own rules (tim.rs): a library
# path, a UUID integrator ID, commit_timeout_s 50-300 (above the bridge's
# 45 s rollback deadline).
function Get-DevBridgePokladnaTimProblems {
    param([string]$Library = "", [string]$IntegratorId = "", [string]$CommitTimeoutS = "")
    $problems = @()
    if (-not $Library -or -not $Library.Trim()) {
        $problems += "DEVBRIDGE_POKLADNA_TIM_LIBRARY must name the TIM C library file ([client.pokladna.tim] is written whole)"
    } elseif ($Library -match '["\x00-\x1F\x7F]') {
        $problems += "DEVBRIDGE_POKLADNA_TIM_LIBRARY must not contain quotes or control characters"
    }
    $hex = '[0-9A-Fa-f]'
    if ($IntegratorId -and ($IntegratorId -cnotmatch "^$hex{8}-$hex{4}-$hex{4}-$hex{4}-$hex{12}\z")) {
        $problems += "DEVBRIDGE_POKLADNA_TIM_INTEGRATOR_ID '$IntegratorId' must be a UUID (8-4-4-4-12 hex digits)"
    }
    if ($CommitTimeoutS -and (($CommitTimeoutS -cnotmatch '^[0-9]{1,3}\z') -or ([int]$CommitTimeoutS -lt 50) -or ([int]$CommitTimeoutS -gt 300))) {
        $problems += "DEVBRIDGE_POKLADNA_TIM_COMMIT_TIMEOUT_S '$CommitTimeoutS' must be a whole number of seconds 50-300"
    }
    return , $problems
}

# The [client.pokladna.tim] block (no trailing newline); validated by
# Get-DevBridgePokladnaTimProblems first. Absent values = the service's
# defaults (integrator ID 7dc442d7-..., commit timeout 60 s).
function Get-DevBridgePokladnaTimToml {
    param([Parameter(Mandatory)][string]$Library, [string]$IntegratorId = "", [string]$CommitTimeoutS = "")
    $lines = @("[client.pokladna.tim]", "library_path = $(ConvertTo-DevBridgeTomlString -Value $Library)")
    if ($IntegratorId) { $lines += "integrator_id = $(ConvertTo-DevBridgeTomlString -Value $IntegratorId.ToLowerInvariant())" }
    if ($CommitTimeoutS) { $lines += "commit_timeout_s = $([int]$CommitTimeoutS)" }
    return ($lines -join "`n")
}

# Whether the [client.pokladna] block keeps the fiscal path (issue #102 lane
# V): DEVBRIDGE_POKLADNA_FISCAL = false is card-only -- the service builds NO
# Portos executor, every fiscal.* job is refused "nothing ran" and the till's
# Portos is never contacted (pjsnvs: no real eKasa receipt there). An
# explicit "true" / "false" (validated by Get-DevBridgePokladnaConfigProblems)
# wins; absent = the preserved config's value ($Existing = its text; "" or no
# key = true), STICKY like test_executor: a key rotation or a forced rewrite
# never re-arms the live Portos. test_executor = true wins over it (service).
# Uses Resolve-DevBridgePokladnaStickyBool of DevBridgeInstallerLib.ps1.
function Resolve-DevBridgePokladnaFiscal {
    param(
        [string]$Requested = "",
        [AllowEmptyString()][string]$Existing = ""
    )
    return (Resolve-DevBridgePokladnaStickyBool -Key "fiscal" -Default $true -Requested $Requested -Existing $Existing)
}

# A lower-case DNS name of at least two labels (pjsnvs.pos.example.sk), at
# most 253 characters, the last label with a letter (no IP address) -- the
# service's own rule for the LAN API hostname. Pure.
function Test-DevBridgeDnsName {
    param([AllowEmptyString()][string]$Name = "")
    if (-not $Name -or $Name.Length -gt 253) { return $false }
    $labels = $Name.Split('.')
    if ($labels.Count -lt 2) { return $false }
    foreach ($label in $labels) {
        if ($label -cnotmatch '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\z') { return $false }
    }
    return [bool]($labels[$labels.Count - 1] -cmatch '[a-z]')
}

# Problems with the requested [client.pokladna.lan_api] settings (empty list =
# OK), checked BEFORE any change. The hostname is needed while it is enabled.
function Get-DevBridgeLanApiConfigProblems {
    param(
        [string]$Enabled = "",
        [string]$Port = "",
        [string]$Hostname = "",
        [int]$DashboardPort = 9120
    )
    $problems = @()
    $on = $true
    if ($Enabled) {
        $value = $Enabled.ToLowerInvariant()
        if (@("true", "false") -cnotcontains $value) {
            $problems += "DEVBRIDGE_LAN_API_ENABLED '$Enabled' must be true or false"
        }
        $on = $value -ne "false"
    }
    if ($Port) {
        if (($Port -notmatch '^[0-9]{1,5}\z') -or ([int]$Port -lt 1) -or ([int]$Port -gt 65535)) {
            $problems += "DEVBRIDGE_LAN_API_PORT '$Port' must be a port number between 1 and 65535"
        } elseif ([int]$Port -eq $DashboardPort) {
            $problems += "DEVBRIDGE_LAN_API_PORT $Port is the dashboard's port"
        }
    }
    if ($on -and -not (Test-DevBridgeDnsName -Name $Hostname)) {
        $problems += "DEVBRIDGE_LAN_API_HOSTNAME '$Hostname' must be the PC's lower-case DNS name <client_id>.<zone> (e.g. pjsnvs.pos.example.sk)"
    }
    return , $problems
}

# The [client.pokladna.lan_api] block (no trailing newline): enabled (default
# true), port and hostname when given. Validated by
# Get-DevBridgeLanApiConfigProblems first.
function Get-DevBridgeLanApiToml {
    param(
        [string]$Enabled = "",
        [string]$Port = "",
        [string]$Hostname = ""
    )
    $on = if ($Enabled) { $Enabled.ToLowerInvariant() } else { "true" }
    $lines = @("[client.pokladna.lan_api]", "enabled = $on")
    if ($Port) { $lines += "port = $([int]::Parse($Port, [System.Globalization.CultureInfo]::InvariantCulture))" }
    if ($Hostname) { $lines += "hostname = $(ConvertTo-DevBridgeTomlString -Value $Hostname)" }
    return ($lines -join "`n")
}

# True when the config file holds a till's LAN key (it must be restricted).
function Test-DevBridgeConfigHasLanKey {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^[ \t]*lan_key[ \t]*=')
}

# Create the inbound TCP rule -DisplayName for -Port, or move an existing one
# (Set-NetFirewallRule) -- same action and profiles either way. Returns
# 'created' or 'updated'.
function Set-DevBridgeFirewallPortRule {
    param(
        [Parameter(Mandatory)][string]$DisplayName,
        [Parameter(Mandatory)][int]$Port,
        [Parameter(Mandatory)][ValidateSet("Allow", "Block")][string]$Action,
        [Parameter(Mandatory)][string[]]$Profiles
    )
    if (Get-NetFirewallRule -DisplayName $DisplayName -ErrorAction SilentlyContinue) {
        Set-NetFirewallRule -DisplayName $DisplayName -Direction Inbound -Protocol TCP -LocalPort $Port `
            -Action $Action -Profile $Profiles -ErrorAction Stop
        return "updated"
    }
    New-NetFirewallRule -DisplayName $DisplayName -Direction Inbound -Protocol TCP -LocalPort $Port `
        -Action $Action -Profile $Profiles -ErrorAction Stop | Out-Null
    return "created"
}

# The LAN API port on a store LAN only: ALLOW on the Private and Domain
# profiles, and BLOCK on Public (rule "<DisplayName>-Public") -- the service
# binary's own allow rule (DevBridge-Service, every profile, every port) would
# otherwise open it on a public network too; a block rule beats an allow
# rule. Returns 'created' or 'updated' (the allow rule).
function Set-DevBridgeLanApiFirewallRule {
    param(
        [Parameter(Mandatory)][int]$Port,
        [string]$DisplayName = "DevBridge-LanApi"
    )
    $allow = Set-DevBridgeFirewallPortRule -DisplayName $DisplayName -Port $Port -Action Allow -Profiles @("Domain", "Private")
    Set-DevBridgeFirewallPortRule -DisplayName "$DisplayName-Public" -Port $Port -Action Block -Profiles @("Public") | Out-Null
    return $allow
}

# Remove both LAN API rules (the API was switched off). Returns how many
# existed.
function Remove-DevBridgeLanApiFirewallRule {
    param([string]$DisplayName = "DevBridge-LanApi")
    $removed = 0
    foreach ($name in @($DisplayName, "$DisplayName-Public")) {
        if (Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue) {
            Remove-NetFirewallRule -DisplayName $name -ErrorAction Stop
            $removed++
        }
    }
    return $removed
}

# Problems of the LAN API as the SERVICE will see it after this install: the
# requested values over the preserved config (issue #101). Mirrors the
# client's start-up check (startup_validation.rs lan_api_problems), which
# would refuse the WHOLE client service -- printing included. -Existing is the
# preserved config text ("" = a fresh config, -ClientId then applies); a
# -LanApiToml / -KasySpec / -PokladnaToml that is given replaces the preserved
# part, -ApiKey is DEVBRIDGE_POKLADNA_API_KEY. No key value is ever part of a
# message.
function Get-DevBridgeLanApiEffectiveProblems {
    param(
        [AllowEmptyString()][string]$Existing = "",
        [string]$ClientId = "",
        [string]$LanApiToml = "",
        [string]$KasySpec = "",
        [string]$PokladnaToml = "",
        [string]$ApiKey = ""
    )
    $problems = @()
    $lanText = if ($LanApiToml) { $LanApiToml } else { Get-DevBridgeTomlTableText -Config $Existing -Table "client.pokladna.lan_api" }
    $pokladnaText = if ($PokladnaToml) { $PokladnaToml } else { Get-DevBridgeTomlTableText -Config $Existing -Table "client.pokladna" }
    # the LAN API runs only with an enabled [client.pokladna]
    if (-not (Get-DevBridgeTomlBoolValue -Text $lanText -Key "enabled") -or
        -not (Get-DevBridgeTomlBoolValue -Text $pokladnaText -Key "enabled")) {
        return , $problems
    }
    $clientText = Get-DevBridgeTomlTableText -Config $Existing -Table "client"
    $clientId = if ($Existing) { Get-DevBridgeTomlStringValue -Text $clientText -Key "client_id" } else { $ClientId }
    $hostname = Get-DevBridgeTomlStringValue -Text $lanText -Key "hostname"
    if (Get-DevBridgeTomlBoolValue -Text $lanText -Key "acme" -Default $true) {
        if (-not $clientId) {
            $problems += "the LAN API needs [client] client_id (DEVBRIDGE_CLIENT_ID): pz-server issues its certificate only for <client_id>.<zone>"
        } elseif (-not $hostname.StartsWith($clientId.ToLowerInvariant() + ".", [System.StringComparison]::Ordinal)) {
            $problems += "DEVBRIDGE_LAN_API_HOSTNAME '$hostname' must be <client_id>.<zone> for client_id '$clientId' -- the only name pz-server issues a certificate for"
        }
    }
    $keys = @()
    if ($KasySpec) {
        foreach ($entry in (ConvertFrom-DevBridgePokladnaKasySpec -Spec $KasySpec)) {
            if ($null -ne $entry.PSObject.Properties["lan_key"]) { $keys += [string]$entry.lan_key }
        }
    } else {
        foreach ($m in [regex]::Matches($Existing, '(?m)^[ \t]*lan_key[ \t]*=[ \t]*"([^"]*)"')) {
            $keys += $m.Groups[1].Value
        }
    }
    if ($keys.Count -eq 0) {
        $problems += "no till has a lan_key (DEVBRIDGE_POKLADNA_KASY) -- no till could call the LAN API"
    }
    $bridgeKey = if ($PokladnaToml) { $ApiKey } else { Get-DevBridgeTomlStringValue -Text $pokladnaText -Key "api_key" }
    if ($bridgeKey -and ($keys -ccontains $bridgeKey)) {
        $problems += "a till's lan_key must not be the Odoo bridge key (DEVBRIDGE_POKLADNA_API_KEY, value not shown)"
    }
    return , $problems
}

# ---------------------------------------------------------------------------
# LAN API certificates of the stores on pz-server, [server.acme] (issue #101):
# Let's Encrypt DNS-01 through Cloudflare. Env (server mode):
# DEVBRIDGE_ACME_ZONE (certificates are <client_id>.<zone>),
# DEVBRIDGE_ACME_CONTACT_EMAIL (optional), DEVBRIDGE_CLOUDFLARE_ZONE_ID and
# DEVBRIDGE_CLOUDFLARE_API_TOKEN (Zone.DNS: Edit on that zone). The token is a
# secret: read from the environment only, never printed, and the config files
# holding it are restricted to SYSTEM + Administrators (a machine-wide
# environment variable would be readable by every local user).
# ---------------------------------------------------------------------------

# Problems with the requested [server.acme] settings (empty list = OK),
# checked BEFORE any change. The token value is NEVER part of a message.
function Get-DevBridgeAcmeConfigProblems {
    param(
        [string]$Zone = "",
        [string]$ContactEmail = "",
        [string]$CloudflareZoneId = "",
        [string]$ApiToken = ""
    )
    $problems = @()
    if (-not (Test-DevBridgeDnsName -Name $Zone)) {
        $problems += "DEVBRIDGE_ACME_ZONE '$Zone' must be a lower-case DNS name such as pos.example.sk"
    }
    if ($ContactEmail -and ($ContactEmail -cnotmatch '^[^\s"\\@]+@[^\s"\\@]+\z')) {
        $problems += "DEVBRIDGE_ACME_CONTACT_EMAIL '$ContactEmail' must be an e-mail address without spaces or quotes"
    }
    if ($CloudflareZoneId -cnotmatch '^[A-Za-z0-9]{1,64}\z') {
        $problems += "DEVBRIDGE_CLOUDFLARE_ZONE_ID '$CloudflareZoneId' must be the Cloudflare zone id (letters and digits)"
    }
    if (-not $ApiToken -or -not $ApiToken.Trim()) {
        $problems += "DEVBRIDGE_CLOUDFLARE_API_TOKEN is not set"
    } elseif ($ApiToken -match '[\s\x00-\x1F\x7F"\\]') {
        $problems += "DEVBRIDGE_CLOUDFLARE_API_TOKEN contains whitespace, quotes, backslashes or control characters (value not shown)"
    }
    return , $problems
}

# The [server.acme] and [server.acme.cloudflare] blocks (no trailing
# newline), as TWO strings: each table is merged on its own into a preserved
# config. Validated by Get-DevBridgeAcmeConfigProblems first.
function Get-DevBridgeAcmeToml {
    param(
        [Parameter(Mandatory)][string]$Zone,
        [string]$ContactEmail = "",
        [Parameter(Mandatory)][string]$CloudflareZoneId,
        [Parameter(Mandatory)][string]$ApiToken
    )
    $acme = @("[server.acme]", "enabled = true", "zone = $(ConvertTo-DevBridgeTomlString -Value $Zone)")
    if ($ContactEmail) { $acme += "contact_email = $(ConvertTo-DevBridgeTomlString -Value $ContactEmail)" }
    $cloudflare = @(
        "[server.acme.cloudflare]",
        "zone_id = $(ConvertTo-DevBridgeTomlString -Value $CloudflareZoneId)",
        "api_token = $(ConvertTo-DevBridgeTomlString -Value $ApiToken)"
    )
    return , @(($acme -join "`n"), ($cloudflare -join "`n"))
}

# True when the config file holds the Cloudflare API token (it must be
# restricted).
function Test-DevBridgeConfigHasAcmeToken {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return [bool]([System.IO.File]::ReadAllText($Path) -match '(?m)^[ \t]*api_token[ \t]*=')
}

# The LAN API part of a client install (issue #101), validated BEFORE any
# change: DEVBRIDGE_POKLADNA_KASY (-KasySpec), DEVBRIDGE_LAN_API_* (-Enabled /
# -Port / -Hostname), DEVBRIDGE_POKLADNA_TIM_* (-Tim*, issue #102) and,
# whenever any of them or the [client.pokladna] block (-PokladnaToml)
# changes, the effective check over the preserved config (-Existing, "" =
# fresh). Returns a hashtable: Problems (empty = OK), KasyToml, LanApiToml,
# TimToml ("" = not requested), FirewallPort (> 0 = open it),
# SwitchedOff (remove the rules), Notes (lines to print -- never a key).
function Get-DevBridgeLanApiRequest {
    param(
        [AllowEmptyString()][string]$KasySpec = "",
        [string]$Enabled = "",
        [string]$Port = "",
        [string]$Hostname = "",
        [int]$DashboardPort = 9120,
        [string]$ApiKey = "",
        [string]$ClientId = "",
        [AllowEmptyString()][string]$Existing = "",
        [string]$PokladnaToml = "",
        [string]$TimLibrary = "",
        [string]$TimIntegratorId = "",
        [string]$TimCommitTimeoutS = ""
    )
    $request = @{ Problems = @(); KasyToml = ""; LanApiToml = ""; TimToml = ""; FirewallPort = 0; SwitchedOff = $false; Notes = @() }
    $lanRequested = [bool]($Enabled -or $Port -or $Hostname)
    $timRequested = [bool]($TimLibrary -or $TimIntegratorId -or $TimCommitTimeoutS)
    $problems = @()
    if ($timRequested) {
        $problems += Get-DevBridgePokladnaTimProblems -Library $TimLibrary -IntegratorId $TimIntegratorId -CommitTimeoutS $TimCommitTimeoutS
    }
    if ($KasySpec) {
        $problems += Get-DevBridgePokladnaKasyProblems -Spec $KasySpec -ApiKey $ApiKey
    }
    if ($lanRequested) {
        $problems += Get-DevBridgeLanApiConfigProblems -Enabled $Enabled -Port $Port -Hostname $Hostname -DashboardPort $DashboardPort
    }
    if ($problems.Count -gt 0) {
        $request.Problems = $problems
        return $request
    }
    if ($KasySpec) {
        $request.KasyToml = Get-DevBridgePokladnaKasyToml -Spec $KasySpec
        $count = ([regex]::Matches($request.KasyToml, '(?m)^\[\[client\.pokladna\.kasy\]\]')).Count
        $request.Notes += "Pokladna tills requested: $count (LAN keys set, not shown)"
    }
    if ($lanRequested) {
        $request.LanApiToml = Get-DevBridgeLanApiToml -Enabled $Enabled -Port $Port -Hostname $Hostname
        if (Get-DevBridgeTomlBoolValue -Text $request.LanApiToml -Key "enabled") {
            $request.FirewallPort = if ($Port) { [int]$Port } else { 9443 }
        } else {
            $request.SwitchedOff = $true
        }
        $request.Notes += "LAN API requested: enabled='$Enabled' port='$Port' hostname='$Hostname' (empty = enabled, port 9443)"
    }
    if ($timRequested) {
        $request.TimToml = Get-DevBridgePokladnaTimToml -Library $TimLibrary -IntegratorId $TimIntegratorId -CommitTimeoutS $TimCommitTimeoutS
        $found = if (Test-Path -LiteralPath $TimLibrary -PathType Leaf) { "found" } else { "NOT FOUND yet -- the terminal reports offline until it is there" }
        $request.Notes += "Card terminal (TIM) requested: library '$TimLibrary' ($found), integrator_id='$TimIntegratorId' commit_timeout_s='$TimCommitTimeoutS' (empty = defaults)"
        $pokladnaText = if ($PokladnaToml) { $PokladnaToml } else { Get-DevBridgeTomlTableText -Config $Existing -Table "client.pokladna" }
        if (-not (Get-DevBridgeTomlBoolValue -Text $pokladnaText -Key "enabled")) {
            $request.Notes += "WARNING: [client.pokladna] is not enabled -- the card terminal stays unused until DEVBRIDGE_POKLADNA_URL / _API_KEY are given"
        }
    }
    if ($request.KasyToml -or $request.LanApiToml -or $PokladnaToml) {
        $request.Problems = Get-DevBridgeLanApiEffectiveProblems -Existing $Existing -ClientId $ClientId `
            -LanApiToml $request.LanApiToml -KasySpec $KasySpec -PokladnaToml $PokladnaToml -ApiKey $ApiKey
    }
    return $request
}

# The [server.acme] part of a server install (issue #101), validated BEFORE
# any change. Returns a hashtable: Problems (empty = OK), Blocks (the two
# tables, see Get-DevBridgeAcmeToml; empty when invalid), Note (a line to
# print -- never the token).
function Get-DevBridgeAcmeRequest {
    param(
        [string]$Zone = "",
        [string]$ContactEmail = "",
        [string]$CloudflareZoneId = "",
        [string]$ApiToken = ""
    )
    $request = @{ Problems = @(); Blocks = @(); Note = "" }
    $problems = Get-DevBridgeAcmeConfigProblems -Zone $Zone -ContactEmail $ContactEmail `
        -CloudflareZoneId $CloudflareZoneId -ApiToken $ApiToken
    if ($problems.Count -gt 0) {
        $request.Problems = $problems
        return $request
    }
    $request.Blocks = Get-DevBridgeAcmeToml -Zone $Zone -ContactEmail $ContactEmail `
        -CloudflareZoneId $CloudflareZoneId -ApiToken $ApiToken
    $request.Note = "[server.acme] requested: zone '$Zone', Cloudflare zone '$CloudflareZoneId' (API token set, not shown)"
    return $request
}

# A warning per network connection on the Public profile: the LAN API port is
# BLOCKED there, so a store LAN classified Public (Windows' default for an
# unidentified network) silently cuts the tills off.
function Get-DevBridgePublicNetworkWarnings {
    $warnings = @()
    foreach ($connection in @(Get-NetConnectionProfile -ErrorAction SilentlyContinue)) {
        if ("$($connection.NetworkCategory)" -eq "Public") {
            $warnings += "network '$($connection.InterfaceAlias)' is Public: the LAN API port is BLOCKED there -- make the store LAN Private (Set-NetConnectionProfile -InterfaceAlias '$($connection.InterfaceAlias)' -NetworkCategory Private)"
        }
    }
    return , $warnings
}

# ---------------------------------------------------------------------------
# Files of the file.write device jobs (issue #130): the Codex EDI ORDER files
# a SERVER-mode bridge (the Codex server) writes into Codex' import folder.
# Env DEVBRIDGE_POKLADNA_FILE_WRITE_DIR = that folder (absolute, local, must
# EXIST -- the installer never creates it) -> [client.pokladna.file_write],
# merged on its own (a key rotation without it keeps the folder). In SERVER
# mode DEVBRIDGE_POKLADNA_URL + _API_KEY are accepted too ([client.pokladna]
# with fiscal = false: the server runs file.* jobs only); the store settings
# (test executor, fiscal = true, tills, TIM, LAN API) are refused there.
# ---------------------------------------------------------------------------

# Problems with a requested file folder (empty list = OK), mirroring the
# service's target_dir_problem: an absolute local path (C:\...), no '..'
# component, no network share, no quote / control character -- and it must
# exist as a folder (read-only Test-Path).
function Get-DevBridgeFileWriteDirProblems {
    param([AllowEmptyString()][string]$Dir = "")
    $problems = @()
    if ($Dir -match '[\x00-\x1F\x7F"]') {
        $problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR contains a quote or a control character"
    } elseif ($Dir -match '^(\\\\|//)') {
        $problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR '$Dir' must be a local folder, not a network share"
    } elseif ($Dir -notmatch '^[A-Za-z]:[\\/]') {
        $problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR '$Dir' must be an absolute path (e.g. C:\folder)"
    } elseif (@($Dir -split '[\\/]') -contains '..') {
        $problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR '$Dir' must not contain a '..' component"
    } elseif (-not (Test-Path -LiteralPath $Dir -PathType Container)) {
        $problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR '$Dir' does not exist (the installer never creates it)"
    }
    return ,$problems
}

# The [client.pokladna.file_write] TOML block (no trailing newline). Validated
# by Get-DevBridgeFileWriteDirProblems first.
function Get-DevBridgeFileWriteToml {
    param([Parameter(Mandatory)][string]$Dir)
    return ("[client.pokladna.file_write]`ntarget_dir = " + (ConvertTo-DevBridgeTomlString -Value $Dir))
}

# The requested [client.pokladna] (issue #99) + [client.pokladna.file_write]
# (issue #130) for -Mode client | server, checked BEFORE any change
# (read-only). $Existing = the current config.toml text ("" on a fresh
# install; also read on a forced rewrite -- the sticky switches survive it).
# Returns @{ Problems; Toml; FileWriteToml; Notes; Warnings }. The API key
# value is never part of a message.
function Get-DevBridgePokladnaRequest {
    param(
        [Parameter(Mandatory)][string]$Mode,
        [string]$Url = "",
        [string]$ApiKey = "",
        [string]$WaitS = "",
        [string]$TestExecutor = "",
        [string]$Fiscal = "",
        [string]$FileWriteDir = "",
        [AllowEmptyString()][string]$Existing = ""
    )
    $r = @{ Problems = @(); Toml = ""; FileWriteToml = ""; Notes = @(); Warnings = @() }
    if (-not $Url -and -not $ApiKey) {
        # [client.pokladna] is written only WHOLE: its parts alone are refused / dropped.
        if ($FileWriteDir) { $r.Problems += "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR needs DEVBRIDGE_POKLADNA_URL and DEVBRIDGE_POKLADNA_API_KEY too ([client.pokladna] is written whole)" }
        if ($Mode -eq "client" -and ($TestExecutor -or $Fiscal)) {
            $r.Problems += "DEVBRIDGE_POKLADNA_TEST_EXECUTOR needs DEVBRIDGE_POKLADNA_URL and DEVBRIDGE_POKLADNA_API_KEY too, so does DEVBRIDGE_POKLADNA_FISCAL ([client.pokladna] is written whole)"
        }
        if ($WaitS) { $r.Warnings += "DEVBRIDGE_POKLADNA_WAIT_S is IGNORED: [client.pokladna] is written only as a whole block -- give DEVBRIDGE_POKLADNA_URL and DEVBRIDGE_POKLADNA_API_KEY too" }
        return $r
    }
    $problems = Get-DevBridgePokladnaConfigProblems -Url $Url -ApiKey $ApiKey -WaitS $WaitS -TestExecutor $TestExecutor -Fiscal $Fiscal
    $r.Problems += $problems
    if ($FileWriteDir) {
        $dirProblems = Get-DevBridgeFileWriteDirProblems -Dir $FileWriteDir
        $r.Problems += $dirProblems
    }
    if ($Mode -eq "server") {
        if ($TestExecutor) { $r.Problems += "DEVBRIDGE_POKLADNA_TEST_EXECUTOR is a store setting -- a server-mode bridge runs file.* jobs only" }
        if ($Fiscal -and $Fiscal.ToLowerInvariant() -eq "true") { $r.Problems += "DEVBRIDGE_POKLADNA_FISCAL=true is a store setting -- a server has no Portos (it runs file.* jobs only)" }
        $hasFolder = $FileWriteDir -or ($Existing -match '(?m)^\[client\.pokladna\.file_write\][ \t]*(?:#[^\r\n]*)?\r?$')
        if (-not $hasFolder) { $r.Problems += "a server-mode bridge runs file.* jobs only: give DEVBRIDGE_POKLADNA_FILE_WRITE_DIR too" }
        $testMode = $false
        $fiscalOn = $false
    } else {
        $testMode = Resolve-DevBridgePokladnaTestExecutor -Requested $TestExecutor -Existing $Existing
        $fiscalOn = Resolve-DevBridgePokladnaFiscal -Requested $Fiscal -Existing $Existing
    }
    if ($r.Problems.Count -gt 0) { return $r }
    $r.Toml = Get-DevBridgePokladnaToml -Url $Url -ApiKey $ApiKey -WaitS $WaitS -TestExecutor $testMode -Fiscal $fiscalOn
    if ($FileWriteDir) { $r.FileWriteToml = Get-DevBridgeFileWriteToml -Dir $FileWriteDir }
    $r.Notes += "Odoo device-job source (pokladna) requested: $Url, wait_s='$WaitS' (empty = service default 25; API key set, not shown), test mode without Portos: $testMode (requested '$TestExecutor', empty = kept), fiscal (false = card-only, no Portos): $fiscalOn (requested '$Fiscal', empty = kept), file.write folder: '$FileWriteDir' (empty = kept), $Mode mode"
    return $r
}
