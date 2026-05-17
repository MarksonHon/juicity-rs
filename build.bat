@echo off
REM ============================================
REM Juicity-RS Build Script for Windows
REM ============================================

echo [Juicity-RS] Building project...

REM Check if Rust/Cargo is installed
where cargo >nul 2>&1
if %ERRORLEVEL% neq 0 (
    echo [ERROR] Cargo not found! Please install Rust from https://rustup.rs/
    exit /b 1
)

REM Build in release mode by default
set BUILD_MODE=%1
if "%BUILD_MODE%"=="" set BUILD_MODE=release

if /I "%BUILD_MODE%"=="debug" (
    echo [Juicity-RS] Build mode: debug
    cargo build
) else if /I "%BUILD_MODE%"=="release" (
    echo [Juicity-RS] Build mode: release
    cargo build --release
) else (
    echo [ERROR] Unknown build mode: %BUILD_MODE%. Use "debug" or "release".
    exit /b 1
)

if %ERRORLEVEL% equ 0 (
    echo.
    echo [Juicity-RS] Build successful!
    echo.
    echo Binaries:
    echo   juicity-server.exe: target\%BUILD_MODE%\juicity-server.exe
    echo   juicity-client.exe: target\%BUILD_MODE%\juicity-client.exe
) else (
    echo.
    echo [ERROR] Build failed!
    exit /b 1
)
