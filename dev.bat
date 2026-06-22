@echo off
cd /d "%~dp0"
set "ROOT=%~dp0"

echo ============================================
echo   Nova Cache - Development Launcher
echo ============================================
echo.

echo [1/6] Cleaning up old processes...
sc stop Novacache >nul 2>&1
fltmc unload Novacache >nul 2>&1
taskkill /F /IM nova-cache-gui.exe >nul 2>&1
taskkill /F /IM nova-cache-service.exe >nul 2>&1

echo [2/6] Installing test signing certificate...
certutil -addstore My "%ROOT%NovaCacheTest.cer" >nul 2>&1
if %errorLevel% equ 0 (echo Certificate installed.) else (echo Certificate already installed or skipped.)

echo [3/6] Locating build tools...
set "MSBUILD="
for /f "usebackq tokens=*" %%i in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.Component.MSBuild -property installationPath 2^>nul`) do (
    for /f "usebackq tokens=*" %%j in (`"%%i\MSBuild\Current\Bin\amd64\MSBuild.exe" --version 2^>nul`) do (
        set "MSBUILD=%%i\MSBuild\Current\Bin\amd64\MSBuild.exe"
    )
)
if "%MSBUILD%"=="" (
    set "MSBUILD=%ProgramFiles(x86)%\Microsoft Visual Studio\18\BuildTools\MSBuild\Current\Bin\amd64\MSBuild.exe"
)
set "SIGNTOOL=%ProgramFiles(x86)%\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe"
if not exist "%SIGNTOOL%" (
    for /f "usebackq tokens=*" %%k in (`dir /s /b "%ProgramFiles(x86)%\Windows Kits\10\bin\*\x64\signtool.exe" 2^>nul`) do (
        set "SIGNTOOL=%%k"
    )
)
echo   MSBuild: %MSBUILD%
echo   Signtool: %SIGNTOOL%

echo [4/6] Building driver...
"%MSBUILD%" "%ROOT%driver\novacache\Novacache.vcxproj" "/p:Configuration=Release" "/p:Platform=x64" "/t:Build" "/v:minimal" /m
if %errorLevel% neq 0 (
    echo DRIVER BUILD FAILED. Press any key to exit.
    pause >nul
    exit /b 1
)
echo Signing driver...
"%SIGNTOOL%" sign /fd SHA256 /s My /n NovaCacheTest "%ROOT%driver\novacache\Release\Novacache.sys" >nul 2>&1
if %errorLevel% neq 0 (
    "%SIGNTOOL%" sign /fd SHA256 /s My /n NovaCacheTest "%ROOT%driver\novacache\Release\Novacache.sys"
)
echo Driver built and signed.

echo Building Rust (service + GUI)...
cargo build --bin nova-cache-service --bin nova-cache-gui
if %errorLevel% neq 0 (
    echo RUST BUILD FAILED. Press any key to exit.
    pause >nul
    exit /b 1
)
echo All built OK.

echo [5/6] Starting service as admin...
start "NovaCache-Service" /min powershell -NoProfile -ExecutionPolicy Bypass -Command "cd '%ROOT%'; cargo run --bin nova-cache-service -- --console"

echo [6/6] Waiting for driver to load...
:wait_driver
timeout /t 1 /nobreak >nul
fltmc filters 2>nul | findstr /I "Novacache" >nul 2>&1
if %errorLevel% neq 0 goto wait_driver
echo Driver loaded!

echo Starting GUI...
start "" cargo run --bin nova-cache-gui -- --no-launch

echo.
echo ============================================
echo   All launched. Close GUI to stop service.
echo ============================================
