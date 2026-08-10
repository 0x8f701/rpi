# rpi installer (Windows x86_64).
#
# Downloads the x86_64-pc-windows-msvc artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as %USERPROFILE%\.rpi\bin\rpi.exe.
#
# Usage:
#   irm https://raw.githubusercontent.com/0x8f701/rpi/master/install.ps1 | iex
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Version v0.2.8
#
# Environment:
#   PI_HOME                install root (default: %USERPROFILE%\.rpi)
#   PI_UPDATE_BASE_URL     GitHub-Releases-shaped API base (default:
#                          https://api.github.com/repos/0x8f701/rpi/releases)
#   GITHUB_TOKEN           authenticate the fixed GitHub API endpoint (default:
#                          none; never sent to release-asset hosts)

[CmdletBinding()]
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Fail([string]$Message) {
    Write-Error "install.ps1: error: $Message"
    exit 1
}

function Get-CandidateIdentityFailure([string]$Path, [string]$ExpectedVersion) {
    $CandidateError = $null
    $CandidateExit = $null
    $CandidateOutput = @()
    try {
        $CandidateOutput = @(& $Path --version 2>$null)
        $CandidateExit = $LASTEXITCODE
    } catch {
        $CandidateError = $_.Exception.Message
    }
    if ($CandidateError) {
        return "failed smoke test ($CandidateError)"
    }
    if ($CandidateExit -ne 0) {
        return "failed smoke test (exit $CandidateExit)"
    }
    if ($CandidateOutput.Count -ne 1 -or [string]$CandidateOutput[0] -cne "rpi $ExpectedVersion") {
        return "reported unexpected identity/version (expected 'rpi $ExpectedVersion')"
    }
    return $null
}

function Ensure-SafeDirectory([string]$Path, [string]$Label) {
    if (Test-Path -LiteralPath $Path) {
        $Item = Get-Item -LiteralPath $Path -Force
        if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to use reparse-point ${Label}: $Path"
        }
        if (-not $Item.PSIsContainer) {
            Fail "$Label is not a directory: $Path"
        }
    } else {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Add-RollbackError(
    [System.Collections.Generic.List[string]]$Errors,
    [string]$Message
) {
    [void]$Errors.Add($Message)
}

function Remove-RollbackPath(
    [string]$Path,
    [string]$Label,
    [System.Collections.Generic.List[string]]$Errors
) {
    try {
        if (Test-Path -LiteralPath $Path -ErrorAction Stop) {
            Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
        }
    } catch {
        Add-RollbackError $Errors "could not remove $Label at ${Path}: $($_.Exception.Message)"
    }
}

function Confirm-RollbackPath(
    [string]$Path,
    [string]$Label,
    [bool]$ShouldExist,
    [System.Collections.Generic.List[string]]$Errors
) {
    try {
        if ($ShouldExist) {
            if (-not (Test-Path -LiteralPath $Path -PathType Leaf -ErrorAction Stop)) {
                Add-RollbackError $Errors "could not verify restored ${Label}: expected a file at $Path"
            }
        } elseif (Test-Path -LiteralPath $Path -ErrorAction Stop) {
            Add-RollbackError $Errors "could not verify removal of ${Label}: path still exists at $Path"
        }
    } catch {
        Add-RollbackError $Errors "could not verify ${Label} at ${Path}: $($_.Exception.Message)"
    }
}

function Restore-RollbackAside(
    [string]$Path,
    [string]$Aside,
    [bool]$HadPrior,
    [string]$Label,
    [System.Collections.Generic.List[string]]$Errors
) {
    Remove-RollbackPath $Path $Label $Errors
    if ($HadPrior) {
        try {
            Move-Item -LiteralPath $Aside -Destination $Path -Force -ErrorAction Stop
        } catch {
            Add-RollbackError $Errors "could not restore ${Label} from ${Aside}: $($_.Exception.Message)"
        }
        Confirm-RollbackPath $Path $Label $true $Errors
        Confirm-RollbackPath $Aside "$Label aside" $false $Errors
    } else {
        Confirm-RollbackPath $Path $Label $false $Errors
    }
}

function Fail-WithRollbackErrors(
    [string]$OriginalError,
    [System.Collections.Generic.List[string]]$Errors
) {
    if ($Errors.Count -eq 0) {
        Fail "$OriginalError; rollback completed and verified"
    }
    Fail "$OriginalError; rollback incomplete: $($Errors -join ' | ')"
}

# Atomic same-volume file replacement via the Win32 MoveFileEx API. PowerShell's
# Move-Item cannot overwrite an existing file, so the reference installer renamed
# the active executable aside before moving the new one in — leaving a window in
# which rpi.exe is absent. MoveFileEx with MOVEFILE_REPLACE_EXISTING renames the
# staged file over the destination in a single syscall, so rpi.exe is never
# missing (when it is not locked by a running process).
if (-not ('PiInstall.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace PiInstall {
    public static class Native {
        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        public static extern bool MoveFileEx(string lpExistingFileName, string lpNewFileName, int dwFlags);
    }
}
'@
}
$MOVEFILE_REPLACE_EXISTING = 1
# ERROR_SHARING_VIOLATION / ERROR_ACCESS_DENIED: a running rpi.exe holds Dest.
$ERR_SHARING_VIOLATION = 32
$ERR_ACCESS_DENIED = 5

$Repo = "0x8f701/rpi"
$ApiBase = if ($env:PI_UPDATE_BASE_URL) { $env:PI_UPDATE_BASE_URL } else { "https://api.github.com/repos/$Repo/releases" }
$PiHome = if ($env:PI_HOME) { $env:PI_HOME } else { Join-Path $env:USERPROFILE ".rpi" }
$Triple = "x86_64-pc-windows-msvc"

# ── Platform gate ────────────────────────────────────────────────────────────
if (-not [System.Environment]::Is64BitOperatingSystem) {
    Fail "rpi requires 64-bit Windows (x86_64)"
}
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne "AMD64") {
    Fail "unsupported architecture '$arch' (only x86_64/AMD64 Windows builds are published)"
}

# ── Version argument ─────────────────────────────────────────────────────────
$Version = $Version.Trim()
if ($Version.StartsWith('v', [System.StringComparison]::Ordinal)) {
    $Version = $Version.Substring(1)
}
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    Fail "invalid version '$Version' (expected X.Y.Z or vX.Y.Z)"
}

# TLS 1.2 for older PowerShell 5.1 defaults.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Headers = @{ "User-Agent" = "rpi-install"; "Accept" = "application/vnd.github+json" }

# Optional GITHUB_TOKEN support, mirroring install.sh and the built-in updater:
# the token authenticates only the fixed GitHub API endpoint (avoiding the
# unauthenticated rate limit) and is never sent to release-asset hosts or a
# custom PI_UPDATE_BASE_URL endpoint.
$GitHubApiBase = "https://api.github.com/repos/$Repo/releases"
$ApiHeaders = $Headers
if ($env:GITHUB_TOKEN -and $ApiBase -eq $GitHubApiBase) {
    $ApiHeaders = @{} + $Headers
    $ApiHeaders["Authorization"] = "Bearer $env:GITHUB_TOKEN"
}

# ── Resolve the release ──────────────────────────────────────────────────────
$ReleaseUrl = if ($Version) { "$ApiBase/tags/v$Version" } else { "$ApiBase/latest" }
Write-Host "Resolving release from $ReleaseUrl"
try {
    $Release = Invoke-RestMethod -Uri $ReleaseUrl -Headers $ApiHeaders
} catch {
    Fail "could not fetch release metadata from ${ReleaseUrl}: $($_.Exception.Message) (GitHub may be rate-limiting this IP; set GITHUB_TOKEN to authenticate)"
}

$Tag = [string]$Release.tag_name
if (-not $Tag) { Fail "release metadata has no tag_name (endpoint: $ReleaseUrl)" }
if ($Tag -notmatch '^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    Fail "release tag '$Tag' is invalid (expected semantic version vX.Y.Z)"
}
$ResolvedVersion = $Tag.Substring(1)
if ($Version -and $ResolvedVersion -ne $Version) {
    Fail "requested version $Version but release tag is $Tag"
}

$Asset = "rpi-$ResolvedVersion-$Triple.zip"
if ($null -eq $Release.assets) { Fail "release $Tag has no assets" }
$ArchiveMatches = @($Release.assets | Where-Object { $_.name -eq $Asset })
$SumsMatches = @($Release.assets | Where-Object { $_.name -eq "SHA256SUMS" })
if ($ArchiveMatches.Count -ne 1) { Fail "release $Tag must contain exactly one asset named $Asset" }
if ($SumsMatches.Count -ne 1) { Fail "release $Tag must contain exactly one SHA256SUMS asset" }
$ArchiveAsset = $ArchiveMatches[0]
$SumsAsset = $SumsMatches[0]

# ── Download + verify ────────────────────────────────────────────────────────
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("rpi-install-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$StateTmp = $null
$InstallMutex = $null
$InstallMutexAcquired = $false
try {
    $ArchivePath = Join-Path $TmpDir $Asset
    $SumsPath = Join-Path $TmpDir "SHA256SUMS"

    Write-Host "Downloading rpi v$ResolvedVersion ($Triple)..."
    Invoke-WebRequest -Uri $ArchiveAsset.browser_download_url -Headers $Headers -OutFile $ArchivePath -UseBasicParsing
    Invoke-WebRequest -Uri $SumsAsset.browser_download_url -Headers $Headers -OutFile $SumsPath -UseBasicParsing

    if ((Get-Item -LiteralPath $SumsPath).Length -gt 1MB) {
        Fail "SHA256SUMS is unexpectedly large"
    }
    if ((Get-Item -LiteralPath $ArchivePath).Length -gt 1GB) {
        Fail "$Asset exceeds the 1 GiB safety limit"
    }

    $ExpectedMatches = @()
    foreach ($line in Get-Content -LiteralPath $SumsPath) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Asset) {
            $ExpectedMatches += [string]$parts[0]
        }
    }
    if ($ExpectedMatches.Count -ne 1) {
        Fail "SHA256SUMS must contain exactly one entry for $Asset"
    }
    $Expected = $ExpectedMatches[0].ToLowerInvariant()
    if ($Expected -notmatch '^[0-9a-f]{64}$') {
        Fail "SHA256SUMS contains an invalid digest for $Asset"
    }

    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Fail "SHA256 mismatch for ${Asset}: expected $Expected, got $Actual"
    }
    Write-Host "Checksum verified."

    # Materialize the root executable only; no bundled runtime is shipped.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $BinaryPath = Join-Path $TmpDir "rpi.exe"
    $Zip = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        if (@($Zip.Entries).Count -gt 4096) {
            Fail "archive $Asset contains too many entries"
        }
        $BinaryEntries = @($Zip.Entries | Where-Object {
            $_.FullName -eq "rpi.exe" -or $_.FullName -eq "./rpi.exe"
        })
        if ($BinaryEntries.Count -ne 1) {
            Fail "archive $Asset must contain exactly one root-level rpi.exe"
        }
        $BinaryEntry = $BinaryEntries[0]
        if ($BinaryEntry.Length -le 0 -or $BinaryEntry.Length -gt 1GB) {
            Fail "archive $Asset contains an invalid-size rpi.exe"
        }
        $UnixFileType = (($BinaryEntry.ExternalAttributes -shr 16) -band 0xF000)
        if ($UnixFileType -ne 0 -and $UnixFileType -ne 0x8000) {
            Fail "archive $Asset contains a non-regular rpi.exe entry"
        }
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile(
            $BinaryEntry, $BinaryPath, $true
        )
    } finally {
        $Zip.Dispose()
    }
    $Binary = Get-Item -LiteralPath $BinaryPath

    Ensure-SafeDirectory $PiHome "rpi install root"
    $BinDir = Join-Path $PiHome "bin"
    Ensure-SafeDirectory $BinDir "rpi bin directory"
    $Dest = Join-Path $BinDir "rpi.exe"
    $StatePath = Join-Path $PiHome "update-state.json"
    if (Test-Path -LiteralPath $Dest) {
        $DestItem = Get-Item -LiteralPath $Dest -Force
        if (($DestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point rpi executable: $Dest"
        }
        if ($DestItem.PSIsContainer) {
            Fail "refusing to replace directory at rpi executable path: $Dest"
        }
    }
    if (Test-Path -LiteralPath $StatePath) {
        $StateItem = Get-Item -LiteralPath $StatePath -Force
        if (($StateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Fail "refusing to replace reparse-point update state: $StatePath"
        }
        if ($StateItem.PSIsContainer) {
            Fail "update-state path is a directory: $StatePath"
        }
    }

    # Verify the downloaded candidate's exact identity before replacing the
    # active executable or persisting updater state.
    $PreSmokeFailure = Get-CandidateIdentityFailure $Binary.FullName $ResolvedVersion
    if ($PreSmokeFailure) {
        Fail "downloaded binary $PreSmokeFailure; existing install left untouched"
    }

    # Prepare parseable updater state before touching the active executable.
    # Windows PowerShell 5.1's `-Encoding UTF8` writes a BOM, which serde_json
    # rejects, so write explicit UTF-8 without BOM.
    $StateTmp = "$StatePath.install.$PID.$([Guid]::NewGuid().ToString('N'))"
    $UnixEpoch = [DateTime]::new(1970, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
    $CheckedAtUnix = [long][Math]::Floor(([DateTime]::UtcNow - $UnixEpoch).TotalSeconds)
    $State = [ordered]@{
        installed_version = $ResolvedVersion
        installed_asset = $Asset
        installed_sha256 = $Expected
        installed_binary = "rpi.exe"
        checked_at_unix = $CheckedAtUnix
    }
    $Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    $StateJson = ($State | ConvertTo-Json) + [Environment]::NewLine
    [IO.File]::WriteAllText($StateTmp, $StateJson, $Utf8NoBom)

    # Serialize concurrent installs over PI_HOME. Downloads run concurrently,
    # but activation is guarded by a named mutex with the same 30-second bound
    # as the self-updater. An abandoned mutex transfers ownership safely.
    $MutexName = "Local\rpi-install-" + ($PiHome -replace '[\\/:]', '_')
    $InstallMutex = New-Object System.Threading.Mutex($false, $MutexName)
    try {
        $InstallMutexAcquired = $InstallMutex.WaitOne([TimeSpan]::FromSeconds(30))
    } catch [System.Threading.AbandonedMutexException] {
        $InstallMutexAcquired = $true
    }
    if (-not $InstallMutexAcquired) {
        Fail "timed out after 30s waiting for another rpi install ($MutexName); retry after it finishes"
    }

    # Detect a legacy installer-managed pi.exe while the install mutex is held.
    # The shared state must prove the old product/version/platform, the command
    # must report that exact version, and its bytes are fingerprinted so a raced
    # or user-replaced file is never removed. Cleanup happens only after rpi and
    # its replacement state have committed successfully.
    $LegacyPiPath = Join-Path $BinDir "pi.exe"
    $LegacyPiSha256 = $null
    if ((Test-Path -LiteralPath $StatePath -PathType Leaf) -and
        (Test-Path -LiteralPath $LegacyPiPath -PathType Leaf)) {
        try {
            $LegacyState = Get-Content -LiteralPath $StatePath -Raw | ConvertFrom-Json
            $LegacyVersion = [string]$LegacyState.installed_version
            $LegacyAsset = [string]$LegacyState.installed_asset
            $LegacyDigest = [string]$LegacyState.installed_sha256
            $LegacyBinary = [string]$LegacyState.installed_binary
            $LegacyItem = Get-Item -LiteralPath $LegacyPiPath -Force
            $LegacyVersionOutput = (& $LegacyPiPath --version 2>$null | Out-String).Trim()
            $LegacyVersionExit = $LASTEXITCODE
            if ($LegacyVersion -match '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$' -and
                $LegacyAsset -eq "pi-rs-$LegacyVersion-$Triple.zip" -and
                $LegacyDigest -match '^[0-9A-Fa-f]{64}$' -and
                $LegacyBinary -eq "pi.exe" -and
                ($LegacyItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0 -and
                -not $LegacyItem.PSIsContainer -and
                $LegacyVersionExit -eq 0 -and
                $LegacyVersionOutput -eq "pi $LegacyVersion") {
                $LegacyPiSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $LegacyPiPath).Hash.ToLowerInvariant()
            }
        } catch {
            $LegacyPiSha256 = $null
        }
    }

    # Stage the new binary next to Dest (same volume) so the final swap is an
    # atomic same-volume rename (MoveFileEx), not a cross-volume move. If a
    # running rpi.exe locks Dest, the atomic replace fails and we refuse outright
    # rather than fall back to a window-opening rename-aside.
    $Staged = Join-Path $BinDir ("rpi.new.$PID.$([Guid]::NewGuid().ToString('N')).exe")
    try {
        Copy-Item -LiteralPath $Binary.FullName -Destination $Staged -Force -ErrorAction Stop
    } catch {
        Fail "could not stage downloaded rpi for activation: $($_.Exception.Message)"
    }
    # Re-verify the staged copy in its final directory before activation.
    $StagedSmokeFailure = Get-CandidateIdentityFailure $Staged $ResolvedVersion
    if ($StagedSmokeFailure) {
        Remove-Item -LiteralPath $Staged -Force -ErrorAction SilentlyContinue
        Fail "staged binary $StagedSmokeFailure; existing install left untouched"
    }

    $HadPrior = Test-Path -LiteralPath $Dest
    # Pre-emptive copy of the prior bytes so rollback can restore them after an
    # atomic replace (which discards the old inode). This copy leaves Dest
    # intact, so there is no missing-path window while it is taken.
    $Backup = "$Dest.bak.$PID.$([Guid]::NewGuid().ToString('N'))"
    if ($HadPrior) {
        try {
            Copy-Item -LiteralPath $Dest -Destination $Backup -Force -ErrorAction Stop
        } catch {
            Remove-Item -LiteralPath $Staged -Force -ErrorAction SilentlyContinue
            Fail "could not back up the existing rpi executable: $($_.Exception.Message)"
        }
    }

    # Atomic activation. MoveFileEx with MOVEFILE_REPLACE_EXISTING renames the
    # staged file over Dest in one syscall — Dest is never absent, so concurrent
    # `rpi` launches never see a missing executable. A running rpi.exe holds Dest
    # and makes the replace fail with a sharing/access error; rather than perform
    # a rename-aside that would open a missing-path window, we refuse outright so
    # the user closes rpi and retries. This keeps activation strictly window-free.
    $MoveFileOk = [PiInstall.Native]::MoveFileEx($Staged, $Dest, $MOVEFILE_REPLACE_EXISTING)
    if (-not $MoveFileOk) {
        $Win32Err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        Remove-Item -LiteralPath $Staged -Force -ErrorAction SilentlyContinue
        if ($HadPrior) { Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue }
        if ($Win32Err -eq $ERR_SHARING_VIOLATION -or $Win32Err -eq $ERR_ACCESS_DENIED) {
            Fail "cannot replace $Dest (close all running rpi sessions and retry)"
        } else {
            Fail "cannot install to ${Dest} (MoveFileEx error $Win32Err)"
        }
    }

    # Verify the activated path still exposes the exact candidate identity;
    # restore prior bytes on any execution or identity failure.
    $ActiveSmokeFailure = Get-CandidateIdentityFailure $Dest $ResolvedVersion
    if ($ActiveSmokeFailure) {
        $RollbackErrors = [System.Collections.Generic.List[string]]::new()
        if ($HadPrior -and $Backup) {
            # Atomic-replace path: restore the pre-emptive backup atomically.
            $RestoreOk = [PiInstall.Native]::MoveFileEx($Backup, $Dest, $MOVEFILE_REPLACE_EXISTING)
            if (-not $RestoreOk) {
                Add-RollbackError $RollbackErrors "could not restore prior rpi executable (MoveFileEx error $([System.Runtime.InteropServices.Marshal]::GetLastWin32Error()))"
            } else {
                $Backup = $null
            }
        } else {
            # Fresh install with no prior binary: remove the new (failed)
            # executable so the system returns to its pre-install state.
            Remove-RollbackPath $Dest "new rpi executable" $RollbackErrors
            Confirm-RollbackPath $Dest "new rpi executable" $false $RollbackErrors
        }
        if ($Backup) { Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue }
        Fail-WithRollbackErrors "installed binary $ActiveSmokeFailure" $RollbackErrors
    }

    # Commit updater state only after the binary activates.
    $StateAside = "$StatePath.old.$PID.$([Guid]::NewGuid().ToString('N'))"
    $HadState = Test-Path -LiteralPath $StatePath
    $StateMovedAside = $false
    try {
        if ($HadState) {
            Move-Item -LiteralPath $StatePath -Destination $StateAside
            $StateMovedAside = $true
        }
        Move-Item -LiteralPath $StateTmp -Destination $StatePath
        $StateTmp = $null
    } catch {
        $StateCommitError = $_.Exception.Message
        $RollbackErrors = [System.Collections.Generic.List[string]]::new()
        if ($HadState) {
            $StateAsideExists = $StateMovedAside
            if (-not $StateAsideExists) {
                try {
                    $StateAsideExists = Test-Path -LiteralPath $StateAside -PathType Leaf -ErrorAction Stop
                } catch {
                    Add-RollbackError $RollbackErrors "could not inspect updater state aside at ${StateAside}: $($_.Exception.Message)"
                }
            }
            if ($StateAsideExists) {
                Restore-RollbackAside $StatePath $StateAside $true "updater state" $RollbackErrors
            } else {
                Confirm-RollbackPath $StatePath "untouched updater state" $true $RollbackErrors
                Confirm-RollbackPath $StateAside "updater state aside" $false $RollbackErrors
            }
        } else {
            Restore-RollbackAside $StatePath $StateAside $false "new updater state" $RollbackErrors
        }
        if ($HadPrior -and $Backup) {
            $RestoreOk = [PiInstall.Native]::MoveFileEx($Backup, $Dest, $MOVEFILE_REPLACE_EXISTING)
            if (-not $RestoreOk) {
                Add-RollbackError $RollbackErrors "could not restore prior rpi executable (MoveFileEx error $([System.Runtime.InteropServices.Marshal]::GetLastWin32Error()))"
            } else {
                $Backup = $null
            }
        } else {
            Remove-RollbackPath $Dest "new rpi executable" $RollbackErrors
            Confirm-RollbackPath $Dest "new rpi executable" $false $RollbackErrors
        }
        Fail-WithRollbackErrors "cannot record rpi update state: $StateCommitError" $RollbackErrors
    }
    if ($HadState -and (Test-Path -LiteralPath $StateAside)) {
        Remove-Item -LiteralPath $StateAside -Force -ErrorAction SilentlyContinue
    }

    if ($Backup -and (Test-Path -LiteralPath $Backup)) {
        Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
    }

    # Clean-cutover migration: remove only the unchanged legacy executable that
    # the prior state proved was installer-managed. Unmanaged paths are kept.
    if ($LegacyPiSha256 -and (Test-Path -LiteralPath $LegacyPiPath -PathType Leaf)) {
        try {
            $LegacyItem = Get-Item -LiteralPath $LegacyPiPath -Force
            $CurrentLegacySha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $LegacyPiPath).Hash.ToLowerInvariant()
            if (($LegacyItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0 -and
                -not $LegacyItem.PSIsContainer -and
                $CurrentLegacySha256 -eq $LegacyPiSha256) {
                Remove-Item -LiteralPath $LegacyPiPath -Force -ErrorAction Stop
                if (Test-Path -LiteralPath $LegacyPiPath) {
                    throw "path still exists after removal"
                }
                Write-Host "Removed legacy installer-managed command $LegacyPiPath"
            } else {
                Write-Warning "Legacy pi path changed during install; leaving it untouched: $LegacyPiPath"
            }
        } catch {
            Write-Warning "rpi installed, but legacy managed command could not be removed: ${LegacyPiPath}: $($_.Exception.Message)"
        }
    }

    Write-Host ""
    Write-Host "rpi v$ResolvedVersion installed to $Dest"

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $OnPath = (($UserPath -split ";") -contains $BinDir) -or
              (($env:Path -split ";") -contains $BinDir)
    if (-not $OnPath) {
        $NewUserPath = if ($UserPath) { "$BinDir;$UserPath" } else { $BinDir }
        try {
            [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
            Write-Host ""
            Write-Host "Added $BinDir to your user PATH."
            Write-Host "Open a new terminal, then run 'rpi' to get started."
        } catch {
            Write-Warning "Could not add $BinDir to your user PATH: $($_.Exception.Message)"
            Write-Host "Add it manually: [Environment]::SetEnvironmentVariable('Path', '$BinDir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        }
    } else {
        Write-Host "Run 'rpi' to get started."
    }
} finally {
    if ($StateTmp -and (Test-Path -LiteralPath $StateTmp)) {
        Remove-Item -LiteralPath $StateTmp -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
    if ($InstallMutex) {
        if ($InstallMutexAcquired) {
            try { $InstallMutex.ReleaseMutex() } catch { }
        }
        try { $InstallMutex.Dispose() } catch { }
    }
}