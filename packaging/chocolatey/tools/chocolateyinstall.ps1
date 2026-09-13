$ErrorActionPreference = 'Stop'

$packageName = 'auricle'
$version     = '__VERSION__'
$url64       = "https://github.com/californiaAlmonds/auricle/releases/download/v$version/auricle-$version-setup.exe"
$checksum64  = '__CHECKSUM__'

Install-ChocolateyPackage `
  -PackageName $packageName `
  -FileType 'exe' `
  -Url64bit $url64 `
  -Checksum64 $checksum64 `
  -ChecksumType64 'sha256' `
  -SilentArgs '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' `
  -ValidExitCodes @(0)
