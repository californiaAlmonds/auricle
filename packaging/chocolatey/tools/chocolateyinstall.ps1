$ErrorActionPreference = 'Stop'

$packageName = 'auricle'
$version     = '0.0.0'
$url64       = "https://github.com/californiaAlmonds/auricle/releases/download/v$version/auricle-$version-setup.exe"
$checksum64  = '0000000000000000000000000000000000000000000000000000000000000000'

Install-ChocolateyPackage `
  -PackageName $packageName `
  -FileType 'exe' `
  -Url64bit $url64 `
  -Checksum64 $checksum64 `
  -ChecksumType64 'sha256' `
  -SilentArgs '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' `
  -ValidExitCodes @(0)
