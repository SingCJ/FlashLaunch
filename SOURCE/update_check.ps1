$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
try {
    $release = Invoke-RestMethod -Uri 'https://api.github.com/repos/SingCJ/FlashLaunch/releases/latest' -Headers @{ 'User-Agent'='FlashLaunch-Updater'; Accept='application/vnd.github+json' } -TimeoutSec 30
    if ($release.draft -or $release.prerelease -or $release.tag_name -notmatch '^v?\d+\.\d+\.\d+$') { throw 'Invalid stable release.' }
    $version = $release.tag_name.TrimStart('v')
    $name = 'FlashLaunch-' + $version + '-windows-' + $env:FLASH_ARCH + '.zip'
    $asset = @($release.assets | Where-Object { $_.name -ceq $name -and $_.state -eq 'uploaded' })
    $url = ''
    $digest = '-'
    if ($asset.Count -eq 1) {
        $url = $asset[0].browser_download_url
        if ($asset[0].digest) { $digest = $asset[0].digest }
    }
    [Console]::Write($version + "`t" + $url + "`t" + $digest)
} catch {
    [Console]::Error.Write($_.Exception.Message)
    exit 1
}
