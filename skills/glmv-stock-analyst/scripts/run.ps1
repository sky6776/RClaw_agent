# Invoke a skill script while preserving the agent workspace cwd.
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$ScriptName,

    [Parameter(Position = 1, ValueFromRemainingArguments = $true)]
    [string[]]$ScriptArgs
)

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot

if ([IO.Path]::GetFileName($ScriptName) -ne $ScriptName) {
    throw "ScriptName must be a file name within the scripts directory"
}

$Target = Join-Path $ScriptDir $ScriptName
if (-not (Test-Path -LiteralPath $Target -PathType Leaf)) {
    throw "The skill script does not exist: $Target"
}

$Python = Join-Path $ScriptDir "venv\Scripts\python.exe"
if (-not (Test-Path -LiteralPath $Python -PathType Leaf)) {
    throw "The skill venv Python is unavailable; run setup.ps1 first."
}

& $Python $Target @ScriptArgs
exit $LASTEXITCODE
