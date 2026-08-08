@echo off
setlocal
cd /d "%~dp0"

set "PORT="
set "CONFIG="
set "SANDBOX="
set "CONFIG_ARG="
set "PORT_ARG="

:parse
if "%~1"=="" goto :run
if /i "%~1"=="-p"          ( set "PORT=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="--port"      ( set "PORT=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="--sandbox"   ( set "SANDBOX=1"  & shift & goto :parse )
if /i "%~1"=="--config"    ( set "CONFIG=%~2" & shift & shift & goto :parse )
if /i "%~1"=="-h"          ( goto :help )
if /i "%~1"=="--help"      ( goto :help )
echo Unknown argument: %~1
echo Run aiproxy.bat --help for usage.
exit /b 1

:run
if defined SANDBOX goto :sandbox

if defined CONFIG set "CONFIG_ARG=--config %CONFIG%"
if defined PORT   set "PORT_ARG=--port %PORT%"
echo [aiproxy] router%CONFIG_ARG%%PORT_ARG%
deno run --allow-net --allow-read --allow-env main.ts %CONFIG_ARG% %PORT_ARG%
goto :eof

:sandbox
if not defined PORT set "PORT=6060"
echo [aiproxy] sandbox cluster: router on %PORT%, fake backends on %PORT%+1,+2,+3
deno run --allow-net --allow-write --allow-env src\sandbox.ts
goto :eof

:help
echo Usage: aiproxy.bat [options]
echo.
echo   -p, --port ^<port^>  Listen port.
echo                 Router: overrides config "port". Sandbox: default 6060.
echo   --sandbox     Run the fake sandbox cluster (router + 3 fake backends).
echo   --config ^<path^>   Router config file (default: config.json).
echo   -h, --help    Show this help.
echo.
echo Examples:
echo   aiproxy.bat --sandbox -p 6060            Router on 6060 + fake backends on 6061..6063
echo   aiproxy.bat                               Router with config.json
echo   aiproxy.bat --config config.alt.json      Router with another config
exit /b 0
