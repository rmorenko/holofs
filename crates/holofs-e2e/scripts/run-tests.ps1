# crates/holofs-e2e/scripts/run-tests.ps1 — Windows runner.
#
# Mirrors run-tests.sh: builds the gateway if needed, starts a local
# chromedriver if HOLOFS_E2E_WEBDRIVER isn't pointing somewhere else,
# runs `cargo test -p holofs-e2e -- --test-threads=1`, and tears the
# chromedriver child down on exit.
#
# Usage examples (PowerShell 7+):
#   .\run-tests.ps1
#   .\run-tests.ps1 ui_catalog::tree_loads
#   $env:HOLOFS_E2E_HEADED=1; .\run-tests.ps1

[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments=$true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Resolve-Path (Join-Path $ScriptDir "..\..\..")
Set-Location $RepoRoot

$WebdriverUrl   = if ($env:HOLOFS_E2E_WEBDRIVER) { $env:HOLOFS_E2E_WEBDRIVER } else { "http://localhost:9515" }
$DriverPort     = if ($env:CHROMEDRIVER_PORT)    { $env:CHROMEDRIVER_PORT }    else { 9515 }
$Exe            = Join-Path $RepoRoot "target\release\holofs-web.exe"

# Step 1 — make sure the gateway binary exists.
if (-not (Test-Path $Exe)) {
    Write-Host "→ building holofs-web (release)..."
    cargo build --release --features ssr --bin holofs-web
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

# Step 2 — start chromedriver if we own its endpoint.
$ChildProc = $null
$cleanup = {
    if ($ChildProc -and -not $ChildProc.HasExited) {
        try { $ChildProc.Kill() } catch { }
        try { $ChildProc.WaitForExit(3000) | Out-Null } catch { }
    }
}
try {
    $localUrls = @("http://localhost:$DriverPort", "http://127.0.0.1:$DriverPort")
    if ($localUrls -contains $WebdriverUrl) {
        $statusOk = $false
        try {
            Invoke-WebRequest "$WebdriverUrl/status" -TimeoutSec 1 | Out-Null
            $statusOk = $true
        } catch { }
        if (-not $statusOk) {
            $cd = Get-Command chromedriver -ErrorAction SilentlyContinue
            if (-not $cd) {
                Write-Error "chromedriver not found on PATH. Try `choco install chromedriver` or `scoop install chromedriver`, or run docker-compose -f crates/holofs-e2e/docker-compose.yml up -d."
                exit 2
            }
            Write-Host "→ starting chromedriver on port $DriverPort..."
            $log = Join-Path $env:TEMP "holofs-e2e-chromedriver.log"
            $ChildProc = Start-Process chromedriver `
                -ArgumentList "--port=$DriverPort","--silent" `
                -PassThru -RedirectStandardOutput $log -RedirectStandardError $log
            $deadline = (Get-Date).AddSeconds(10)
            while ((Get-Date) -lt $deadline) {
                try {
                    Invoke-WebRequest "$WebdriverUrl/status" -TimeoutSec 1 | Out-Null
                    $statusOk = $true
                    break
                } catch { Start-Sleep -Milliseconds 200 }
            }
            if (-not $statusOk) {
                Write-Error "chromedriver never became ready. See $log."
                exit 3
            }
        }
    }

    Write-Host "→ webdriver:  $WebdriverUrl"
    Write-Host "→ gateway:    $Exe"

    # Step 3 — run the suite.
    $argsList = @("test","-p","holofs-e2e","--","--test-threads=1") + $CargoArgs
    & cargo @argsList
    exit $LASTEXITCODE
}
finally {
    & $cleanup
}
