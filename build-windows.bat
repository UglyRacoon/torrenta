@echo off
rem ---------------------------------------------------------------------------
rem  Обёртка для build-windows.ps1: запускает PowerShell-скрипт сборки Torrenta.
rem  Параметры передаются дальше как есть, например:
rem      build-windows.bat
rem      build-windows.bat -SkipTests -NoInstall
rem      build-windows.bat -Dev
rem      build-windows.bat -Help
rem ---------------------------------------------------------------------------
chcp 65001 >nul
setlocal
cd /d "%~dp0"

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-windows.ps1" %*
set "RC=%ERRORLEVEL%"

if not "%RC%"=="0" (
    echo.
    echo  [x] Сборка завершилась с ошибкой, код %RC%.
    echo      Подробности - выше в выводе и в docs\BUILD.md
    echo.
    pause
)

endlocal & exit /b %RC%
