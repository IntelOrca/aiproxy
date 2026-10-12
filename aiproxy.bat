@echo off
setlocal
cd /d "%~dp0"

set "CONFIG="
set "PORT="
set "BIND="
set "SANDBOX="
set "CONFIG_ARG="
set "PORT_ARG="
set "BIND_ARG="

rem Use the compiled binary when present, otherwise build on the fly.
set "BIN=target\release\aiproxy.exe"

:parse
if "%~1"=="" goto :run
if /i "%~1"=="-p"          ( set "PORT=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="--port"      ( set "PORT=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="-b"          ( set "BIND=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="--bind"      ( set "BIND=%~2"   & shift & shift & goto :parse )
if /i "%~1"=="--sandbox"   ( set "SANDBOX=1"  & shift & goto :parse )
if /i "%~1"=="--config"    ( set "CONFIG=%~2" & shift & shift & goto :parse )
if /i "%~1"=="-h"          ( goto :help )
if /i "%~1"=="--help"      ( goto :help )
if /i "%~1"=="--version"   ( goto :version )
echo Unknown argument: %~1
echo Run aiproxy.bat --help for usage.
exit /b 1

:run
if defined SANDBOX goto :sandbox

if defined CONFIG set "CONFIG_ARG=--config %CONFIG%"
if defined PORT   set "PORT_ARG=--port %PORT%"
if defined BIND   set "BIND_ARG=--bind %BIND%"
echo [aiproxy] router%CONFIG_ARG%%PORT_ARG%%BIND_ARG%
if exist "%BIN%" (
  "%BIN%" %CONFIG_ARG% %PORT_ARG% %BIND_ARG%
) else (
  cargo run --release --quiet -- %CONFIG_ARG% %PORT_ARG% %BIND_ARG%
)
goto :eof

:sandbox
if not defined PORT set "PORT=6060"
echo [aiproxy] sandbox cluster: router on %PORT%, fake backends on %PORT%+1,+2,+3
set "SANDBOX_ARGS=--sandbox"
if defined BIND set "SANDBOX_ARGS=--sandbox --bind %BIND%"
if exist "%BIN%" (
  set "PORT=%PORT%" && "%BIN%" %SANDBOX_ARGS%
) else (
  set "PORT=%PORT%" && cargo run --release --quiet -- %SANDBOX_ARGS%
)
goto :eof

:help
echo Usage: aiproxy.bat [options]
echo.
echo   -p, --port ^<port^>  Listen port.
echo                 Router: overrides the config port. Sandbox: default 6060.
echo   -b, --bind ^<addr^>  Listen address: HOST, HOST:PORT, or *:PORT for every
echo                 interface. Overrides the config bind/host. A port here must
echo                 not disagree with -p.
echo   --sandbox     Run the fake sandbox cluster (router + 3 fake backends).
echo   --config ^<path^>   Router config file (default:
echo                 ~/.config/aiproxy/aiproxy.json, then /etc/aiproxy/aiproxy.json).
echo   -h, --help    Show this help.
echo.
echo Examples:
echo   aiproxy.bat --sandbox -p 6060            Router on 6060 + fake backends on 6061..6063
echo   aiproxy.bat -b *:9090                    Router on every interface, port 9090
echo   aiproxy.bat -b localhost                 Router on localhost, config port
echo   aiproxy.bat                               Router with the default config
echo   aiproxy.bat --config config.alt.json      Router with another config
exit /b 0

:version
if exist "%BIN%" (
  "%BIN%" --version
) else (
  cargo run --release --quiet -- --version
)
exit /b 0
