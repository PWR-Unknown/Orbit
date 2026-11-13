@echo off
setlocal
REM Builds orbit.exe and the no-admin installer. Needs Rust (https://rustup.rs) and, for the installer, Inno Setup 6.
cd /d "%~dp0"

where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust was not found. Install it from https://rustup.rs, then open a NEW terminal and run this again.
  pause
  exit /b 1
)

REM The installer takes its version from Cargo.toml so it can never disagree with the exe.
set "VER="
for /f "tokens=2 delims==" %%v in ('findstr /b /c:"version = " Cargo.toml') do if not defined VER set "VER=%%v"
set "VER=%VER: =%"
set "VER=%VER:"=%"
if "%VER%"=="" set "VER=0.0.0"

cargo build --release
if errorlevel 1 (
  echo BUILD FAILED
  pause
  exit /b 1
)
if not exist target\release\orbit.exe (
  echo BUILD FAILED: target\release\orbit.exe was not produced
  pause
  exit /b 1
)
if not exist dist mkdir dist
copy /y target\release\orbit.exe dist\Orbit.exe >nul
copy /y target\release\orbit-updater.exe dist\orbit-updater.exe >nul
if exist updater.json copy /y updater.json dist\updater.json >nul

set "ISCC="
for %%p in ("%LOCALAPPDATA%\Programs\Inno Setup 6" "%ProgramFiles(x86)%\Inno Setup 6" "%ProgramFiles%\Inno Setup 6") do if not defined ISCC if exist "%%~p\ISCC.exe" set "ISCC=%%~p\ISCC.exe"
if not defined ISCC (
  where ISCC >nul 2>nul
  if not errorlevel 1 set "ISCC=ISCC"
)

if not defined ISCC goto :noinno
"%ISCC%" /DAppVersion=%VER% installer\orbit.iss
if errorlevel 1 (
  echo INSTALLER BUILD FAILED - see the Inno Setup messages above
  pause
  exit /b 1
)
echo.
echo Installer: dist\Orbit-Setup.exe   version %VER%
echo Portable:  dist\Orbit.exe
pause
exit /b 0

:noinno
echo.
echo Inno Setup 6 was not found, so the installer was skipped. Get it from https://jrsoftware.org/isdl.php
echo Portable exe: dist\Orbit.exe
pause
