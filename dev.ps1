param([switch]$Force)
$ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path
$STATE_FILE = Join-Path $ROOT ".dev_state.json"

# --- State --------------------------------------------------------
function Read-State {
    if (-not (Test-Path $STATE_FILE)) { return @{} }
    try {
        $raw = Get-Content $STATE_FILE -Raw -EA 0
        if (-not $raw) { return @{} }
        $obj = $raw | ConvertFrom-Json -EA 0
        if (-not $obj -or $obj -isnot [PSCustomObject]) { return @{} }
        $ht = @{}
        $obj.PSObject.Properties | ForEach-Object { $ht[$_.Name] = $_.Value }
        return $ht
    } catch { return @{} }
}
function Get-State($name) { $s = Read-State; if ($s.ContainsKey($name)) { $s[$name] } else { $false } }
function Set-State($name, $val) {
    try { $s = Read-State; $s[$name] = $val; $s | ConvertTo-Json | Set-Content $STATE_FILE -EA 0 }
    catch { Write-Host "  WARNING: state save failed for $name" }
}

# â”€â”€â”€ Helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
function Log($msg) { Write-Host ">>> $msg" -ForegroundColor Cyan }
function Ok($msg) { Write-Host "  OK  $msg" -ForegroundColor Green }
function Skip($msg) { Write-Host "  --  $msg (skipped)" -ForegroundColor Yellow }
function Fail($msg) { Write-Host "  FAIL $msg" -ForegroundColor Red; exit 1 }
function Check-Admin { ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]"Administrator") }

function NewerThan($target, $sources) {
    if (-not (Test-Path $target)) { return $false }
    $t = (Get-Item $target).LastWriteTime
    foreach ($src in $sources) { if ((Get-Item $src).LastWriteTime -gt $t) { return $false } }
    $true
}

function Find-MSBuild {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $path = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild -find "MSBuild\**\Bin\*.exe" 2>$null
        if ($path) {
            $exe = $path | Where-Object { $_ -like "*MSBuild.exe" } | Select-Object -First 1
            if ($exe) { return $exe }
        }
    }
    # Fallback: common VS install locations
    $fallbacks = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\MSBuild\*\Bin\amd64\MSBuild.exe"
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\MSBuild\*\Bin\amd64\MSBuild.exe"
        "${env:ProgramFiles}\Microsoft Visual Studio\*\*\MSBuild\*\Bin\amd64\MSBuild.exe"
        "${env:ProgramFiles}\Microsoft Visual Studio\*\MSBuild\*\Bin\amd64\MSBuild.exe"
    )
    foreach ($pattern in $fallbacks) {
        $found = Get-ChildItem $pattern -ErrorAction 0 | Sort-Object FullName -Descending | Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    # Fallback: check PATH
    $fromPath = Get-Command MSBuild.exe -EA 0
    if ($fromPath) { return $fromPath.Source }
    $null
}

function Find-Signtool {
    $paths = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\*\x64\signtool.exe"
    )
    $found = Get-ChildItem $paths -ErrorAction 0 | Sort-Object Name -Descending | Select-Object -First 1
    if ($found) { return $found.FullName }
    # fallback recursive search
    $found = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction 0 | Sort-Object DirectoryName -Descending | Select-Object -First 1
    if ($found) { return $found.FullName }
    $null
}

# â”€â”€â”€ Steps â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

function Step-Rust {
    Log "Step 1/6: Rust toolchain"
    $have = Get-Command rustc -EA 0
    if ($have -and (Get-State "rust")) { Skip "Rust already installed"; return }

    if (-not $have) {
        Log "  Downloading rustup-init.exe..."
        $url = "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
        $dst = "$env:TEMP\rustup-init.exe"
        Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing
        Log "  Installing Rust (defaults)..."
        Start-Process $dst -Wait -ArgumentList "-y --default-host x86_64-pc-windows-msvc --default-toolchain stable"
        Remove-Item $dst -EA 0
        $env:Path = [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")
        if (-not (Get-Command rustc -EA 0)) { Fail "Rust installation failed. Try manually: https://rustup.rs" }
    }
    Ok "rustc $(& rustc --version 2>$null)"
    Set-State "rust" $true
}

function Step-VsWdk {
    Log "Step 2/6: Visual Studio Build Tools + WDK"
    $msbuild = Find-MSBuild
    if ($msbuild) { Ok "MSBuild: $msbuild" }
    else {
        Log "  MSBuild not found. Attempting install via winget..."
        $winget = Get-Command winget -EA 0
        if ($winget) {
            Log "  Installing VS 2022 Build Tools (C++ workload)..."
            & winget install Microsoft.VisualStudio.2022.BuildTools --silent --accept-package-agreements 2>$null
            if ($LASTEXITCODE -ne 0) {
                Log "  winget failed, trying direct download..."
                $url = "https://aka.ms/vs/17/release/vs_BuildTools.exe"
                $dst = "$env:TEMP\vs_BuildTools.exe"
                Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing
                Start-Process $dst -Wait -ArgumentList "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --includeRecommended"
            }
            $msbuild = Find-MSBuild
            if (-not $msbuild) { Fail "VS Build Tools installed but MSBuild not found. Try manually: https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022" }
        } else { Fail "MSBuild not found. Install VS 2022 Build Tools: https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022" }
    }
    Set-State "msbuild" $msbuild

    $signtool = Find-Signtool
    if ($signtool) { Ok "Signtool: $signtool" }
    else {
        Log "  WDK signtool not found. Attempting WDK download..."
        $url = "https://go.microsoft.com/fwlink/?linkid=2273610"
        $dst = "$env:TEMP\wdk.exe"
        try {
            Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing
            Start-Process $dst -Wait -ArgumentList "/quiet /install"
            $signtool = Find-Signtool
        } catch { Log "  WDK download failed. Install manually: https://learn.microsoft.com/en-us/windows-hardware/drivers/download-the-wdk" }
        if (-not $signtool) { Log "  WARNING: signtool not found. Driver signing will be skipped." }
    }
    Set-State "signtool" $signtool
}

function Step-BuildDriver {
    Log "Step 3/6: Build driver"
    $msbuild = Get-State "msbuild"
    if (-not $msbuild) { Fail "MSBuild path not found. Run Step-VsWdk first." }

    $sys = "$ROOT\driver\novacache\Release\Novacache.sys"
    $sources = @(Get-ChildItem "$ROOT\driver\novacache\*.c","$ROOT\driver\novacache\*.h","$ROOT\driver\novacache\*.vcxproj" -ErrorAction 0).FullName
    if ((-not $Force) -and (NewerThan $sys $sources)) { Skip "driver up to date"; return }

    Log "  Building driver (this may take a while)..."
    & $msbuild "$ROOT\driver\novacache\Novacache.vcxproj" "/p:Configuration=Release" "/p:Platform=x64" "/t:Build" "/v:minimal" /m
    if ($LASTEXITCODE -ne 0) { Fail "Driver build failed. Check MSBuild output above." }
    if (-not (Test-Path $sys)) { Fail "Novacache.sys not found after build" }
    Ok "Novacache.sys built"
}

function Step-BuildRust {
    Log "Step 4/6: Build Rust binaries"
    $bins = @("nova-cache-service.exe","nova-cache-gui.exe")
    $allUp = $true
    foreach ($bin in $bins) {
        $p = "$ROOT\target\release\$bin"
        if ($Force -or -not (Test-Path $p)) { $allUp = $false; break }
    }
    if ($allUp) { Skip "Rust binaries up to date"; return }

    Log "  cargo build --release..."
    Push-Location $ROOT
    cargo build --release --bin nova-cache-service --bin nova-cache-gui 2>&1 | ForEach-Object { Write-Host "    $_" }
    if ($LASTEXITCODE -ne 0) { Pop-Location; Fail "Rust build failed" }
    Pop-Location
    Ok "Rust binaries built"
}

function Step-Sign {
    Log "Step 5/6: Sign driver"
    $signtool = Get-State "signtool"
    if (-not $signtool) { Log "  WARNING: signtool not available, skipping sign"; return }

    $sys = "$ROOT\driver\novacache\Release\Novacache.sys"
    if (-not (Test-Path $sys)) { Fail "$sys not found - build driver first" }

    # Check if already signed (Authenticode signature present and valid)
    $sig = Get-AuthenticodeSignature $sys -EA 0
    if ($sig.Status -eq "Valid" -and -not $Force) { Skip "already signed"; return }

    # Stop service and unload driver to release file lock
    sc stop Novacache 2>$null
    Start-Sleep -Seconds 2
    fltmc unload Novacache 2>$null

    # Create test cert (PowerShell only - no dialog, no signtool create)
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Subject -match "NovaCacheTest" } | Select-Object -First 1
    if (-not $cert) {
        Log "  Creating self-signed test certificate..."
        $cert = New-SelfSignedCertificate `
            -Type Custom `
            -Subject "CN=NovaCacheTest" `
            -KeyUsage DigitalSignature `
            -CertStoreLocation "Cert:\CurrentUser\My" `
            -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3") `
            -NotAfter (Get-Date).AddYears(10) `
            -Confirm:$false
        if (-not $cert) { Fail "Failed to create test certificate" }
        Ok "Certificate created"
    }

    # Add to Trusted Root (silent - no dialog)
    $rootStore = [System.Security.Cryptography.X509Certificates.X509Store]::new("Root", "CurrentUser")
    $rootStore.Open("ReadWrite")
    $existing = $rootStore.Certificates | Where-Object { $_.Subject -match "NovaCacheTest" }
    if (-not $existing) {
        $rootStore.Add($cert)
        Log "  Added certificate to Trusted Root"
    }
    $rootStore.Close()

    # Add to Trusted Publishers (for driver signing)
    $pubStore = [System.Security.Cryptography.X509Certificates.X509Store]::new("TrustedPublisher", "CurrentUser")
    $pubStore.Open("ReadWrite")
    $existing = $pubStore.Certificates | Where-Object { $_.Subject -match "NovaCacheTest" }
    if (-not $existing) {
        $pubStore.Add($cert)
        Log "  Added certificate to Trusted Publishers"
    }
    $pubStore.Close()

    Log "  Signing Novacache.sys..."
    & $signtool sign /fd SHA256 /a /n "NovaCacheTest" $sys 2>&1
    if ($LASTEXITCODE -ne 0) { Fail "Signing failed" }
    Ok "Novacache.sys signed"
}

function Step-Run {
    Log "Step 6/6: Start service + GUI"

    # Clean up old instances
    sc stop Novacache 2>$null
    fltmc unload Novacache 2>$null
    taskkill /F /IM nova-cache-gui.exe 2>$null
    taskkill /F /IM nova-cache-service.exe 2>$null

    # Start service in console mode
    Log "  Starting nova-cache-service (console mode)..."
    $svcDir = "$ROOT\target\release"
    if (-not (Test-Path "$svcDir\nova-cache-service.exe")) {
        # try debug
        $svcDir = "$ROOT\target\debug"
    }
    $ps = Start-Process -FilePath "$svcDir\nova-cache-service.exe" -ArgumentList "--console" -WindowStyle Hidden -PassThru -RedirectStandardOutput "$env:TEMP\nova_svc.log" -RedirectStandardError "$env:TEMP\nova_svc_err.log"
    Start-Sleep -Seconds 2

    # Wait for driver to load
    Log "  Waiting for minifilter to register..."
    $timeout = 30
    while ($timeout -gt 0) {
        $loaded = fltmc filters 2>$null | Select-String "Novacache"
        if ($loaded) { break }
        Start-Sleep -Seconds 1
        $timeout--
    }
    if ($timeout -eq 0) { Log "  WARNING: minifilter not detected. Check $env:TEMP\nova_svc_err.log" }
    else { Ok "Novacache minifilter registered" }

    # Start GUI
    Log "  Starting nova-cache-gui..."
    Start-Process -FilePath "$svcDir\nova-cache-gui.exe" -ArgumentList "--no-launch" -WindowStyle Normal
    Start-Sleep -Seconds 1
    Ok "Nova Cache is running. GUI should appear shortly."
}

# â”€â”€â”€ Entry â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
function Main {
    if (-not (Check-Admin)) { Fail "Must run as Administrator" }

    Log "Nova Cache - Automated Build & Setup"
    Log "Root: $ROOT"
    ""

    if ($Force) { Log "Force mode: ignoring cached state" }

    Step-Rust
    Step-VsWdk
    Step-BuildDriver
    Step-BuildRust
    Step-Sign
    Step-Run

    ""
    Log "Done. Nova Cache is active."
    ""
    Log "To stop: taskkill /F /IM nova-cache-service.exe"
}

Main
