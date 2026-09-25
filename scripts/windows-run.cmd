@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0windows-run.ps1" %*
exit /b %ERRORLEVEL%
