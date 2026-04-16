@echo off
rem tn.cmd — Windows CMD wrapper around the `therminal` CLI.
rem
rem Windows equivalent of scripts/tn (bash). See that file for the
rem CLI-vs-MCP usage policy and full subcommand reference.
rem
rem Binary discovery order:
rem   1. THERMINAL_BIN env var (explicit override)
rem   2. therminal on PATH
rem   3. %USERPROFILE%\Desktop\therminal.exe (common dev build location)
rem
rem Installation: copy this file to a directory on %%PATH%%.
rem The .exe copy is preferred — it works in both CMD and
rem PowerShell without execution-policy concerns.

if defined THERMINAL_BIN (
    "%THERMINAL_BIN%" %*
    exit /b %ERRORLEVEL%
)

where therminal >nul 2>nul
if %ERRORLEVEL% equ 0 (
    therminal %*
    exit /b %ERRORLEVEL%
)

if exist "%USERPROFILE%\Desktop\therminal.exe" (
    "%USERPROFILE%\Desktop\therminal.exe" %*
    exit /b %ERRORLEVEL%
)

echo tn: therminal binary not found >&2
echo   Checked: THERMINAL_BIN env, PATH, %%USERPROFILE%%\Desktop >&2
echo   Fix: set THERMINAL_BIN or add therminal.exe to PATH >&2
exit /b 1
