@echo off
setlocal
cd /d "%~dp0"
set "EXE=%~dp0lanlink.exe"
if not exist "%EXE%" goto missing
start "" "%EXE%" --agent
exit /b 0

:missing
echo.
echo [LanLink] lanlink.exe was not found in:
echo   %~dp0
echo.
echo 1. Extract ALL files from the zip to a normal folder first.
echo    Do NOT double-click this .bat inside the zip preview.
echo 2. If lanlink.exe disappeared after extraction, antivirus may have
echo    quarantined it. Open Windows Security - Protection history,
echo    restore it, and add this folder to exclusions.
echo 3. Read README.md for details.
echo.
pause
exit /b 1
