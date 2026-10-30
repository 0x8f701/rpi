# Stage the host Electron runtime with this desktop shell and rpi.exe.
# No packaging framework is added. The zip root is the rpi-desktop directory.
# This script does not sign the Windows executable.
#
# Usage: $env:VERSION = '1.0.0'; $env:RPI_BINARY = 'C:\path\rpi.exe'; ./package-win.ps1 out.zip
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$version = $env:VERSION
if (-not $version) { throw 'VERSION is required' }
$binary = $env:RPI_BINARY
if (-not $binary) { throw 'RPI_BINARY is required' }
if ($args.Count -lt 1 -or -not $args[0]) { throw 'output zip path is required' }
$out = $args[0]
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "missing release rpi: $binary" }

$runtime = Join-Path $here 'node_modules\electron\dist'
$electron = Join-Path $runtime 'electron.exe'
if (-not (Test-Path -LiteralPath $electron -PathType Leaf)) { throw "missing Electron runtime: $electron" }
$license = Join-Path $here '..\..\..\LICENSE'
if (-not (Test-Path -LiteralPath $license -PathType Leaf)) { throw 'LICENSE is required in desktop archives' }

Push-Location $here
try {
  if (Test-Path -LiteralPath 'dist') { Remove-Item -LiteralPath 'dist' -Recurse -Force }
  & npm run build
  if ($LASTEXITCODE -ne 0) { throw "npm run build exited $LASTEXITCODE" }
} finally {
  Pop-Location
}

$stageParent = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$stage = Join-Path $stageParent ("rpi-desktop-stage-" + [guid]::NewGuid().ToString('n'))
$dest = Join-Path $stage 'rpi-desktop'
New-Item -ItemType Directory -Path $dest | Out-Null
try {
  Copy-Item -Path (Join-Path $runtime '*') -Destination $dest -Recurse -Force
  $defaultAsar = Join-Path $dest 'resources\default_app.asar'
  if (Test-Path -LiteralPath $defaultAsar) { Remove-Item -LiteralPath $defaultAsar -Recurse -Force }
  $app = Join-Path $dest 'resources\app'
  New-Item -ItemType Directory -Path (Join-Path $app 'dist') -Force | Out-Null
  Copy-Item -Path (Join-Path $here 'dist\*') -Destination (Join-Path $app 'dist') -Recurse -Force
  $pkgPath = Join-Path $here 'package.json'
  $pkg = Get-Content -LiteralPath $pkgPath -Raw | ConvertFrom-Json
  $pkg.version = $version
  $manifest = $pkg | ConvertTo-Json -Depth 20
  Set-Content -LiteralPath (Join-Path $app 'package.json') -Value $manifest -Encoding utf8
  Copy-Item -LiteralPath (Join-Path $here 'README.md') -Destination (Join-Path $app 'README.md')
  Copy-Item -LiteralPath $binary -Destination (Join-Path $dest 'resources\rpi.exe')
  Copy-Item -LiteralPath $license -Destination (Join-Path $dest 'LICENSE')
  $notice = Join-Path $here '..\..\..\NOTICE'
  if (Test-Path -LiteralPath $notice -PathType Leaf) {
    Copy-Item -LiteralPath $notice -Destination (Join-Path $dest 'NOTICE')
  }
  $outParent = Split-Path -Parent $out
  if ($outParent -and -not (Test-Path -LiteralPath $outParent)) {
    New-Item -ItemType Directory -Path $outParent | Out-Null
  }
  if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Force }
  Compress-Archive -Path $dest -DestinationPath $out
} finally {
  if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
Write-Output $out
