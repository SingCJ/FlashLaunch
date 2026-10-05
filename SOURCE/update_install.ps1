$ErrorActionPreference = 'Stop'
$stage = [IO.Path]::GetFullPath($env:FLASH_STAGE)
$root = [IO.Path]::GetFullPath($env:FLASH_ROOT)
$exe = [IO.Path]::GetFullPath($env:FLASH_EXE)
$backup = Join-Path $root ('BACKUP\update-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $stage 'payload'
$changed = [Collections.Generic.List[object]]::new()
try {
    if ([IO.Path]::GetDirectoryName($exe) -ine $root.TrimEnd('\')) { throw 'Executable is outside the application folder.' }
    if (-not $stage.StartsWith((Join-Path $root 'TEMP') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Staging folder is outside TEMP.' }
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    while (Get-Process -Id ([int]$env:FLASH_PID) -ErrorAction SilentlyContinue) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'Application did not close.' }
        Start-Sleep -Milliseconds 200
    }
    New-Item -ItemType Directory -Path $backup -Force | Out-Null
    $files = @(Get-ChildItem -LiteralPath $payload -File -Recurse)
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($payload.Length + 1)
        $target = if ($relative -eq 'Flash Launch.exe') { $exe } else { Join-Path $root $relative }
        # Reject junctions and other redirected destination folders before writing anything.
        $parent = [IO.DirectoryInfo]::new([IO.Path]::GetDirectoryName($target))
        while ($parent -and $parent.FullName.Length -ge $root.Length) {
            if ($parent.Exists -and ($parent.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Redirected destination folder.' }
            $parent = $parent.Parent
        }
        if (Test-Path -LiteralPath $target) {
            if ((Get-Item -LiteralPath $target).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Redirected destination file.' }
            $saved = Join-Path $backup $relative
            New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($saved)) -Force | Out-Null
            Copy-Item -LiteralPath $target -Destination $saved
        }
    }
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($payload.Length + 1)
        $target = if ($relative -eq 'Flash Launch.exe') { $exe } else { Join-Path $root $relative }
        $saved = Join-Path $backup $relative
        $changed.Add(@{Target=$target; Saved=$saved; Existed=(Test-Path -LiteralPath $target)})
        New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($target)) -Force | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
    }
    Start-Process -FilePath $exe -WorkingDirectory $root -WindowStyle Hidden
} catch {
    $failure = $_.Exception.Message
    for ($index = $changed.Count - 1; $index -ge 0; $index--) {
        $item = $changed[$index]
        try {
            if ($item.Existed) { Copy-Item -LiteralPath $item.Saved -Destination $item.Target -Force }
            elseif (Test-Path -LiteralPath $item.Target) { Remove-Item -LiteralPath $item.Target }
        } catch { $failure += "`r`n" + $_.Exception.Message }
    }
    [IO.File]::WriteAllText((Join-Path $stage 'install-error.log'), $failure)
    try { Start-Process -FilePath $exe -WorkingDirectory $root -WindowStyle Hidden } catch {}
    Add-Type -AssemblyName PresentationFramework
    [System.Windows.MessageBox]::Show($env:FLASH_FAILURE + "`r`n" + $failure, 'Flash Launch') | Out-Null
    exit 1
}
