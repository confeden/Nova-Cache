@echo off
cd /d "%~dp0"

:: Check for admin — re-launch if not
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo Requesting administrator privileges...
    powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process cmd -Verb RunAs -ArgumentList '/c cd /d \"%~dp0\" && \"%~f0\"'"
    exit /b 0
)

:: Pass through flags
set "FLAGS="
if /I "%~1"=="--force" set "FLAGS=-Force"
if /I "%~1"=="-f" set "FLAGS=-Force"

:: Launch the automation script
powershell -NoProfile -ExecutionPolicy Bypass -Command "& '%~dp0dev.ps1' %FLAGS%"
if %errorLevel% neq 0 (
    echo.
    echo Something went wrong. Check the output above.
    pause
    exit /b 1
)
