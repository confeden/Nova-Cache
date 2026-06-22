# Resolve project root: script is in <root>/scripts/start_service.ps1
$dir = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $dir "target\debug\nova-cache-service.exe"
if (-not (Test-Path $exe)) {
    Write-Host "Service exe not found: $exe"
    exit 1
}
Start-Process -FilePath $exe -ArgumentList "--console" -Verb RunAs
