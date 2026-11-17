# rpi installer (Windows).
#
# Windows builds are not published. This entry point fails closed and does not
# download, verify, or install anything, and it does not modify the system.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Help

[CmdletBinding()]
param(
    [switch]$Help
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($Help) {
    Write-Host "rpi Windows installer"
    Write-Host ""
    Write-Host "Windows builds are not published. This script does not install rpi"
    Write-Host "and does not change PATH, files, or the registry."
    Write-Host ""
    Write-Host "Usage:"
    Write-Host "  powershell -ExecutionPolicy Bypass -File install.ps1 [-Help]"
    exit 1
}

Write-Error "install.ps1: error: Windows builds are not published; rpi is not installed and the system is unchanged"
exit 1