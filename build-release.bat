@echo off
setlocal EnableExtensions
chcp 65001 >nul
set "ROOT=%~dp0"
set "CI=1"
set "RELEASE_NO_RESTART=%NO_RESTART%"
set "NO_RESTART=1"
set "TEMP_ROOT=%ROOT%AI_CLI_TEMP"
set "X64_EXE=%TEMP_ROOT%\release-x64\Flash Launch.exe"
set "X86_EXE=%TEMP_ROOT%\release-x86\Flash Launch.exe"
set "CLEAN_ROOT=%ROOT%"
set "RELEASE_ROOT=%~dp0."
if /I "%~1"=="--help" goto usage
if /I "%~1"=="/?" goto usage
if /I "%~1"=="--version" goto parse_version
if not "%~1"=="" (echo Invalid argument: %~1&goto fail)
goto build_release
:parse_version
if "%~2"=="" (echo Missing version after --version.&goto fail)
if not "%~3"=="" (echo Invalid argument: %~3&goto fail)
set "VERSION=%~2"
:build_release
if not exist "%TEMP_ROOT%\release-x64" md "%TEMP_ROOT%\release-x64"
if not exist "%TEMP_ROOT%\release-x86" md "%TEMP_ROOT%\release-x86"
if defined VERSION (call "%ROOT%build.bat" --version %VERSION%) else (call "%ROOT%build.bat")
if errorlevel 1 goto fail
if not exist "%TEMP_ROOT%\release-x64" md "%TEMP_ROOT%\release-x64"
copy /y "%ROOT%Flash Launch.exe" "%X64_EXE%" >nul
if errorlevel 1 goto fail
call :build_x86
if errorlevel 1 goto fail
py -3 -X utf8 "%ROOT%Script\package_release.py" --architecture x64 --executable "%X64_EXE%" --output-dir "%RELEASE_ROOT%"
if errorlevel 1 goto fail
py -3 -X utf8 "%ROOT%Script\package_release.py" --architecture x86 --executable "%X86_EXE%" --output-dir "%RELEASE_ROOT%"
if errorlevel 1 goto fail
move /Y "%X64_EXE%" "%ROOT%Flash Launch.exe" >nul
if errorlevel 1 goto fail
move /Y "%X86_EXE%" "%ROOT%Flash Launch x86.exe" >nul
if errorlevel 1 goto fail
call :clean_release_temps
if errorlevel 1 goto fail
if exist "%TEMP_ROOT%" (echo Could not remove temporary folder: %TEMP_ROOT%&goto fail)
if exist "%ROOT%target" (echo Could not remove build folder: %ROOT%target&goto fail)
echo Release executables and two ZIP archives were created in the project; temporary files were removed.
echo GitHub upload was not performed.
if /I not "%RELEASE_NO_RESTART%"=="1" start "" /D "%ROOT%" "%ROOT%Flash Launch.exe"
if not "%CI%"=="1" pause
exit /b 0
:fail
echo Release build failed; no fake package was created.
if not "%CI%"=="1" pause
exit /b 1
:usage
echo Usage: build-release.bat [--version X.Y.Z]
echo Build Windows x64 and x86, verify PE headers, and create two ZIP archives.
echo After success, executables and ZIP archives stay in the project folder.
exit /b 0

:clean_release_temps
powershell -NoProfile -ExecutionPolicy Bypass -Command "$root=[IO.Path]::GetFullPath($env:CLEAN_ROOT); foreach($relative in @('AI_CLI_TEMP','TEMP','target')) { $path=[IO.Path]::GetFullPath((Join-Path $root $relative)); if(-not $path.StartsWith($root,[StringComparison]::OrdinalIgnoreCase)){throw 'Temporary path escaped the project.'}; if(Test-Path -LiteralPath $path) { $item=Get-Item -LiteralPath $path -Force; if(($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw ('Refusing reparse point: '+$path)}; Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop } }"
if errorlevel 1 (echo Safe temporary cleanup failed.&exit /b 1)
exit /b 0

:build_x86
if not exist "%TEMP_ROOT%\release-x86" md "%TEMP_ROOT%\release-x86"
set "BUILD_TIMESTAMP="
for /f "usebackq delims=" %%T in (`powershell -NoProfile -Command "Get-Date -Format 'ddMMMyy h:mmtt'"`) do set "BUILD_TIMESTAMP=%%T"
if not defined BUILD_TIMESTAMP (echo Could not generate x86 build timestamp.&exit /b 1)
echo x86 build timestamp: %BUILD_TIMESTAMP%
where cargo.exe >nul 2>nul
if errorlevel 1 (echo Rust cargo.exe was not found in PATH. Install Rust and add cargo.exe to PATH.&exit /b 1)
where rustup.exe >nul 2>nul
if errorlevel 1 (echo Rustup rustup.exe was not found in PATH. Install Rustup and add rustup.exe to PATH.&exit /b 1)
rustup target list --installed 2>nul | findstr /I /C:"i686-pc-windows-msvc" >nul
if errorlevel 1 (echo Missing x86 Rust target. Run: rustup target add i686-pc-windows-msvc&exit /b 1)
pushd "%ROOT%"
cargo build --release --locked --target i686-pc-windows-msvc
if errorlevel 1 (popd&exit /b 1)
copy /y "target\i686-pc-windows-msvc\release\flashlaunch.exe" "%X86_EXE%" >nul
if errorlevel 1 (popd&exit /b 1)
py -3 -X utf8 "%ROOT%Script\embed_icon.py" "%X86_EXE%" "%ROOT%Assets\Flash Launch.ico"
if errorlevel 1 (popd&exit /b 1)
py -3 -c "import os,pathlib; timestamp=os.environ['BUILD_TIMESTAMP']; data=pathlib.Path(os.environ['X86_EXE']).read_bytes(); assert timestamp.encode() in data, 'x86 build timestamp missing from executable'; print('Verified x86 build timestamp:', timestamp)"
if errorlevel 1 (popd&exit /b 1)
popd
exit /b 0
