@echo off
setlocal EnableExtensions EnableDelayedExpansion

set "PROJECT_ROOT=%~dp0"
set "RUSTFLAGS=%RUSTFLAGS% -C target-feature=+crt-static"
set "APP_EXE=Flash Launch.exe"
set "APP_ICON=Assets\Flash Launch.ico"
set "APP_WAS_RUNNING=0"
set "APP_RESTART_EXE=%APP_EXE%"
set "BUMP_VERSION=1"
set "REQUESTED_VERSION="
set "BUILD_TIMESTAMP="

if not "%~3"=="" (
    echo Unknown extra argument: %~3
    call :usage
    goto fail
)
if "%~1"=="" goto args_done
if /I "%~1"=="/h" (
    call :usage
    exit /b 0
)
if /I "%~1"=="/help" (
    call :usage
    exit /b 0
)
if /I "%~1"=="/?" (
    call :usage
    exit /b 0
)
if /I "%~1"=="--help" (
    call :usage
    exit /b 0
)
if /I "%~1"=="--version" goto parse_version_arg
echo Unknown argument: %~1
call :usage
goto fail

:parse_version_arg
if "%~2"=="" (
    echo Missing version after --version.
    call :usage
    goto fail
)
echo(%~2| findstr /R "^[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$" >nul
if errorlevel 1 (
    echo Invalid version: %~2
    echo Expected format: x.y.z
    goto fail
)
set "REQUESTED_VERSION=%~2"
goto args_done

:args_done

call :configure_rust_toolchain
if errorlevel 1 goto fail
if /I "%CHECK_TOOLCHAIN_ONLY%"=="1" exit /b 0
if not exist "%PROJECT_ROOT%%APP_ICON%" goto missing_assets
if not exist "%PROJECT_ROOT%Assets\fping.wav" goto missing_assets

if "%BUMP_VERSION%"=="1" (
    call :bump_cargo_version "%REQUESTED_VERSION%"
    if errorlevel 1 goto fail
)

call :set_build_timestamp
if errorlevel 1 goto fail
echo Build timestamp: %BUILD_TIMESTAMP%

call :stop_app_if_running "%APP_EXE%"

if exist "%PROJECT_ROOT%AI_CLI_TEMP" rd /s /q "%PROJECT_ROOT%AI_CLI_TEMP"

pushd "%PROJECT_ROOT%"

py -3 -X utf8 "%PROJECT_ROOT%Script\check_languages.py"
if errorlevel 1 goto fail_popd

cargo test --locked
if errorlevel 1 goto fail_popd

cargo build --release --locked
if errorlevel 1 goto fail_popd

copy /y "%PROJECT_ROOT%target\release\flashlaunch.exe" "%PROJECT_ROOT%%APP_EXE%" >nul
if errorlevel 1 goto fail_popd

set "APP_RESTART_EXE=%APP_EXE%"

if exist "%PROJECT_ROOT%%APP_ICON%" (
    py -3 -X utf8 "%PROJECT_ROOT%Script\embed_icon.py" "%PROJECT_ROOT%%APP_EXE%" "%PROJECT_ROOT%%APP_ICON%"
    if errorlevel 1 goto fail_popd
)


popd

if exist "%PROJECT_ROOT%target" rd /s /q "%PROJECT_ROOT%target"
if exist "%PROJECT_ROOT%AI_CLI_TEMP" rd /s /q "%PROJECT_ROOT%AI_CLI_TEMP"

echo Built: %PROJECT_ROOT%%APP_EXE%
if /I not "%NO_RESTART%"=="1" call :restart_app
if not "%CI%"=="1" pause
exit /b 0

:fail_popd
popd
:fail
if exist "%PROJECT_ROOT%AI_CLI_TEMP" rd /s /q "%PROJECT_ROOT%AI_CLI_TEMP"
echo Build failed.
if /I not "%NO_RESTART%"=="1" call :restart_app_if_needed
if not "%CI%"=="1" pause
exit /b 1

:usage
echo Usage: build.bat [--version x.y.z]
echo.
echo   build.bat              Increment Cargo.toml patch version, update Cargo.lock, then build.
echo   build.bat --version X  Set Cargo.toml/Cargo.lock to X, then build.
exit /b 0

:bump_cargo_version
set "REQUESTED_VERSION=%~1"
set "CURRENT_VERSION="
for /f "tokens=3 delims= " %%V in ('findstr /B /C:"version = " "%PROJECT_ROOT%Cargo.toml"') do (
    set "CURRENT_VERSION=%%~V"
    goto got_cargo_version
)
:got_cargo_version
if not defined CURRENT_VERSION (
    echo Could not read package version from Cargo.toml.
    exit /b 1
)
echo(%CURRENT_VERSION%| findstr /R "^[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$" >nul
if errorlevel 1 (
    echo Invalid current version: %CURRENT_VERSION%
    exit /b 1
)
if defined REQUESTED_VERSION (
    set "NEW_VERSION=%REQUESTED_VERSION%"
) else (
    set "VERSION_MAJOR="
    set "VERSION_MINOR="
    set "VERSION_PATCH="
    set "VERSION_EXTRA="
    for /f "tokens=1-4 delims=." %%A in ("%CURRENT_VERSION%") do (
        set "VERSION_MAJOR=%%A"
        set "VERSION_MINOR=%%B"
        set "VERSION_PATCH=%%C"
        set "VERSION_EXTRA=%%D"
    )
    if defined VERSION_EXTRA (
        echo Invalid current version: %CURRENT_VERSION%
        exit /b 1
    )
    set /a VERSION_PATCH=!VERSION_PATCH!+1
    set "NEW_VERSION=!VERSION_MAJOR!.!VERSION_MINOR!.!VERSION_PATCH!"
)
echo(%NEW_VERSION%| findstr /R "^[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$" >nul
if errorlevel 1 (
    echo Invalid new version: %NEW_VERSION%
    exit /b 1
)
if "%CURRENT_VERSION%"=="%NEW_VERSION%" (
    echo Version unchanged: %CURRENT_VERSION%
    exit /b 0
)
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $enc=[System.Text.UTF8Encoding]::new($false); $q=[char]34; $old=$env:CURRENT_VERSION; $new=$env:NEW_VERSION; $root=$env:PROJECT_ROOT; $toml=Join-Path $root 'Cargo.toml'; $text=[IO.File]::ReadAllText($toml); $oldLine='version = '+$q+$old+$q; $newLine='version = '+$q+$new+$q; if(-not $text.Contains($oldLine)){throw 'Cargo.toml version line not found.'}; $text=$text.Replace($oldLine,$newLine); [IO.File]::WriteAllText($toml,$text,$enc); $lock=Join-Path $root 'Cargo.lock'; if(Test-Path -LiteralPath $lock){$lockText=[IO.File]::ReadAllText($lock); $oldLock='name = '+$q+'flashlaunch'+$q+[Environment]::NewLine+'version = '+$q+$old+$q; $newLock='name = '+$q+'flashlaunch'+$q+[Environment]::NewLine+'version = '+$q+$new+$q; if($lockText.Contains($oldLock)){$lockText=$lockText.Replace($oldLock,$newLock)}else{$oldLock='name = '+$q+'flashlaunch'+$q+[char]10+'version = '+$q+$old+$q; $newLock='name = '+$q+'flashlaunch'+$q+[char]10+'version = '+$q+$new+$q; if(-not $lockText.Contains($oldLock)){throw 'Cargo.lock flashlaunch version not found.'}; $lockText=$lockText.Replace($oldLock,$newLock)}; [IO.File]::WriteAllText($lock,$lockText,$enc)}"
if errorlevel 1 exit /b 1
echo Version: %CURRENT_VERSION% -^> %NEW_VERSION%
exit /b 0

:set_build_timestamp
for /f "usebackq tokens=*" %%T in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "Get-Date -Format 'ddMMMyy h:mmtt'"`) do set "BUILD_TIMESTAMP=%%T"
if not defined BUILD_TIMESTAMP (
    echo Could not generate build timestamp.
    exit /b 1
)
exit /b 0

:missing_assets
echo Required Assets files are missing. Restore Assets\Flash Launch.ico and Assets\fping.wav.
goto fail

:configure_rust_toolchain
where cargo.exe >nul 2>nul
if errorlevel 1 goto rust_missing
cargo --version
where rustup.exe >nul 2>nul
if errorlevel 1 goto rustup_missing
rustup --version
exit /b 0
:rust_missing
echo Rust was not found in PATH. Install Rust and ensure cargo.exe is available.
exit /b 1
:rustup_missing
echo Rustup was not found in PATH. Install Rustup and ensure rustup.exe is available.
exit /b 1

:restart_app_if_needed
if "%APP_WAS_RUNNING%"=="1" if exist "%PROJECT_ROOT%%APP_RESTART_EXE%" (
    echo Restarting %APP_RESTART_EXE%...
    start "" /D "%PROJECT_ROOT%" "%PROJECT_ROOT%%APP_RESTART_EXE%"
)
exit /b 0

:restart_app
if exist "%PROJECT_ROOT%%APP_EXE%" (
    echo Starting %APP_EXE%...
    start "" /D "%PROJECT_ROOT%" "%PROJECT_ROOT%%APP_EXE%"
)
exit /b 0

:stop_app_if_running
tasklist /FI "IMAGENAME eq %~1" 2>nul | find /I "%~1" >nul
if not errorlevel 1 (
    if "%APP_WAS_RUNNING%"=="0" set "APP_RESTART_EXE=%~1"
    set "APP_WAS_RUNNING=1"
    echo Stopping running %~1...
    taskkill /IM "%~1" /F >nul 2>nul
    call :wait_app_exit "%~1"
)
exit /b 0

:wait_app_exit
for /L %%A in (1,1,30) do (
    tasklist /FI "IMAGENAME eq %~1" 2>nul | find /I "%~1" >nul
    if errorlevel 1 exit /b 0
    timeout /t 1 /nobreak >nul
)
exit /b 0
