# Native Windows setup for the stock analyst skill.
param()

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot
$VenvDir = Join-Path $ScriptDir "venv"
$VenvPython = Join-Path $VenvDir "Scripts\python.exe"
$Requirements = Join-Path $ScriptDir "requirements.txt"
$RequiredModules = @("pandas", "numpy", "matplotlib", "akshare", "markdown", "bleach", "requests", "fpdf", "docx", "PIL")

function Test-Python([string]$Command, [string[]]$PrefixArgs) {
    & $Command @PrefixArgs -c "import sys" *> $null
    return $LASTEXITCODE -eq 0
}

function Test-RequiredModules {
    $Code = "import importlib,sys; [importlib.import_module(name) for name in sys.argv[1:]]"
    & $VenvPython -c $Code @RequiredModules *> $null
    return $LASTEXITCODE -eq 0
}

function Show-Usage {
    Write-Host ""
    Write-Host "Fetch stock data: skill__glmv_stock_analyst__fetch_data"
    Write-Host "Fetch IR documents: skill__glmv_stock_analyst__fetch_ir"
    Write-Host "Generate the report: skill__glmv_stock_analyst__generate_report"
    Write-Host "Do not write or run Skill script commands."
}

if ((Test-Path -LiteralPath $VenvPython -PathType Leaf) -and (Test-RequiredModules)) {
    Write-Host "venv is ready: $VenvPython"
    Show-Usage
    exit 0
}

if ((Test-Path -LiteralPath $VenvDir -PathType Container) -and -not (Test-Path -LiteralPath $VenvPython -PathType Leaf)) {
    throw "The venv directory exists without Scripts\python.exe. Move the broken venv aside and retry."
}

$Candidates = @(
    @{ Command = "py"; Prefix = @("-3") },
    @{ Command = "python"; Prefix = @() },
    @{ Command = "python3"; Prefix = @() }
)
$SystemPython = $null
foreach ($Candidate in $Candidates) {
    $Resolved = Get-Command $Candidate.Command -ErrorAction SilentlyContinue
    if ($null -eq $Resolved -or $Resolved.Source -like "*WindowsApps*") {
        continue
    }
    if (Test-Python $Candidate.Command $Candidate.Prefix) {
        $SystemPython = $Candidate
        break
    }
}
if ($null -eq $SystemPython) {
    throw "Python is unavailable. Install it first: winget install Python.Python.3.12"
}

if (-not (Test-Path -LiteralPath $VenvDir -PathType Container)) {
    Write-Host "Creating the virtual environment..."
    $CreateArgs = @($SystemPython.Prefix) + @("-m", "venv", $VenvDir)
    & $SystemPython.Command @CreateArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to create the virtual environment"
    }
} else {
    Write-Host "The venv exists but is incomplete; installing dependencies in place."
}

if (-not (Test-Path -LiteralPath $VenvPython -PathType Leaf)) {
    throw "The venv Python is still unavailable: $VenvPython"
}

Write-Host "Installing Python dependencies..."
& $VenvPython -m pip install -r $Requirements
if ($LASTEXITCODE -ne 0) {
    throw "Dependency installation failed"
}
if (-not (Test-RequiredModules)) {
    $Code = "import importlib,sys; [importlib.import_module(name) for name in sys.argv[1:]]"
    & $VenvPython -c $Code @RequiredModules
    throw "Required modules remain unavailable after installation"
}

Write-Host "Setup complete."
Show-Usage
exit 0
