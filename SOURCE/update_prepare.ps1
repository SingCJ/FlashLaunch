$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
try {
    $stage = [IO.Path]::GetFullPath($env:FLASH_STAGE)
    $archive = Join-Path $stage 'release.zip'
    $payload = Join-Path $stage 'payload'
    if (Test-Path -LiteralPath $payload) { throw 'An update is already staged. Remove its TEMP folder and retry.' }
    Invoke-WebRequest -UseBasicParsing -Uri $env:FLASH_URL -OutFile $archive -TimeoutSec 180
    if ($env:FLASH_DIGEST -ne '-') {
        if ($env:FLASH_DIGEST -notmatch '^sha256:[a-fA-F0-9]{64}$') { throw 'Unsupported release digest.' }
        $stream = [IO.File]::OpenRead($archive)
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { $hash = [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-','') }
        finally { $hasher.Dispose(); $stream.Dispose() }
        if ($hash -ine $env:FLASH_DIGEST.Substring(7)) { throw 'Release checksum mismatch.' }
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        $total = 0L
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\','/')
            if ($name -ne 'Flash Launch.exe' -and $name -notmatch '^(Assets|Languages)/[^:]+$') { throw 'Unexpected release content.' }
            if ($name -match '(^|/)\.\.?(/|$)' -or $name.StartsWith('/') -or -not $seen.Add($name)) { throw 'Unsafe archive path.' }
            $total += $entry.Length
            if ($total -gt 536870912) { throw 'Release archive is too large.' }
        }
    } finally { $zip.Dispose() }
    [IO.Compression.ZipFile]::ExtractToDirectory($archive, $payload)
    foreach ($required in @('Flash Launch.exe','Assets/Flash Launch.ico','Assets/fping.wav','Languages/vi.ini')) {
        if (-not (Test-Path -LiteralPath (Join-Path $payload $required) -PathType Leaf)) { throw "Incomplete archive: $required" }
    }
    $bytes = [IO.File]::ReadAllBytes((Join-Path $payload 'Flash Launch.exe'))
    if ($bytes.Length -lt 64 -or $bytes[0] -ne 77 -or $bytes[1] -ne 90) { throw 'Invalid executable.' }
    $offset = [BitConverter]::ToInt32($bytes,60)
    if ($offset -lt 64 -or $offset + 6 -gt $bytes.Length -or [BitConverter]::ToUInt32($bytes,$offset) -ne 17744) { throw 'Invalid executable header.' }
    $machine = [BitConverter]::ToUInt16($bytes,$offset+4)
    $expected = if ($env:FLASH_ARCH -eq 'x86') { 332 } else { 34404 }
    if ($machine -ne $expected) { throw 'Wrong executable architecture.' }
} catch {
    [Console]::Error.Write($_.Exception.Message)
    exit 1
}
