@echo off
"%~dp0\target\release\otaripper.exe" %*
if %errorlevel% neq 0 pause
