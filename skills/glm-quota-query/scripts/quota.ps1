# glm-quota-query: Query Zhipu GLM Coding Plan quota (Windows PowerShell)
# Endpoint: /api/monitor/usage/quota/limit (internal monitoring endpoint, not in public docs)
# The secret is read ONLY from skills/glmv-stock-analyst/config.toml, never printed.
# DESIGN NOTES (hard constraints, learned from harness behavior):
# 1. PURE ASCII only. PS 5.1 parses BOM-less files as ANSI/GBK, and GBK decoding
#    of UTF-8 comments can swallow newlines (breaking line-based parsing).
# 2. The harness scrubber rewrites secret-like tokens to [REDACTED] in text the
#    model reads/writes. Keep key handling on plain string ops so one bad patch
#    cannot corrupt the file.
# 3. Read config lines with -Encoding UTF8 explicitly (PS 5.1 defaults to ANSI).
# 4. Resolve the config by probing candidates ($PSScriptRoot-relative first,
#    then USERPROFILE); never hardcode a machine-specific absolute path.
param([switch]$ModelUsage)

# --- Locate config.toml by probing fixed candidates ---
$ConfigCandidates = @()
if ($PSScriptRoot) {
    $ConfigCandidates += (Join-Path $PSScriptRoot "..\glmv-stock-analyst\config.toml")
}
if ($env:USERPROFILE) { $ConfigCandidates += ($env:USERPROFILE + "\.rclaw\agent\skills\glmv-stock-analyst\config.toml") }

$Config = ""
foreach ($C in $ConfigCandidates) {
    if (-Not $Config) {
        if (Test-Path -LiteralPath $C) { $Config = $C }
    }
}
if (-Not $Config) {
    Write-Output ("ERROR config.toml not found. cwd=" + (Get-Location).Path + " userprofile=" + $env:USERPROFILE)
    exit 1
}

# --- Read the secret with plain string ops (NO regex captures) ---
$K = ""
foreach ($Line in (Get-Content -LiteralPath $Config -Encoding UTF8)) {
    $T = $Line.Trim()
    if (-Not $T.StartsWith("api_key")) { continue }
    $Eq = $T.IndexOf([char]61)
    if ($Eq -lt 0) { continue }
    $V = $T.Substring($Eq + 1).Trim()
    if ($V.Length -gt 0) {
        $V = $V.Trim([char]34, [char]39).Trim()
        if ($V.Length -gt 0) { $K = $V; break }
    }
}
if ($K.Length -eq 0) { Write-Output "ERROR secret not found in config.toml"; exit 1 }

$Headers = @{ Authorization = ("Bearer" + " " + $K) }

function Get-ResetInfo([double]$Ms) {
    if ($Ms -le 0) { return "unknown" }
    $Sec = [long]($Ms / 1000)
    $Reset = [DateTimeOffset]::FromUnixTimeSeconds($Sec).ToLocalTime()
    $Diff = $Reset - [DateTimeOffset]::Now
    if ($Diff.TotalSeconds -lt 0) { $Diff = [TimeSpan]::Zero }
    return ("{0:yyyy-MM-dd HH:mm} (resets in ~{1}h {2}m)" -f $Reset, [int]$Diff.TotalHours, $Diff.Minutes)
}

$Body = $null
foreach ($Base in @("https://open.bigmodel.cn", "https://bigmodel.cn")) {
    try {
        $R = Invoke-RestMethod -Uri ($Base + "/api/monitor/usage/quota/limit") -Headers $Headers -TimeoutSec 20
        if ($R.success -or $R.code -eq 200) { $Body = $R; break }
    } catch { }
}
if (-Not $Body) { Write-Output "ERROR quota endpoint failed on both domains"; exit 2 }

Write-Output "=============================================="
Write-Output " GLM Coding Plan Quota"
Write-Output (" Time: " + (Get-Date -Format "yyyy-MM-dd HH:mm:ss"))
$LevelName = switch ($Body.data.level) { "lite" { "Lite" } "pro" { "Pro" } "max" { "Max" } default { "$($Body.data.level)" } }
Write-Output (" Plan: " + $LevelName)
Write-Output "=============================================="

foreach ($Lim in @($Body.data.limits)) {
    if ($Lim.type -eq "TOKENS_LIMIT") {
        $Title = switch ($Lim.unit) { 3 { "5-hour Token window" } 6 { "Weekly Token window" } default { "Token window (unit=$($Lim.unit))" } }
        $Rem = [math]::Round(100 - [double]$Lim.percentage, 1)
        Write-Output ""
        Write-Output ("[$Title]")
        Write-Output ("  Used: " + $Lim.percentage + "%    Remaining: " + $Rem + "%")
        Write-Output ("  Reset: " + (Get-ResetInfo ([double]$Lim.nextResetTime)))
    }
}

$Mcp = @($Body.data.limits) | Where-Object { $_.type -eq "TIME_LIMIT" } | Select-Object -First 1
if ($Mcp) {
    Write-Output ""
    Write-Output "[MCP monthly quota] (search-prime / web-reader / zread)"
    Write-Output ("  Used: " + $Mcp.percentage + "%")
    Write-Output ("  Progress: " + $Mcp.currentValue + " / " + $Mcp.usage + " calls, remaining " + $Mcp.remaining)
    Write-Output ("  Reset: " + (Get-ResetInfo ([double]$Mcp.nextResetTime)))
    if ($Mcp.usageDetails) {
        Write-Output "  Details:"
        foreach ($D in @($Mcp.usageDetails)) {
            Write-Output ("    - " + $D.modelCode + ": used " + $D.usage)
        }
    }
    Write-Output "  (note: usage=total quota, currentValue=used, remaining=remaining; field names per platform)"
}

if ($ModelUsage) {
    Write-Output ""
    Write-Output "----------------------------------------------"
    Write-Output "[24h model usage] (model-usage)"
    $Mu = $null
    foreach ($Base in @("https://open.bigmodel.cn", "https://bigmodel.cn")) {
        try { $Mu = Invoke-RestMethod -Uri ($Base + "/api/monitor/usage/model-usage") -Headers $Headers -TimeoutSec 20; break } catch { }
    }
    if (-Not $Mu) { Write-Output "  No data (endpoint may return empty body when there is no usage)" }
    else { $Mu | ConvertTo-Json -Depth 6 }
}

Write-Output ""
Write-Output "=============================================="
Write-Output " Note: percentage = used percent; remaining = 100 - used."
Write-Output " Internal monitoring endpoint; structure may change without notice."
