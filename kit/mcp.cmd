@echo off
rem Launches the MCP server from a COPY of the release binary.
rem
rem WHY: Windows keeps a running image file open, so a live MCP server holds
rem target\release\toasteddrums.exe and `cargo build --release` dies on it with
rem "Access is denied (os error 5)". Editing the tracker while an agent is connected to it
rem is the normal case here, so the server must not be the thing blocking the rebuild.
rem
rem Each launch gets its own copy, which also stops a stale copy from an older session
rem shadowing a fresh build. Copies from finished sessions are swept on the way in; ones
rem still running are locked and are simply skipped.
setlocal
set ROOT=%~dp0..
set SRC=%ROOT%\target\release\toasteddrums.exe
set DIR=%ROOT%\target\mcp

if not exist "%SRC%" (
  echo toasteddrums: %SRC% not found -- run "cargo build --release" first 1>&2
  exit /b 1
)
if not exist "%DIR%" mkdir "%DIR%" 2>nul
del /q "%DIR%\td-*.exe" >nul 2>&1

set RUN=%DIR%\td-%RANDOM%%RANDOM%.exe
copy /y "%SRC%" "%RUN%" >nul 2>&1
if not exist "%RUN%" set RUN=%SRC%

"%RUN%" %*
