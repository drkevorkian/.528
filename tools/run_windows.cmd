@echo off
setlocal
powershell -ExecutionPolicy Bypass -File "%~dp0run_windows.ps1" %*
