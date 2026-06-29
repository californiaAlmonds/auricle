$ErrorActionPreference = 'Stop'

$packageName = 'auricle'
$version     = '0.0.0'
$url64       = "https://github.com/californiaAlmonds/auricle/releases/download/v$version/auricle-$version-installer.exe"
$checksum64  = '0000000000000000000000000000000000000000000000000000000000000000'

$toolsDir = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
$exePath  = Join-Path $toolsDir 'auricle.exe'

Get-ChocolateyWebFile `
  -PackageName $packageName `
  -FileFullPath $exePath `
  -Url64bit $url64 `
  -Checksum64 $checksum64 `
  -ChecksumType64 'sha256'

# Standalone, portable executable: expose it on PATH via a shim.
Install-BinFile -Name 'auricle' -Path $exePath
